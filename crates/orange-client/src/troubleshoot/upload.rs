//! Uploading troubleshooting reports to the configured relay.

use super::history;
use super::logs::LogAttachment;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::net::IpAddr;
use std::time::Duration;

const MAX_REPORT_BYTES: usize = 32 * 1024;
const MAX_LOGS: usize = 3;
const MAX_LOG_BYTES: usize = 128 * 1024;
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_RESPONSE_BYTES: u64 = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UploadError {
    SignedOut,
    Rejected,
    Unavailable,
    Network,
}

#[derive(Serialize)]
struct UploadRequest {
    schema: u8,
    report: String,
    logs: Vec<LogAttachment>,
}

#[derive(Deserialize)]
struct UploadResponse {
    report_id: String,
}

pub(super) fn send(
    server: &str,
    token: &str,
    report: &str,
    logs: Vec<LogAttachment>,
) -> Result<String, UploadError> {
    validate_payload(token, report, &logs)?;
    let url = diagnostics_url(server).map_err(|_| UploadError::Network)?;
    let request = UploadRequest {
        schema: 1,
        report: report.to_string(),
        logs,
    };
    let body = serde_json::to_vec(&request).map_err(|_| UploadError::Network)?;
    if body.len() > MAX_BODY_BYTES {
        return Err(UploadError::Rejected);
    }

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("orange/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| UploadError::Network)?;

    let response = client
        .post(url)
        .bearer_auth(token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .map_err(|_| UploadError::Network)?;

    match response.status().as_u16() {
        201 => {
            let bytes =
                read_limited(response, MAX_RESPONSE_BYTES).map_err(|_| UploadError::Network)?;
            let body: UploadResponse =
                serde_json::from_slice(&bytes).map_err(|_| UploadError::Network)?;
            if valid_report_id(&body.report_id) {
                Ok(body.report_id)
            } else {
                Err(UploadError::Network)
            }
        }
        401 => Err(UploadError::SignedOut),
        400 | 413 | 429 => Err(UploadError::Rejected),
        503 => Err(UploadError::Unavailable),
        _ => Err(UploadError::Network),
    }
}

pub(super) fn diagnostics_url(server: &str) -> Result<Url, String> {
    let mut url = Url::parse(server).map_err(|error| error.to_string())?;
    if url.username() != "" || url.password().is_some() || url.query().is_some() {
        return Err("server URL must not include credentials or a query".into());
    }
    match url.scheme() {
        "wss" => url
            .set_scheme("https")
            .map_err(|_| "could not build diagnostics URL".to_string())?,
        "ws" => url
            .set_scheme("http")
            .map_err(|_| "could not build diagnostics URL".to_string())?,
        _ => return Err("server URL must use ws or wss".into()),
    }
    url.set_path("/diagnostics");
    url.set_query(None);
    url.set_fragment(None);
    if url.scheme() == "http" && !loopback(&url) {
        return Err("sending diagnostics requires HTTPS, except loopback development URLs".into());
    }
    Ok(url)
}

fn loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .strip_prefix('[')
            .and_then(|stripped| stripped.strip_suffix(']'))
            .unwrap_or(host)
            .parse::<IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false)
}

fn validate_payload(token: &str, report: &str, logs: &[LogAttachment]) -> Result<(), UploadError> {
    if token.trim().is_empty() {
        return Err(UploadError::Rejected);
    }
    if report.len() > MAX_REPORT_BYTES {
        return Err(UploadError::Rejected);
    }
    if logs.len() > MAX_LOGS {
        return Err(UploadError::Rejected);
    }
    if logs
        .iter()
        .any(|log| !history::valid_log_name(&log.name) || log.contents.len() > MAX_LOG_BYTES)
    {
        return Err(UploadError::Rejected);
    }
    Ok(())
}

fn read_limited(mut response: reqwest::blocking::Response, limit: u64) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    response.by_ref().take(limit + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() as u64 <= limit, "response exceeds limit");
    Ok(bytes)
}

