//! Bounded diagnostic attachments for an explicit support upload.

use serde::Serialize;
use serde_json::{json, Map, Value};
use std::{
    collections::VecDeque,
    fs::{self, File},
    io::Read,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::SystemTime,
};

const MAX_LOG_BYTES: usize = 128 * 1024;
const READ_ERROR: &str = "Could not read recent Orange logs. Please try again.";

#[derive(Debug, Serialize)]
pub(super) struct LogAttachment {
    pub(super) name: String,
    pub(super) contents: String,
    pub(super) truncated: bool,
}

pub(super) fn collect(directory: &Path, cancel: &AtomicBool) -> Result<Vec<LogAttachment>, String> {
    check_cancel(cancel)?;
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(READ_ERROR.into()),
    };
    let mut files = Vec::new();
    let mut unreadable = false;
    for entry in entries.take(256) {
        check_cancel(cancel)?;
        let Ok(entry) = entry else {
            unreadable = true;
            continue;
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !super::history::valid_log_name(name) {
            continue;
        }
        match entry.file_type() {
            Ok(kind) if kind.is_file() => {}
            Err(_) => {
                unreadable = true;
                continue;
            }
            _ => continue,
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        files.push((modified, name.to_string(), entry.path()));
    }
    files.sort_by_key(|file| std::cmp::Reverse(file.0));
    let mut recent = Vec::new();
    for (_, name, path) in files.into_iter().take(64) {
        check_cancel(cancel)?;
        let Ok((bytes, mut truncated)) = super::history::read_tail(&path) else {
            unreadable = true;
            continue;
        };
        let header = if truncated {
            session_header(&path).unwrap_or_else(|_| {
                unreadable = true;
                String::new()
            })
        } else {
            String::new()
        };
        let mut lines = VecDeque::new();
        let mut size = 0;
        let mut newest = 0;
        for line in bytes.split(|&b| b == b'\n').filter(|line| !line.is_empty()) {
            check_cancel(cancel)?;
            let record = serde_json::from_slice::<Value>(line)
                .ok()
                .and_then(|v| sanitize_record(&v));
            let Some(record) = record else {
                truncated = true;
                continue;
            };
            let text = format!("{record}\n");
            if text.len() + header.len() > MAX_LOG_BYTES {
                truncated = true;
                continue;
            }
            newest = newest.max(record["at_unix_ms"].as_u64().unwrap_or_default());
            if text == header {
                continue;
            }
            size += text.len();
            lines.push_back(text);
            while size + header.len() > MAX_LOG_BYTES {
                if let Some(oldest) = lines.pop_front() {
                    size -= oldest.len();
                }
                truncated = true;
            }
        }
        if !lines.is_empty() || !header.is_empty() {
            recent.push((
                newest,
                LogAttachment {
                    name,
                    contents: header + &lines.into_iter().collect::<String>(),
                    truncated,
                },
            ));
            // Keep only three tails in memory even when scanning many sessions.
            recent.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
            recent.truncate(3);
        }
    }
    check_cancel(cancel)?;
    if recent.is_empty() && unreadable {
        return Err(READ_ERROR.into());
    }
    Ok(recent.into_iter().map(|(_, log)| log).collect())
}

fn session_header(path: &Path) -> Result<String, String> {
    // The correlation ID is written once near the beginning, so a tail from
    // a long-running host would otherwise be impossible to pair with its viewer.
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| READ_ERROR)?
        .take(4096)
        .read_to_end(&mut bytes)
        .map_err(|_| READ_ERROR)?;
    for line in bytes.split(|&b| b == b'\n') {
        if let Ok(record) = serde_json::from_slice::<Value>(line) {
            if record["event"] == "diagnostic-session" {
                if let Some(record) = sanitize_record(&record) {
                    return Ok(format!("{record}\n"));
                }
            }
        }
    }
    Ok(String::new())
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err("Report sending was cancelled.".into())
    } else {
        Ok(())
    }
}

