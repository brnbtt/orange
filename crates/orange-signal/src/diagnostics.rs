//! Diagnostics upload validation and admission controls.
//!
//! The relay accepts support uploads from authenticated users and persists them
//! privately. This module owns request-shape checks plus bounded request and
//! per-account admission to keep the endpoint cheap under abuse.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

pub(crate) const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_REPORT_BYTES: usize = 32 * 1024;
pub(crate) const MAX_LOG_BYTES: usize = 128 * 1024;
pub(crate) const MAX_LOG_ATTACHMENTS: usize = 3;
pub(crate) const BODY_READ_TIMEOUT: Duration = Duration::from_secs(5);

const MAX_CONCURRENT_REQUESTS: usize = 4;
const RATE_LIMIT_MAX_REQUESTS: u32 = 3;
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);
const RATE_LIMIT_ACCOUNTS_CAPACITY: usize = 4096;

#[derive(Clone)]
pub(crate) struct Diagnostics {
    store: Option<crate::store::DiagnosticsStore>,
    requests: Arc<Semaphore>,
    accounts: Arc<Mutex<HashMap<String, (std::time::Instant, u32)>>>,
}

impl Diagnostics {
    pub(crate) fn from_env() -> Self {
        Self {
            store: crate::store::DiagnosticsStore::from_env(),
            requests: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
            accounts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(store: Option<crate::store::DiagnosticsStore>) -> Self {
        Self {
            store,
            requests: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
            accounts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn admit_request(&self) -> Option<OwnedSemaphorePermit> {
        self.requests.clone().try_acquire_owned().ok()
    }

    pub(crate) async fn admit_account(&self, id: &str) -> Result<(), ()> {
        let mut accounts = self.accounts.lock().await;
        let now = std::time::Instant::now();
        accounts.retain(|_, (since, _)| now.saturating_duration_since(*since) < RATE_LIMIT_WINDOW);
        if accounts.len() >= RATE_LIMIT_ACCOUNTS_CAPACITY && !accounts.contains_key(id) {
            return Err(());
        }
        let (_, count) = accounts.entry(id.to_string()).or_insert((now, 0));
        if *count >= RATE_LIMIT_MAX_REQUESTS {
            return Err(());
        }
        *count += 1;
        Ok(())
    }

    pub(crate) fn enabled(&self) -> bool {
        self.store.is_some()
    }

    pub(crate) async fn upload(
        &self,
        report_id: &str,
        identity_id: &str,
        request: &DiagnosticsUploadRequest,
    ) -> anyhow::Result<()> {
        let Some(store) = &self.store else {
            anyhow::bail!("diagnostics storage not configured");
        };
        let envelope = StoredDiagnosticsEnvelope {
            received_at_unix_ms: unix_ms_now(),
            account_id_hash: hashed_identity(identity_id),
            request,
        };
        store.put_report(report_id, &envelope).await
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiagnosticsUploadRequest {
    pub schema: u32,
    pub report: String,
    #[serde(default)]
    pub logs: Vec<DiagnosticsUploadLog>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiagnosticsUploadLog {
    pub name: String,
    pub contents: String,
    pub truncated: bool,
}

#[derive(Serialize)]
struct StoredDiagnosticsEnvelope<'a> {
    received_at_unix_ms: u64,
    account_id_hash: String,
    request: &'a DiagnosticsUploadRequest,
}

pub(crate) fn validate(request: &DiagnosticsUploadRequest) -> Result<(), &'static str> {
    if request.schema != 1 {
        return Err("schema must be 1");
    }
    if request.report.trim().is_empty() {
        return Err("report is required");
    }
    if request.report.len() > MAX_REPORT_BYTES {
        return Err("report is too large");
    }
    if request.logs.len() > MAX_LOG_ATTACHMENTS {
        return Err("too many logs");
    }
    for log in &request.logs {
        if !valid_log_name(&log.name) {
            return Err("invalid log name");
        }
        if log.contents.len() > MAX_LOG_BYTES {
            return Err("log contents are too large");
        }
    }
    Ok(())
}

pub(crate) fn random_report_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

pub(crate) fn valid_log_name(value: &str) -> bool {
    let Some(pid) = value
        .strip_prefix("orange-media-")
        .and_then(|tail| tail.strip_suffix(".jsonl"))
    else {
        return false;
    };
    if pid.is_empty() || pid.len() > 10 || !pid.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    if pid.starts_with('0') {
        return false;
    }
    pid.parse::<u32>().is_ok_and(|parsed| parsed > 0)
}

fn hashed_identity(id: &str) -> String {
    format!("{:x}", Sha256::digest(id.as_bytes()))
}

fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_upload_request_accepts_the_documented_log_filename_shape() {
        assert!(valid_log_name("orange-media-1.jsonl"));
        assert!(valid_log_name("orange-media-123456.jsonl"));
        assert!(valid_log_name("orange-media-4294967295.jsonl"));
        assert!(!valid_log_name("orange-media-0.jsonl"));
        assert!(!valid_log_name("orange-media-01.jsonl"));
        assert!(!valid_log_name("orange-media-4294967296.jsonl"));
        assert!(!valid_log_name("orange-media-12345678901.jsonl"));
        assert!(!valid_log_name("orange-media-abc.jsonl"));
        assert!(!valid_log_name("other-123.jsonl"));
    }

    #[test]
    fn diagnostics_upload_limits_match_the_contract() {
        let ok = DiagnosticsUploadRequest {
            schema: 1,
            report: "a".repeat(MAX_REPORT_BYTES),
            logs: vec![
                DiagnosticsUploadLog {
                    name: "orange-media-123.jsonl".into(),
                    contents: "b".repeat(MAX_LOG_BYTES),
                    truncated: true,
                };
                MAX_LOG_ATTACHMENTS
            ],
        };
        assert!(validate(&ok).is_ok());

        let mut too_many_logs = ok.clone();
        too_many_logs.logs.push(DiagnosticsUploadLog {
            name: "orange-media-456.jsonl".into(),
            contents: String::new(),
            truncated: false,
        });
        assert!(matches!(validate(&too_many_logs), Err("too many logs")));

        let too_large_report = DiagnosticsUploadRequest {
            report: "a".repeat(MAX_REPORT_BYTES + 1),
            ..ok.clone()
        };
        assert!(matches!(
            validate(&too_large_report),
            Err("report is too large")
        ));

        let blank_report = DiagnosticsUploadRequest {
            report: "   \n\t".into(),
            ..ok.clone()
        };
        assert!(matches!(validate(&blank_report), Err("report is required")));

        let mut too_large_log = ok;
        too_large_log.logs[0].contents.push('c');
        assert!(matches!(
            validate(&too_large_log),
            Err("log contents are too large")
        ));
    }

    #[tokio::test]
    async fn diagnostics_request_capacity_and_account_rate_limits_are_bounded() {
        let diagnostics = Diagnostics::for_test(None);
        let mut permits = Vec::new();
        for _ in 0..4 {
            permits.push(diagnostics.admit_request().expect("capacity should remain"));
        }
        assert!(diagnostics.admit_request().is_none());
        drop(permits.pop());
        assert!(diagnostics.admit_request().is_some());

        for _ in 0..3 {
            diagnostics.admit_account("42").await.unwrap();
        }
        assert!(diagnostics.admit_account("42").await.is_err());
    }
}
