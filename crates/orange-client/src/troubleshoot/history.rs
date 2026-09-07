//! Bounded, privacy-safe summaries of observed session outcomes.

use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::SystemTime,
};

const TAIL_BYTES: u64 = 128 * 1024;
const MAX_FILES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecentConnectionOutcome {
    Connected,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RecentConnection {
    pub(super) at_unix_ms: u64,
    pub(super) outcome: RecentConnectionOutcome,
}

impl RecentConnection {
    pub(super) fn friendly_age(&self, now_unix_ms: u128) -> String {
        if now_unix_ms < u128::from(self.at_unix_ms) {
            return "recorded with a future clock".into();
        }
        let seconds = (now_unix_ms - u128::from(self.at_unix_ms)) / 1000;
        match seconds {
            0..60 => "less than a minute ago".into(),
            60..3600 => format!("{} min ago", seconds / 60),
            3600..86400 => format!("{} h ago", seconds / 3600),
            _ => format!("{} days ago", seconds / 86400),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Summary {
    pub(super) lines: Vec<String>,
    pub(super) latest: Option<RecentConnection>,
}

#[derive(Default)]
struct Observation {
    at: u64,
    build: String,
    watching: bool,
    ice_failed: bool,
    peer_failed: bool,
    peer_connected: bool,
    video_rtp: bool,
    failure: Option<&'static str>,
}

impl Observation {
    fn description(&self) -> &'static str {
        if self.ice_failed {
            "ICE connectivity checks failed. No working direct route was available at the failure. This does not identify a firewall, NAT, or candidate-exchange fault. Retry without a VPN or on another network and collect both peers' logs from the same attempt."
        } else if let Some(failure) = self.failure {
            failure
        } else if self.peer_failed {
            "Peer connection failed. The log does not establish a more specific cause. Collect both peers' logs from the same attempt."
        } else if self.video_rtp && self.watching {
            "Video RTP arrived; physical playback is not verified. If the picture stayed black, collect both peers' logs to check frame assembly, decoding and presentation."
        } else if self.peer_connected {
            "Peer connected; media delivery is not confirmed by this summary. Try a stream with the intended friend to verify picture and sound."
        } else {
            "No terminal connection outcome in the inspected log tail. The attempt may still be running, may have been closed, or may be incomplete. Reproduce the issue and run Troubleshoot again."
        }
    }

    fn outcome(&self) -> RecentConnectionOutcome {
        if self.ice_failed || self.peer_failed || self.failure.is_some() {
            RecentConnectionOutcome::Failed
        } else if self.video_rtp || self.peer_connected {
            RecentConnectionOutcome::Connected
        } else {
            RecentConnectionOutcome::Unknown
        }
    }
}

/// Historical evidence never contributes a current readiness verdict.
pub(super) fn summarize(directory: &Path, cancelled: &AtomicBool) -> Summary {
    if cancelled.load(Ordering::Acquire) {
        return Summary {
            lines: Vec::new(),
            latest: None,
        };
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Summary {
                lines: vec![
                    "No session logs yet. Try a stream, then run Troubleshoot again.".into(),
                ],
                latest: None,
            };
        }
        Err(_) => {
            return Summary {
                lines: vec![
                    "Session logs could not be read. Use Diagnostics > Open folder to check access."
                        .into(),
                ],
                latest: None,
            };
        }
    };
    let mut files = Vec::new();
    let mut limited = false;
    let mut unreadable = false;
    for (index, entry) in entries.take(257).enumerate() {
        if cancelled.load(Ordering::Acquire) {
            return Summary {
                lines: Vec::new(),
                latest: None,
            };
        }
        if index == 256 {
            limited = true;
            break;
        }
        let Ok(entry) = entry else {
            unreadable = true;
            continue;
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !valid_log_name(name) {
            continue;
        }
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        files.push((modified, entry.path()));
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    limited |= files.len() > MAX_FILES;
    let mut observations = Vec::new();
    for (_, path) in files.into_iter().take(MAX_FILES) {
        if cancelled.load(Ordering::Acquire) {
            return Summary {
                lines: Vec::new(),
                latest: None,
            };
        }
        match read_tail(&path) {
            Ok((bytes, _)) => observations.extend(observe(&bytes, cancelled)),
            Err(_) => unreadable = true,
        }
    }
    if cancelled.load(Ordering::Acquire) {
        return Summary {
            lines: Vec::new(),
            latest: None,
        };
    }
    observations.sort_by_key(|observation| std::cmp::Reverse(observation.at));
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let latest = observations.first().map(|observation| RecentConnection {
        at_unix_ms: observation.at,
        outcome: observation.outcome(),
    });
    let mut lines: Vec<_> = observations
        .into_iter()
        .take(3)
        .map(|o| {
            let role = if o.watching {
                "Viewer"
            } else {
                "Host connection"
            };
            let age = RecentConnection {
                at_unix_ms: o.at,
                outcome: o.outcome(),
            }
            .friendly_age(now_ms);
            format!(
                "{role} — {age}; event Unix ms {}; build {}: {}",
                o.at,
                o.build,
                o.description()
            )
        })
        .collect();
    if lines.is_empty() {
        lines.push("No usable connection evidence in recent log tails. Try a stream, then run Troubleshoot again.".into());
    }
    if unreadable {
        lines.push("Some session logs could not be read; this history is incomplete.".into());
    }
    if limited {
        lines.push(
            "The log scan reached its file limit; older or additional sessions may be omitted."
                .into(),
        );
    }
    Summary { lines, latest }
}

pub(super) fn valid_log_name(name: &str) -> bool {
    name.strip_prefix("orange-media-")
        .and_then(|s| s.strip_suffix(".jsonl"))
        .is_some_and(|pid| {
            !pid.starts_with('0')
                && pid.len() <= 10
                && pid.bytes().all(|b| b.is_ascii_digit())
                && pid.parse::<u32>().is_ok_and(|value| value > 0)
        })
}

pub(super) fn read_tail(path: &Path) -> std::io::Result<(Vec<u8>, bool)> {
    let mut file = File::open(path)?;
    // Open NTFS writers can report zero directory-entry bytes despite flushed
    // records. Query the actual stream end rather than skipping by metadata.
    let end = file.seek(SeekFrom::End(0))?;
    let start = end.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes)?;
    if start != 0 {
        let skip = bytes
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        bytes.drain(..skip);
    }
    Ok((bytes, start != 0))
}

fn observe(bytes: &[u8], cancelled: &AtomicBool) -> Vec<Observation> {
    let mut roles = BTreeMap::<String, Observation>::new();
    for line in bytes.split(|&b| b == b'\n') {
        if cancelled.load(Ordering::Acquire) {
            return Vec::new();
        }
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(role) = record["role"].as_str() else {
            continue;
        };
        let watching = role == "watch";
        let host = role.strip_prefix("host-viewer-").is_some_and(|id| {
            !id.is_empty() && id.len() <= 10 && id.bytes().all(|b| b.is_ascii_digit())
        });
        if !watching && !host {
            continue;
        }
        let Some(at) = record["at_unix_ms"].as_u64() else {
            continue;
        };
        if roles.len() >= 32 && !roles.contains_key(role) {
            continue;
        }
        let o = roles.entry(role.into()).or_default();
        let event = record["event"].as_str().unwrap_or_default();
        let payload = &record["payload"];
        // A reused PID/log or a fresh negotiation must not inherit an older
        // terminal failure. The writer normally truncates each process log.
        if (event == "connection-stage" && payload["event"] == "watch-started")
            || (event == "ice-state" && payload == "Checking")
        {
            *o = Observation::default();
        }
        o.at = o.at.max(at);
        o.watching = watching;
        o.build = record["build"]
            .as_str()
            .filter(|s| (7..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .unwrap_or("unknown")
            .into();
        match event {
            "ice-state" if payload == "Failed" => o.ice_failed = true,
            "peer-connection-state" if payload == "Failed" => o.peer_failed = true,
            "peer-connection-state" if payload == "Connected" => o.peer_connected = true,
            "media-progress" => {
                o.video_rtp |= payload["progress"]["rtp"]["buffers"].as_u64().is_some_and(|n| n > 0);
            }
            "connection-stage" => match payload["stage"].as_str() {
                Some("failed-signaling") => o.failure = Some("Signalling failed during the session. Check the current signalling result and retry; collect both peers' logs if it repeats."),
                Some("failed-room") => o.failure = Some("Joining the room failed. Ask the host to confirm the stream is still running and has room for another viewer, then retry."),
                Some("failed-negotiation") => o.failure = Some("Stream negotiation failed. Update both peers and collect their logs from the same attempt."),
                Some("failed-playback") => o.failure = Some("Playback setup or delivery failed. Check media capabilities and GPU/audio drivers, then collect both peers' logs if it repeats."),
                Some("failed-network") => o.peer_failed = true,
                _ => {}
            },
            _ => {}
        }
    }
    roles.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // The reported ICE failure must survive the generic peer Failed callback
    // that follows it, without inventing a firewall or NAT diagnosis.
    #[test]
    fn an_ice_failure_remains_the_explanation_after_the_peer_also_fails() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("orange-media-123.jsonl"), concat!(
            "{\"at_unix_ms\":1000,\"build\":\"20ba4ee\",\"role\":\"watch\",\"event\":\"ice-state\",\"payload\":\"Checking\"}\n",
            "{\"at_unix_ms\":11000,\"build\":\"20ba4ee\",\"role\":\"watch\",\"event\":\"ice-state\",\"payload\":\"Failed\"}\n",
            "{\"at_unix_ms\":11002,\"build\":\"20ba4ee\",\"role\":\"watch\",\"event\":\"peer-connection-state\",\"payload\":\"Failed\"}\n"
        )).unwrap();
        let result = summarize(dir.path(), &AtomicBool::new(false));
        assert_eq!(
            result.latest.as_ref().map(|latest| latest.outcome),
            Some(RecentConnectionOutcome::Failed)
        );
        let result = result.lines.join("\n");
        assert!(
            result.contains("ICE connectivity checks failed"),
            "{result}"
        );
        assert!(result.contains("20ba4ee"));
        assert!(result.contains("11002"));
        assert!(!result.contains("firewall blocked"));
    }

    // Session metadata, arbitrary errors and raw network data must never be
    // copied into a support report, even when malformed logs contain them.
    #[test]
    fn incomplete_logs_do_not_leak_raw_payloads_or_claim_readiness() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("orange-media-44.jsonl"), concat!(
            "{\"at_unix_ms\":12000,\"build\":\"secret-build-token\",\"role\":\"watch\",\"event\":\"peer-connection-state\",\"payload\":\"Connected\"}\n",
            "{\"at_unix_ms\":12001,\"build\":\"secret-build-token\",\"role\":\"watch\",\"event\":\"arbitrary\",\"payload\":\"secret-payload 203.0.113.42 room-secret\"}\n",
            "{\"incomplete\":"
        )).unwrap();
        let result = summarize(dir.path(), &AtomicBool::new(false));
        assert_eq!(
            result.latest.as_ref().map(|latest| latest.outcome),
            Some(RecentConnectionOutcome::Connected)
        );
        let result = result.lines.join("\n");
        assert!(result.contains("Peer connected"), "{result}");
        assert!(result.contains("media delivery is not confirmed"));
        for secret in ["secret", "203.0.113", "Ready"] {
            assert!(!result.contains(secret), "{result}");
        }
    }

    // The streamer's old failed viewer must not be combined with a later
    // viewer's successful connection into a fictional shared outcome.
    #[test]
    fn host_connections_are_kept_separate_and_sorted_by_record_time() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("orange-media-7.jsonl"), concat!(
            "{\"at_unix_ms\":1000,\"build\":\"20ba4ee\",\"role\":\"host-viewer-1\",\"event\":\"ice-state\",\"payload\":\"Failed\"}\n",
            "{\"at_unix_ms\":2000,\"build\":\"20ba4ee\",\"role\":\"host-viewer-2\",\"event\":\"peer-connection-state\",\"payload\":\"Connected\"}\n"
        )).unwrap();
        let result = summarize(dir.path(), &AtomicBool::new(false));
        assert_eq!(
            result.latest.as_ref().map(|latest| latest.outcome),
            Some(RecentConnectionOutcome::Connected)
        );
        assert!(result.lines[0].contains("Peer connected"), "{result:?}");
        assert!(result.lines[1].contains("ICE connectivity checks failed"));
    }

    // A valid later record remains usable after a partial/oversized first line
    // in the bounded tail. Receiving RTP is not evidence of physical output.
    #[test]
    fn a_bounded_tail_still_reports_received_media_without_claiming_playback() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = vec![b'x'; 300_000];
        bytes.extend_from_slice(b"\n{\"at_unix_ms\":3000,\"build\":\"20ba4ee\",\"role\":\"watch\",\"event\":\"media-progress\",\"payload\":{\"progress\":{\"rtp\":{\"buffers\":5},\"decoded\":{\"buffers\":0}}}}\n");
        fs::write(dir.path().join("orange-media-8.jsonl"), bytes).unwrap();
        let result = summarize(dir.path(), &AtomicBool::new(false));
        assert_eq!(
            result.latest.as_ref().map(|latest| latest.outcome),
            Some(RecentConnectionOutcome::Connected)
        );
        let result = result.lines.join("\n");
        assert!(result.contains("Video RTP arrived"), "{result}");
        assert!(result.contains("physical playback is not verified"));
    }

    #[test]
    fn a_cancelled_scan_produces_no_stale_history() {
        let dir = tempfile::tempdir().unwrap();
        let summary = summarize(dir.path(), &AtomicBool::new(true));
        assert!(summary.lines.is_empty());
        assert!(summary.latest.is_none());
    }

    #[test]
    fn a_new_unknown_attempt_suppresses_older_failure_in_latest_outcome() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("orange-media-99.jsonl"),
            concat!(
                "{\"at_unix_ms\":1000,\"build\":\"20ba4ee\",\"role\":\"watch\",\"event\":\"ice-state\",\"payload\":\"Failed\"}\n",
                "{\"at_unix_ms\":2000,\"build\":\"20ba4ee\",\"role\":\"watch\",\"event\":\"connection-stage\",\"payload\":{\"event\":\"watch-started\"}}\n",
                "{\"at_unix_ms\":2001,\"build\":\"20ba4ee\",\"role\":\"watch\",\"event\":\"ice-state\",\"payload\":\"Checking\"}\n"
            ),
        )
        .unwrap();
        let summary = summarize(dir.path(), &AtomicBool::new(false));
        assert_eq!(
            summary.latest.as_ref().map(|latest| latest.outcome),
            Some(RecentConnectionOutcome::Unknown)
        );
    }
}