fn hex(value: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn sanitize_record(record: &Value) -> Option<Value> {
    let event = record["event"].as_str()?;
    if !matches!(
        event,
        "diagnostic-session"
            | "connection-stage"
            | "ice-state"
            | "ice-gathering-state"
            | "peer-connection-state"
            | "media-progress"
            | "webrtc-stats"
            | "encoder-gop"
            | "playout-correction"
            | "pipeline-eos"
            | "pipeline-latency-recalculated"
            | "operation-started"
            | "operation-finished"
            | "pad-added"
            | "receive-branch-ready"
            | "ice-candidate"
            | "ice-route"
            | "ice-runtime"
    ) {
        return None;
    }
    let role = record["role"].as_str()?;
    if !matches!(role, "watch" | "host" | "send" | "recv" | "loopback")
        && !role.strip_prefix("host-viewer-").is_some_and(|id| {
            !id.is_empty() && id.len() <= 10 && id.bytes().all(|b| b.is_ascii_digit())
        })
    {
        return None;
    }
    let at = record["at_unix_ms"].as_u64()?;
    let build = record["build"]
        .as_str()
        .filter(|s| hex(s, 7, 40))
        .unwrap_or("unknown");
    let payload = if event == "diagnostic-session" {
        // This opaque correlation ID is not an authentication token; retaining
        // it lets support pair host and viewer logs from the same failed join.
        let id = record["payload"]["id"]
            .as_str()
            .filter(|s| hex(s, 32, 32))?;
        json!({"id":id})
    } else if matches!(
        event,
        "ice-state" | "ice-gathering-state" | "peer-connection-state"
    ) {
        let state = record["payload"].as_str()?;
        if !matches!(
            state,
            "New"
                | "Checking"
                | "Connected"
                | "Completed"
                | "Disconnected"
                | "Failed"
                | "Closed"
                | "Gathering"
                | "Complete"
                | "Connecting"
        ) {
            return None;
        }
        json!(state)
    } else if matches!(event, "ice-candidate" | "ice-route" | "ice-runtime") {
        network_payload(event, &record["payload"])?
    } else {
        clean_payload(&record["payload"], "", 0)?
    };
    let mut safe =
        json!({"at_unix_ms":at,"event":event,"role":role,"build":build,"payload":payload});
    if let Some(elapsed) = record["elapsed_ms"].as_u64() {
        safe["elapsed_ms"] = elapsed.into();
    }
    Some(safe)
}

fn network_token<'a>(value: &'a Value, key: &str, allowed: &[&str]) -> Option<&'a str> {
    value[key].as_str().filter(|text| allowed.contains(text))
}

fn network_descriptor(value: &Value) -> Option<Value> {
    Some(json!({
        "kind":network_token(value, "kind", &["host", "srflx", "prflx", "relay", "unknown"] )?,
        "transport":network_token(value, "transport", &["udp", "tcp", "unknown"] )?,
        "family":network_token(value, "family", &["ipv4", "ipv6", "mdns", "unknown"] )?,
        "scope":network_token(value, "scope", &["public", "private", "shared", "link-local", "loopback", "unknown"] )?,
    }))
}

fn network_payload(event: &str, value: &Value) -> Option<Value> {
    // These records describe candidate addresses without exporting them. Use
    // finite values and typed numbers, not the general nested-field filter.
    match event {
        "ice-candidate" => {
            let mut safe = network_descriptor(value)?;
            safe["direction"] = network_token(value, "direction", &["local", "remote"])?.into();
            safe["action"] = network_token(
                value,
                "action",
                &[
                    "gathered",
                    "signalled",
                    "signalling-closed",
                    "submitted",
                    "submission-completed",
                    "submission-failed",
                    "end-of-candidates",
                    "invalid",
                ],
            )?
            .into();
            safe["mline"] = u32::try_from(value["mline"].as_u64()?).ok()?.into();
            Some(safe)
        }
        "ice-route" => {
            let selected = value["selected"].as_bool()?;
            if selected {
                Some(
                    json!({"selected":true, "local":network_descriptor(&value["local"] )?, "remote":network_descriptor(&value["remote"] )?}),
                )
            } else {
                Some(json!({"selected":false}))
            }
        }
        "ice-runtime" => {
            let mut safe = Map::new();
            for key in ["gst_major", "gst_minor", "gst_micro", "gst_nano"] {
                safe.insert(key.into(), u32::try_from(value[key].as_u64()?).ok()?.into());
            }
            Some(Value::Object(safe))
        }
        _ => None,
    }
}

