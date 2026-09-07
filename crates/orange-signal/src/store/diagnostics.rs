//! Private Azure Blob persistence for diagnostics uploads.
//!
//! Uploads are written as `reports/<id>.json` objects in a dedicated private
//! container on the existing storage account used for table-backed sessions.

use anyhow::{Context, Result};
use base64::Engine;
use hmac::{Mac, SimpleHmac};
use reqwest::StatusCode;
use serde::Serialize;
use sha2::Sha256;
use std::time::{Duration, SystemTime};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const BLOB_VERSION: &str = "2021-12-02";
const BLOB_CONTENT_TYPE: &str = "application/json";
const BLOB_TYPE: &str = "BlockBlob";
const RESERVED_CONTAINERS: [&str; 2] = ["releases", "$web"];

#[derive(Clone)]
pub(crate) struct DiagnosticsStore {
    account: String,
    key: String,
    container: String,
    endpoint: String,
    http: reqwest::Client,
}

impl DiagnosticsStore {
    pub(crate) fn from_env() -> Option<Self> {
        let container = std::env::var("ORANGE_DIAGNOSTICS_CONTAINER").ok()?;
        let account = std::env::var("ORANGE_TABLE_ACCOUNT").ok()?;
        let key = std::env::var("ORANGE_TABLE_KEY").ok()?;
        if key.is_empty() || !valid_account_name(&account) || !valid_container_name(&container) {
            return None;
        }
        Some(Self {
            endpoint: format!("https://{account}.blob.core.windows.net"),
            account,
            key,
            container,
            http: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .ok()?,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(account: &str, key: &str, container: &str, endpoint: &str) -> Self {
        Self {
            account: account.to_string(),
            key: key.to_string(),
            container: container.to_string(),
            endpoint: endpoint.to_string(),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("test HTTP client configuration is valid"),
        }
    }

    pub(crate) async fn put_report<T: Serialize>(
        &self,
        report_id: &str,
        envelope: &T,
    ) -> Result<()> {
        let path = format!("{}/reports/{report_id}.json", self.container);
        let body = serde_json::to_vec(envelope).context("could not encode diagnostics envelope")?;
        let date = httpdate::fmt_http_date(SystemTime::now());
        let authorization = self.authorization(body.len(), &date, &path)?;

        let response = self
            .http
            .put(format!("{}/{}", self.endpoint, path))
            .header("Authorization", authorization)
            .header("x-ms-blob-type", BLOB_TYPE)
            .header("x-ms-date", &date)
            .header("x-ms-version", BLOB_VERSION)
            .header("Content-Type", BLOB_CONTENT_TYPE)
            .body(body)
            .send()
            .await
            .context("could not reach diagnostics blob storage")?;

        let status = response.status();
        if status != StatusCode::CREATED {
            anyhow::bail!("blob storage rejected diagnostics upload with status {status}");
        }
        Ok(())
    }

    fn authorization(&self, content_length: usize, date: &str, path: &str) -> Result<String> {
        let secret = base64::engine::general_purpose::STANDARD
            .decode(&self.key)
            .context("ORANGE_TABLE_KEY is not valid base64")?;
        let mut mac =
            SimpleHmac::<Sha256>::new_from_slice(&secret).context("invalid storage account key")?;
        mac.update(self.string_to_sign(content_length, date, path).as_bytes());
        let signature =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        Ok(format!("SharedKey {}:{signature}", self.account))
    }

    fn string_to_sign(&self, content_length: usize, date: &str, path: &str) -> String {
        let canonicalized_headers =
            format!("x-ms-blob-type:{BLOB_TYPE}\nx-ms-date:{date}\nx-ms-version:{BLOB_VERSION}\n");
        format!(
            "PUT\n\n\n{content_length}\n\n{BLOB_CONTENT_TYPE}\n\n\n\n\n\n\n{canonicalized_headers}/{}/{}",
            self.account, path
        )
    }
}

fn valid_account_name(value: &str) -> bool {
    value.len() >= 3
        && value.len() <= 24
        && value
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
}

fn valid_container_name(value: &str) -> bool {
    if RESERVED_CONTAINERS.contains(&value) {
        return false;
    }
    if value.len() < 3 || value.len() > 63 || value.contains("--") {
        return false;
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        return false;
    }
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let last = value.chars().last().unwrap_or(first);
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && (last.is_ascii_lowercase() || last.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        extract::State,
        http::Request,
        response::IntoResponse,
        routing::put,
        Router,
    };
    use base64::Engine;
    use hmac::{Mac, SimpleHmac};
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
    };
    use tokio::sync::Mutex;

    #[derive(Clone)]
    struct Capture {
        status: Arc<AtomicU16>,
        method: Arc<Mutex<Option<String>>>,
        path: Arc<Mutex<Option<String>>>,
        auth: Arc<Mutex<Option<String>>>,
        date: Arc<Mutex<Option<String>>>,
        version: Arc<Mutex<Option<String>>>,
        blob_type: Arc<Mutex<Option<String>>>,
        body: Arc<Mutex<Option<Vec<u8>>>>,
    }

    impl Default for Capture {
        fn default() -> Self {
            Self {
                status: Arc::new(AtomicU16::new(StatusCode::CREATED.as_u16())),
                method: Arc::new(Mutex::new(None)),
                path: Arc::new(Mutex::new(None)),
                auth: Arc::new(Mutex::new(None)),
                date: Arc::new(Mutex::new(None)),
                version: Arc::new(Mutex::new(None)),
                blob_type: Arc::new(Mutex::new(None)),
                body: Arc::new(Mutex::new(None)),
            }
        }
    }

    async fn fake_blob(
        State(capture): State<Capture>,
        request: Request<Body>,
    ) -> impl IntoResponse {
        *capture.method.lock().await = Some(request.method().as_str().to_string());
        *capture.path.lock().await = Some(request.uri().path().to_string());
        *capture.auth.lock().await = request
            .headers()
            .get("Authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        *capture.date.lock().await = request
            .headers()
            .get("x-ms-date")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        *capture.version.lock().await = request
            .headers()
            .get("x-ms-version")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        *capture.blob_type.lock().await = request
            .headers()
            .get("x-ms-blob-type")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        *capture.body.lock().await = Some(
            to_bytes(request.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        );
        StatusCode::from_u16(capture.status.load(Ordering::Relaxed)).unwrap()
    }

    #[tokio::test]
    async fn diagnostics_upload_signs_and_sends_the_expected_blob_request() {
        let capture = Capture::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/*path", put(fake_blob))
            .with_state(capture.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let key = base64::engine::general_purpose::STANDARD.encode(b"secret-key");
        let store = DiagnosticsStore::for_test("acct", &key, "diagnostics", &endpoint);
        let envelope = json!({"received_at_unix_ms":1,"account_id_hash":"abc","request":{"schema":1,"report":"ok","logs":[]}});

        store.put_report("abcd", &envelope).await.unwrap();

        let method = capture.method.lock().await.clone().unwrap();
        let path = capture.path.lock().await.clone().unwrap();
        let date = capture.date.lock().await.clone().unwrap();
        let version = capture.version.lock().await.clone().unwrap();
        let blob_type = capture.blob_type.lock().await.clone().unwrap();
        let body = capture.body.lock().await.clone().unwrap();
        let auth = capture.auth.lock().await.clone().unwrap();

        assert_eq!(method, "PUT");
        assert_eq!(path, "/diagnostics/reports/abcd.json");
        assert_eq!(version, BLOB_VERSION);
        assert_eq!(blob_type, "BlockBlob");
        let canonical_string = format!(
            "PUT\n\n\n{}\n\napplication/json\n\n\n\n\n\n\n\
x-ms-blob-type:BlockBlob\n\
x-ms-date:{date}\n\
x-ms-version:{BLOB_VERSION}\n\
/acct/diagnostics/reports/abcd.json",
            body.len()
        );
        let mut mac =
            SimpleHmac::<Sha256>::new_from_slice(b"secret-key").expect("test key is valid");
        mac.update(canonical_string.as_bytes());
        let signature =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        let expected = format!("SharedKey acct:{signature}");
        assert_eq!(auth, expected);

        server.abort();
    }

    #[tokio::test]
    async fn diagnostics_upload_treats_non_201_statuses_as_failures_without_following_redirects() {
        // Blob writes are successful only on Created. A redirect or another 2xx
        // can leave the report uncommitted at the intended private path.
        let capture = Capture::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/*path", put(fake_blob))
            .with_state(capture.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let key = base64::engine::general_purpose::STANDARD.encode(b"secret-key");
        let store = DiagnosticsStore::for_test("acct", &key, "diagnostics", &endpoint);
        let envelope = json!({"received_at_unix_ms":1,"account_id_hash":"abc","request":{"schema":1,"report":"ok","logs":[]}});

        capture
            .status
            .store(StatusCode::FOUND.as_u16(), Ordering::Relaxed);
        assert!(store.put_report("abcd", &envelope).await.is_err());

        capture
            .status
            .store(StatusCode::NO_CONTENT.as_u16(), Ordering::Relaxed);
        assert!(store.put_report("abcd", &envelope).await.is_err());

        server.abort();
    }

    #[test]
    fn blob_string_to_sign_includes_verb_length_content_type_headers_and_resource() {
        let store = DiagnosticsStore::for_test(
            "testaccount",
            &base64::engine::general_purpose::STANDARD.encode(b"key"),
            "diagnostics",
            "http://example.invalid",
        );
        let actual = store.string_to_sign(
            42,
            "Sun, 11 Oct 2009 19:52:39 GMT",
            "diagnostics/reports/abcd.json",
        );
        assert_eq!(
            actual,
            concat!(
                "PUT\n\n\n42\n\napplication/json\n\n\n\n\n\n\n",
                "x-ms-blob-type:BlockBlob\n",
                "x-ms-date:Sun, 11 Oct 2009 19:52:39 GMT\n",
                "x-ms-version:2021-12-02\n",
                "/testaccount/diagnostics/reports/abcd.json"
            )
        );
    }

    #[test]
    fn diagnostics_store_validates_account_and_container_names() {
        assert!(valid_account_name("abc123"));
        assert!(!valid_account_name("ab"));
        assert!(!valid_account_name("ABC"));

        assert!(valid_container_name("diagnostics"));
        assert!(valid_container_name("diag-logs-1"));
        assert!(!valid_container_name("releases"));
        assert!(!valid_container_name("$web"));
        assert!(!valid_container_name("ab"));
        assert!(!valid_container_name("bad--name"));
        assert!(!valid_container_name("-bad"));
        assert!(!valid_container_name("bad-"));
    }
}