fn valid_report_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_url_uses_the_signalling_origin_without_credentials_or_query() {
        assert_eq!(
            diagnostics_url("wss://relay.example.com/ws")
                .unwrap()
                .as_str(),
            "https://relay.example.com/diagnostics"
        );
        assert_eq!(
            diagnostics_url("ws://127.0.0.1:9000/ws").unwrap().as_str(),
            "http://127.0.0.1:9000/diagnostics"
        );
        assert_eq!(
            diagnostics_url("ws://[::1]:9000/ws").unwrap().as_str(),
            "http://[::1]:9000/diagnostics"
        );
        assert!(diagnostics_url("ws://relay.example.com/ws").is_err());
        assert!(diagnostics_url("wss://user:pass@relay.example.com/ws").is_err());
        assert!(diagnostics_url("wss://relay.example.com/ws?token=1").is_err());
    }

    #[test]
    fn send_posts_authorized_payload_and_returns_a_valid_receipt() {
        let server = crate::background::tests::HttpServer::new(vec![(
            201,
            br#"{"report_id":"0123456789abcdef0123456789abcdef"}"#.to_vec(),
        )]);
        let report = "technical report";
        let logs = vec![LogAttachment {
            name: "orange-media-123.jsonl".into(),
            contents: "{\"event\":\"ok\"}".into(),
            truncated: false,
        }];
        let receipt = send(
            &server.url("/ws").replacen("http://", "ws://", 1),
            "session-token",
            report,
            logs,
        )
        .unwrap();
        assert_eq!(receipt, "0123456789abcdef0123456789abcdef");
        let requests = server.finish();
        let request = &requests[0].1;
        let lower = request.to_ascii_lowercase();
        assert!(lower.starts_with("post /diagnostics "));
        assert!(lower.contains("authorization: bearer session-token"));
        let body = request.split("\r\n\r\n").nth(1).expect("request body");
        let wire: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(wire["schema"], 1);
        assert_eq!(wire["report"], "technical report");
        assert_eq!(wire["logs"].as_array().unwrap().len(), 1);
        assert_eq!(wire["logs"][0]["name"], "orange-media-123.jsonl");
        assert_eq!(wire["logs"][0]["contents"], "{\"event\":\"ok\"}");
        assert_eq!(wire["logs"][0]["truncated"], false);
        assert!(!body.contains("session-token"));
    }

    #[test]
    fn send_maps_auth_service_and_receipt_failures() {
        let server = crate::background::tests::HttpServer::new(vec![
            (401, Vec::new()),
            (503, Vec::new()),
            (201, br#"{"report_id":"BAD"}"#.to_vec()),
        ]);
        let ws = server.url("/ws").replacen("http://", "ws://", 1);
        assert_eq!(
            send(&ws, "secret", "report", Vec::new()),
            Err(UploadError::SignedOut)
        );
        assert_eq!(
            send(&ws, "secret", "report", Vec::new()),
            Err(UploadError::Unavailable)
        );
        assert_eq!(
            send(&ws, "secret", "report", Vec::new()),
            Err(UploadError::Network)
        );
        assert_eq!(server.finish().len(), 3);
    }

    #[test]
    fn oversize_or_invalid_payload_is_rejected_before_http() {
        let valid = || LogAttachment {
            name: "orange-media-123.jsonl".into(),
            contents: "{\"event\":\"ok\"}".into(),
            truncated: false,
        };
        assert_eq!(
            send(
                "wss://relay.example.com/ws",
                "secret",
                &"x".repeat(MAX_REPORT_BYTES + 1),
                vec![valid()]
            ),
            Err(UploadError::Rejected)
        );
        assert_eq!(
            send(
                "wss://relay.example.com/ws",
                "secret",
                "ok",
                vec![valid(), valid(), valid(), valid()]
            ),
            Err(UploadError::Rejected)
        );
        assert_eq!(
            send(
                "wss://relay.example.com/ws",
                "secret",
                "ok",
                vec![LogAttachment {
                    name: "orange-media-0123.jsonl".into(),
                    contents: "{\"event\":\"ok\"}".into(),
                    truncated: false,
                }]
            ),
            Err(UploadError::Rejected)
        );
        assert_eq!(
            send(
                "wss://relay.example.com/ws",
                "secret",
                "ok",
                vec![LogAttachment {
                    name: "orange-media-123.jsonl".into(),
                    contents: "x".repeat(MAX_LOG_BYTES + 1),
                    truncated: false,
                }]
            ),
            Err(UploadError::Rejected)
        );
        assert_eq!(
            send("wss://relay.example.com/ws", "  ", "ok", vec![valid()]),
            Err(UploadError::Rejected)
        );
    }

    #[test]
    fn send_rejects_400_413_and_429() {
        let server = crate::background::tests::HttpServer::new(vec![
            (400, Vec::new()),
            (413, Vec::new()),
            (429, Vec::new()),
        ]);
        let ws = server.url("/ws").replacen("http://", "ws://", 1);
        assert_eq!(
            send(&ws, "secret", "report", Vec::new()),
            Err(UploadError::Rejected)
        );
        assert_eq!(
            send(&ws, "secret", "report", Vec::new()),
            Err(UploadError::Rejected)
        );
        assert_eq!(
            send(&ws, "secret", "report", Vec::new()),
            Err(UploadError::Rejected)
        );
        assert_eq!(server.finish().len(), 3);
    }
}