// Export only known diagnostic fields. A blacklist would miss a newly-added
// credential, raw SDP, URL or address field and upload it without review.
fn clean_payload(value: &Value, key: &str, depth: usize) -> Option<Value> {
    if depth > 8 {
        return None;
    }
    match value {
        Value::Object(object) => {
            let mut safe = Map::new();
            for (key, value) in object {
                if matches!(
                    key.as_str(),
                    "stage"
                        | "event"
                        | "encoding"
                        | "operation"
                        | "element"
                        | "factory"
                        | "elapsed_ms"
                        | "previous_stage_ms"
                        | "duration_ms"
                        | "success"
                        | "ui_responsive"
                        | "progress"
                        | "rtp"
                        | "depay"
                        | "parsed"
                        | "decoded"
                        | "video_sink_input"
                        | "audio_rtp"
                        | "audio_depay"
                        | "audio_decoded"
                        | "audio_sink_input"
                        | "buffers"
                        | "bytes"
                        | "silent_ms"
                        | "pts_ms"
                        | "av_offset_ms"
                        | "keyframes"
                        | "queue_overruns"
                        | "inbound_video"
                        | "outbound_video"
                        | "inbound_audio"
                        | "outbound_audio"
                        | "unclassified_streams"
                        | "packets_received"
                        | "payload_bytes_received"
                        | "packets_lost"
                        | "packets_repaired"
                        | "packets_discarded"
                        | "packets_duplicated"
                        | "jitter_ms"
                        | "nack_sent"
                        | "pli_sent"
                        | "fir_sent"
                        | "rtx_requested"
                        | "rtx_succeeded"
                        | "packets_late"
                        | "jitterbuffer_packets_pushed"
                        | "jitterbuffer_packets_lost"
                        | "jitterbuffer_packets_duplicated"
                        | "jitterbuffer_avg_jitter_ms"
                        | "jitterbuffer_rtx_per_packet"
                        | "jitterbuffer_rtx_rtt_ms"
                        | "packets_sent"
                        | "payload_bytes_sent"
                        | "nack_received"
                        | "pli_received"
                        | "fir_received"
                        | "sampled_frames"
                        | "sampled_ms"
                        | "measured_gop_size"
                        | "active_gop_size"
                        | "offset_ms"
                        | "audio"
                        | "observed_running_ms"
                        | "buffer_running_ms"
                        | "sink_latency_ms"
                ) {
                    if let Some(value) = clean_payload(value, key, depth + 1) {
                        safe.insert(key.clone(), value);
                    }
                }
            }
            Some(Value::Object(safe))
        }
        Value::String(text) => {
            let accepted = match key {
                "stage" => matches!(
                    text.as_str(),
                    "joining-room"
                        | "exchanging-stream-details"
                        | "finding-direct-route"
                        | "securing-connection"
                        | "starting-video-and-audio"
                        | "connected"
                        | "failed-signaling"
                        | "failed-room"
                        | "failed-negotiation"
                        | "failed-network"
                        | "failed-playback"
                ),
                "event" => matches!(
                    text.as_str(),
                    "watch-started"
                        | "stream-info"
                        | "sdp-offer-received"
                        | "sdp-offer-installed"
                        | "sdp-answer-installed"
                        | "ice-gathering"
                        | "ice-checking"
                        | "ice-connected"
                        | "peer-connected"
                        | "pad-added"
                        | "receive-branch-ready"
                        | "first-video-frame"
                        | "signaling-failed"
                        | "room-failed"
                        | "negotiation-failed"
                        | "network-failed"
                        | "playback-failed"
                ),
                "encoding" => matches!(text.as_str(), "H264" | "H265" | "AV1" | "OPUS"),
                "operation" | "element" | "factory" => {
                    !text.is_empty()
                        && text.len() <= 80
                        && text
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                }
                _ => false,
            };
            accepted.then(|| value.clone())
        }
        Value::Number(_) | Value::Bool(_) | Value::Null => Some(value.clone()),
        Value::Array(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::fs;

    fn record(at: u64, event: &str, payload: Value) -> Value {
        json!({"at_unix_ms":at,"elapsed_ms":1,"event":event,"role":"watch","build":"01d2ce9","payload":payload})
    }

    fn write(dir: &Path, name: &str, value: &Value) {
        fs::write(dir.join(name), format!("{value}\n")).unwrap();
    }

    #[test]
    fn support_uploads_keep_candidate_lifecycle_and_selected_route_without_addresses() {
        // The first real support report showed failed ICE but discarded the
        // evidence needed to distinguish gathering from submission problems.
        let dir = tempfile::tempdir().unwrap();
        let records = [
            record(
                1,
                "ice-candidate",
                json!({
                    "direction":"remote", "action":"submission-completed", "kind":"srflx",
                    "family":"ipv4", "transport":"udp", "scope":"shared", "mline":0,
                    "address":"198.51.100.4", "candidate":"secret-candidate", "ufrag":"secret-ufrag"
                }),
            ),
            record(
                2,
                "ice-route",
                json!({"selected":true,
                    "local":{"kind":"host","transport":"udp","family":"ipv6","scope":"public","address":"2001:db8::1"},
                    "remote":{"kind":"host","transport":"udp","family":"ipv6","scope":"public","port":4242},
                    "selected_pair_id":"secret-pair"
                }),
            ),
            record(
                3,
                "ice-runtime",
                json!({"gst_major":1,"gst_minor":28,"gst_micro":6,"gst_nano":0,"path":"secret-path"}),
            ),
        ];
        fs::write(
            dir.path().join("orange-media-42.jsonl"),
            records.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
        let logs = collect(dir.path(), &AtomicBool::new(false)).unwrap();
        assert_eq!(logs.len(), 1, "network evidence must be attached");
        let rows: Vec<Value> = logs[0]
            .contents
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["payload"]["action"], "submission-completed");
        assert_eq!(rows[0]["payload"]["scope"], "shared");
        assert_eq!(rows[1]["payload"]["remote"]["family"], "ipv6");
        assert_eq!(rows[2]["payload"]["gst_micro"], 6);
        for private in ["secret-", "198.51.100.4", "2001:db8::1", "4242"] {
            assert!(!logs[0].contents.contains(private));
        }
    }

    #[test]
    fn network_report_fields_require_known_values_and_numeric_version_fields() {
        // A field allowlist alone still leaks arbitrary strings under a known
        // key. Types and finite enum values must survive the same boundary.
        let invalid = [
            record(
                1,
                "ice-candidate",
                json!({"direction":"remote","action":"secret-token","kind":"host","transport":"udp","family":"ipv4","scope":"private","mline":0}),
            ),
            record(2, "ice-route", json!({"selected":"secret-token"})),
            record(
                3,
                "ice-runtime",
                json!({"gst_major":"secret-token","gst_minor":28,"gst_micro":6,"gst_nano":0}),
            ),
        ];
        for record in invalid {
            assert!(sanitize_record(&record).is_none());
        }
        let safe = sanitize_record(&record(
            4,
            "ice-route",
            json!({"selected":false,"error":"secret-error"}),
        ))
        .unwrap();
        assert_eq!(safe["payload"], json!({"selected":false}));
    }

    #[test]
    fn uploads_keep_failure_evidence_and_strip_credentials_and_network_addresses() {
        // Sending the logs must not bypass the existing report privacy boundary.
        let dir = tempfile::tempdir().unwrap();
        let mut value = record(
            1,
            "connection-stage",
            json!({
                "stage":"failed-network", "event":"network-failed", "elapsed_ms":10100,
                "token":"private-token", "candidate":"203.0.113.8", "sdp":"private-sdp"
            }),
        );
        value["device_id"] = "private-device".into();
        value["authorization"] = "private-auth".into();
        write(dir.path(), "orange-media-123.jsonl", &value);
        let attachments = collect(dir.path(), &AtomicBool::new(false)).unwrap();
        assert_eq!(attachments.len(), 1);
        let text = &attachments[0].contents;
        assert!(text.contains("failed-network"));
        assert!(text.contains("10100"));
        assert!(!text.contains("private"));
        assert!(!text.contains("203.0.113"));
    }

    #[test]
    fn attachments_keep_timing_loss_and_session_correlation_for_support() {
        // Summaries alone lost the shared host/viewer session ID and the RTP
        // counters needed to distinguish connection failure from playback failure.
        let dir = tempfile::tempdir().unwrap();
        let id = "0123456789abcdef0123456789abcdef";
        let records = [
            record(1, "diagnostic-session", json!({"id":id})),
            record(
                2,
                "webrtc-stats",
                json!({"inbound_video":{"packets_received":4,"packets_lost":-1,"jitter_ms":2.5}}),
            ),
            record(
                3,
                "media-progress",
                json!({"ui_responsive":true,"progress":{"rtp":{"buffers":4,"bytes":900,"pts_ms":123,"silent_ms":null}}}),
            ),
        ];
        fs::write(
            dir.path().join("orange-media-11.jsonl"),
            records.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
        let logs = collect(dir.path(), &AtomicBool::new(false)).unwrap();
        assert!(
            !logs[0].truncated,
            "recognized complete logs must remain marked complete"
        );
        let text = &logs[0].contents;
        assert!(text.contains(id));
        assert!(text.contains("\"packets_lost\":-1"));
        assert!(text.contains("\"pts_ms\":123"));
        assert!(text.contains("\"jitter_ms\":2.5"));
    }

    #[test]
    fn attachments_choose_record_time_and_never_read_unrelated_files() {
        // NTFS directory timestamps lag open writers. Ranking the evidence
        // inside each tail keeps a long-running host among recent attachments.
        let dir = tempfile::tempdir().unwrap();
        for (pid, at) in [(1, 500), (2, 100), (3, 300), (4, 200)] {
            write(
                dir.path(),
                &format!("orange-media-{pid}.jsonl"),
                &record(at, "ice-state", json!("Failed")),
            );
        }
        write(
            dir.path(),
            "session.json",
            &record(999, "ice-state", json!("Failed")),
        );
        write(
            dir.path(),
            "orange-media-secret.jsonl",
            &record(999, "ice-state", json!("Failed")),
        );
        let logs = collect(dir.path(), &AtomicBool::new(false)).unwrap();
        let names: Vec<_> = logs.iter().map(|log| log.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "orange-media-1.jsonl",
                "orange-media-3.jsonl",
                "orange-media-4.jsonl"
            ]
        );
    }

    #[test]
    fn oversized_or_incomplete_logs_produce_valid_bounded_jsonl_tails() {
        // A running log may end mid-record, and a large log starts mid-record
        // when read as a tail. Neither fragment should become an attachment row.
        let dir = tempfile::tempdir().unwrap();
        let session = record(
            1,
            "diagnostic-session",
            json!({"id":"0123456789abcdef0123456789abcdef"}),
        );
        let mut text = format!("{session}\n{}", "x".repeat(300_000));
        text.push('\n');
        text.push_str(&record(4, "ice-state", json!("Failed")).to_string());
        text.push_str("\n{\"incomplete\":");
        fs::write(dir.path().join("orange-media-5.jsonl"), text).unwrap();
        let logs = collect(dir.path(), &AtomicBool::new(false)).unwrap();
        assert_eq!(logs.len(), 1);
        assert!(logs[0].truncated);
        assert!(logs[0].contents.len() <= 128 * 1024);
        let rows: Vec<Value> = logs[0]
            .contents
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["payload"]["id"], "0123456789abcdef0123456789abcdef");
        assert_eq!(rows[1]["payload"], "Failed");
    }

    #[test]
    fn cancelling_collection_does_not_return_a_partial_upload() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "orange-media-1.jsonl",
            &record(1, "ice-state", json!("Failed")),
        );
        assert!(collect(dir.path(), &AtomicBool::new(true)).is_err());
    }

    #[test]
    fn users_without_session_logs_can_send_the_check_report() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            collect(&dir.path().join("missing"), &AtomicBool::new(false))
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(windows)]
    #[test]
    fn an_unreadable_old_log_does_not_discard_readable_recent_evidence() {
        use std::os::windows::fs::OpenOptionsExt;
        // Another process can deny reads of one file during rotation or scan.
        // That must not discard all the other readable logs in this upload.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "orange-media-1.jsonl",
            &record(1, "ice-state", json!("Failed")),
        );
        let _locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(dir.path().join("orange-media-1.jsonl"))
            .unwrap();
        write(
            dir.path(),
            "orange-media-2.jsonl",
            &record(2, "ice-state", json!("Failed")),
        );
        let logs = collect(dir.path(), &AtomicBool::new(false)).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].name, "orange-media-2.jsonl");
        assert!(logs[0].contents.contains("Failed"));
    }
}
