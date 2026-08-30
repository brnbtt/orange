//! HTTP + WebSocket server for the relay.
//!
//! Both live on one port because Azure Container Apps exposes a single
//! ingress. `axum` handles the routing and the WebSocket upgrade.

use crate::auth::{Auth, DiscordConfig, PollResult};
use crate::{handle_peer, Rooms};
use anyhow::{bail, Context, Result};
use axum::{
    async_trait,
    body::Bytes,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, FromRequestParts, Query, State,
    },
    http::{header, request::Parts, HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use uuid::Uuid;

const DIAGNOSTICS_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub(crate) enum DiagnosticsStorage {
    Disabled,
    Local(PathBuf),
    Blob {
        container_url: reqwest::Url,
        http: reqwest::Client,
    },
}

impl DiagnosticsStorage {
    fn from_env() -> Result<Self> {
        let container_url = std::env::var("ORANGE_DIAGNOSTICS_CONTAINER_URL").ok();
        let directory = std::env::var("ORANGE_DIAGNOSTICS_DIRECTORY").ok();
        Self::configured(container_url.as_deref(), directory.as_deref())
    }

    fn configured(container_url: Option<&str>, directory: Option<&str>) -> Result<Self> {
        if let Some(container_url) = container_url {
            return Self::blob(container_url);
        }
        if let Some(directory) = directory {
            if directory.is_empty() {
                bail!("ORANGE_DIAGNOSTICS_DIRECTORY cannot be empty");
            }
            return Ok(Self::Local(PathBuf::from(directory)));
        }
        Ok(Self::Disabled)
    }

    fn blob(container_url: &str) -> Result<Self> {
        let container_url =
            reqwest::Url::parse(container_url).context("invalid diagnostics container URL")?;
        Ok(Self::Blob {
            container_url,
            http: reqwest::Client::new(),
        })
    }

    fn description(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Local(_) => "local directory",
            Self::Blob { .. } => "Azure Blob",
        }
    }

    async fn store(&self, key: &str, body: Bytes) -> Result<()> {
        match self {
            Self::Disabled => bail!("diagnostics storage is disabled"),
            Self::Local(root) => store_local(root, key, &body).await,
            Self::Blob {
                container_url,
                http,
            } => {
                let mut url = container_url.clone();
                let mut segments = url
                    .path_segments_mut()
                    .map_err(|_| anyhow::anyhow!("diagnostics container URL cannot be a base"))?;
                segments.pop_if_empty();
                for segment in key.split('/') {
                    segments.push(segment);
                }
                drop(segments);

                http.put(url)
                    .header("x-ms-blob-type", "BlockBlob")
                    .header(header::CONTENT_TYPE, "application/zip")
                    .timeout(Duration::from_secs(30))
                    .body(body)
                    .send()
                    .await
                    .context("diagnostics blob request failed")?
                    .error_for_status()
                    .context("diagnostics blob storage rejected the upload")?;
                Ok(())
            }
        }
    }
}

async fn store_local(root: &Path, key: &str, body: &[u8]) -> Result<()> {
    let relative = Path::new(key);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("invalid diagnostics storage key");
    }

    tokio::fs::create_dir_all(root)
        .await
        .context("could not create diagnostics root")?;
    let root = tokio::fs::canonicalize(root)
        .await
        .context("could not resolve diagnostics root")?;
    let target = root.join(relative);
    let parent = target
        .parent()
        .context("diagnostics storage key has no parent")?;
    tokio::fs::create_dir_all(parent)
        .await
        .context("could not create diagnostics directory")?;
    let parent = tokio::fs::canonicalize(parent)
        .await
        .context("could not resolve diagnostics directory")?;
    if !parent.starts_with(&root) {
        bail!("diagnostics storage path escaped its root");
    }

    let file_name = target
        .file_name()
        .context("diagnostics storage key has no file name")?
        .to_string_lossy();
    let temporary = parent.join(format!(".{file_name}.{:016x}.tmp", rand::random::<u64>()));
    let write_result: Result<()> = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await
            .context("could not create temporary diagnostics file")?;
        file.write_all(body)
            .await
            .context("could not write diagnostics file")?;
        file.sync_all()
            .await
            .context("could not sync diagnostics file")?;
        drop(file);
        tokio::fs::rename(&temporary, parent.join(file_name.as_ref()))
            .await
            .context("could not commit diagnostics file")?;
        Ok(())
    }
    .await;
    if write_result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    write_result
}

#[derive(Clone)]
pub struct AppState {
    pub rooms: Rooms,
    pub auth: Auth,
    pub(crate) diagnostics: DiagnosticsStorage,
}

pub async fn serve(addr: &str) -> Result<()> {
    let auth = Auth::new(DiscordConfig::from_env());
    if auth.enabled() {
        println!("Discord login: configured");
    } else {
        println!("Discord login: not configured (set DISCORD_CLIENT_ID/SECRET/REDIRECT_URI)");
    }

    let diagnostics = DiagnosticsStorage::from_env()?;
    println!("Diagnostics storage: {}", diagnostics.description());
    let state = AppState {
        rooms: Arc::new(Mutex::new(HashMap::new())),
        auth,
        diagnostics,
    };

    let app = router(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("could not bind {addr}"))?;
    println!("orange relay listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { "orange relay" }))
        .route("/health", get(|| async { "ok" }))
        .route("/auth/start", get(auth_start))
        .route("/auth/callback", get(auth_callback))
        .route("/auth/poll", get(auth_poll))
        .route(
            "/diagnostics",
            post(upload_diagnostics).layer(DefaultBodyLimit::max(DIAGNOSTICS_LIMIT)),
        )
        .route("/ws", get(ws_upgrade))
        .with_state(state)
}

struct DiagnosticsMetadata {
    build: String,
    run: String,
    device: String,
    profile: String,
}

fn header_value<'a>(headers: &'a HeaderMap, name: &'static str) -> Result<&'a str, ()> {
    let mut values = headers.get_all(name).iter();
    let value = values.next().ok_or(())?;
    if values.next().is_some() {
        return Err(());
    }
    value.to_str().map_err(|_| ())
}

fn parse_diagnostics_metadata(headers: &HeaderMap) -> Result<DiagnosticsMetadata, ()> {
    let build = header_value(headers, "x-orange-build")?;
    if build.len() != 40
        || !build
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(());
    }

    let run = canonical_uuid(header_value(headers, "x-orange-run")?)?;
    let device = canonical_uuid(header_value(headers, "x-orange-device")?)?;
    let profile = header_value(headers, "x-orange-profile")?;
    if profile.is_empty()
        || profile.len() > 64
        || profile.starts_with('-')
        || profile.ends_with('-')
        || !profile
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(());
    }

    Ok(DiagnosticsMetadata {
        build: build.to_string(),
        run,
        device,
        profile: profile.to_string(),
    })
}

fn canonical_uuid(value: &str) -> Result<String, ()> {
    let parsed = Uuid::parse_str(value).map_err(|_| ())?;
    if parsed.hyphenated().to_string() != value {
        return Err(());
    }
    Ok(value.to_string())
}

fn diagnostics_storage_key(metadata: &DiagnosticsMetadata, discord_id: &str, date: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(discord_id.as_bytes()));
    format!(
        "{}/{}/{}/{}/{}.zip",
        metadata.build,
        date,
        &digest[..24],
        metadata.device,
        metadata.run
    )
}

struct AuthenticatedDiagnostics {
    identity: crate::auth::Identity,
    metadata: DiagnosticsMetadata,
}

#[async_trait]
impl FromRequestParts<AppState> for AuthenticatedDiagnostics {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> std::result::Result<Self, Self::Rejection> {
        let session = header_value(&parts.headers, "authorization")
            .ok()
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|value| !value.is_empty())
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let identity = state
            .auth
            .identify(session)
            .await
            .ok_or(StatusCode::UNAUTHORIZED)?;
        if header_value(&parts.headers, "content-type") != Ok("application/zip") {
            return Err(StatusCode::BAD_REQUEST);
        }
        let metadata =
            parse_diagnostics_metadata(&parts.headers).map_err(|()| StatusCode::BAD_REQUEST)?;
        let _validated_profile = &metadata.profile;
        Ok(Self { identity, metadata })
    }
}

async fn upload_diagnostics(
    State(app): State<AppState>,
    authenticated: AuthenticatedDiagnostics,
    body: Bytes,
) -> StatusCode {
    if matches!(&app.diagnostics, DiagnosticsStorage::Disabled) {
        return StatusCode::SERVICE_UNAVAILABLE;
    }

    let date = Utc::now().format("%Y-%m-%d").to_string();
    let key = diagnostics_storage_key(&authenticated.metadata, &authenticated.identity.id, &date);
    match app.diagnostics.store(&key, body).await {
        Ok(()) => StatusCode::CREATED,
        Err(_) => StatusCode::BAD_GATEWAY,
    }
}

#[derive(Serialize)]
struct StartResponse {
    url: String,
    state: String,
}

async fn auth_start(State(app): State<AppState>) -> impl IntoResponse {
    match app.auth.start().await {
        Ok((url, state)) => Json(StartResponse { url, state }).into_response(),
        Err(err) => (axum::http::StatusCode::SERVICE_UNAVAILABLE, err.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Where Discord sends the browser back to. The response is for a human, so it
/// is a page rather than JSON; the desktop app learns the outcome by polling.
async fn auth_callback(
    State(app): State<AppState>,
    Query(params): Query<CallbackParams>,
) -> impl IntoResponse {
    if let Some(error) = params.error {
        return Html(page("Login cancelled", &error));
    }
    let (Some(code), Some(state)) = (params.code, params.state) else {
        return Html(page("Login failed", "Discord did not return a code."));
    };

    match app.auth.complete(&state, &code).await {
        Ok(identity) => Html(page(
            &format!("Signed in as {}", identity.name),
            "You can close this tab and go back to orange.",
        )),
        Err(err) => Html(page("Login failed", &err.to_string())),
    }
}

#[derive(Deserialize)]
struct PollParams {
    state: String,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum PollBody {
    Waiting,
    Ready {
        session: String,
        id: String,
        name: String,
        avatar_url: Option<String>,
    },
    Failed {
        message: String,
    },
    Unknown,
}

async fn auth_poll(
    State(app): State<AppState>,
    Query(params): Query<PollParams>,
) -> impl IntoResponse {
    let body = match app.auth.poll(&params.state).await {
        PollResult::Waiting => PollBody::Waiting,
        PollResult::Ready { session, identity } => PollBody::Ready {
            session,
            id: identity.id,
            name: identity.name,
            avatar_url: identity.avatar_url,
        },
        PollResult::Failed(message) => PollBody::Failed { message },
        PollResult::Unknown => PollBody::Unknown,
    };
    Json(body)
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(app): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, app))
}

async fn handle_socket(socket: WebSocket, app: AppState) {
    if let Err(err) = handle_peer(socket, app.rooms, app.auth).await {
        eprintln!("[signal] peer disconnected: {err}");
    }
}

/// Minimal styled page for the browser leg of the login.
fn page(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>orange</title>
<style>
 body{{background:#141416;color:#eee;font:16px/1.6 system-ui,sans-serif;
      display:grid;place-items:center;height:100vh;margin:0;text-align:center}}
 h1{{color:#ff7a00;font-size:1.5rem;margin:0 0 .5rem}}
 p{{color:#9a9a9a;margin:0}}
</style></head><body><div><h1>{title}</h1><p>{body}</p></div></body></html>"#
    )
}

/// Convenience for the peer loop, which speaks in `Message`.
pub fn text(value: impl Serialize) -> Message {
    Message::Text(serde_json::to_string(&value).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Identity;
    use axum::{
        body::{to_bytes, Body},
        http::{header, HeaderMap, HeaderValue, Request, StatusCode, Uri},
    };
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;
    use tower::ServiceExt;

    const BUILD: &str = "0123456789abcdef0123456789abcdef01234567";
    const RUN: &str = "12345678-1234-4abc-8def-1234567890ab";
    const DEVICE: &str = "87654321-4321-4abc-8def-ba0987654321";

    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer valid-session".parse().unwrap(),
        );
        headers.insert(header::CONTENT_TYPE, "application/zip".parse().unwrap());
        headers.insert("x-orange-build", BUILD.parse().unwrap());
        headers.insert("x-orange-run", RUN.parse().unwrap());
        headers.insert("x-orange-device", DEVICE.parse().unwrap());
        headers.insert(
            "x-orange-profile",
            "hardware-bounded-jitter".parse().unwrap(),
        );
        headers
    }

    fn request(headers: HeaderMap, body: impl Into<Body>) -> Request<Body> {
        let mut request = Request::post("/diagnostics").body(body.into()).unwrap();
        *request.headers_mut() = headers;
        request
    }

    async fn app(storage: DiagnosticsStorage) -> Router {
        let auth = Auth::new(None);
        auth.insert_test_session(
            "valid-session",
            Identity {
                id: "discord-123".into(),
                name: "Tester".into(),
                avatar_url: None,
            },
        )
        .await;
        router(AppState {
            rooms: Rooms::default(),
            auth,
            diagnostics: storage,
        })
    }

    fn files_below(root: &Path) -> Vec<PathBuf> {
        let mut pending = vec![root.to_path_buf()];
        let mut files = Vec::new();
        while let Some(path) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(path) else {
                continue;
            };
            for entry in entries {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    files.push(path);
                }
            }
        }
        files
    }

    #[test]
    fn metadata_accepts_only_strict_canonical_values() {
        let valid = parse_diagnostics_metadata(&headers()).expect("valid metadata rejected");
        assert_eq!(valid.build, BUILD);
        assert_eq!(valid.run, RUN);
        assert_eq!(valid.device, DEVICE);
        assert_eq!(valid.profile, "hardware-bounded-jitter");

        let invalid = [
            ("x-orange-build", "ABCDEF0123456789abcdef0123456789abcdef01"),
            ("x-orange-build", "0123456789abcdef0123456789abcdef0123456"),
            ("x-orange-run", "1234567812344abc8def1234567890ab"),
            ("x-orange-run", "12345678-1234-4ABC-8def-1234567890ab"),
            ("x-orange-device", "not-a-uuid"),
            ("x-orange-profile", "-profile"),
            ("x-orange-profile", "profile-"),
            ("x-orange-profile", "has_underscore"),
            ("x-orange-profile", "UPPERCASE"),
            (
                "x-orange-profile",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
        ];
        for (name, value) in invalid {
            let mut candidate = headers();
            candidate.insert(name, HeaderValue::from_str(value).unwrap());
            assert!(
                parse_diagnostics_metadata(&candidate).is_err(),
                "accepted {name}: {value}"
            );
        }
    }

    #[test]
    fn storage_key_uses_date_and_pseudonymous_user_component() {
        let metadata = parse_diagnostics_metadata(&headers()).unwrap();
        assert_eq!(
            diagnostics_storage_key(&metadata, "discord-123", "2026-08-30"),
            "0123456789abcdef0123456789abcdef01234567/2026-08-30/d01927f07db83a3796cbab37/87654321-4321-4abc-8def-ba0987654321/12345678-1234-4abc-8def-1234567890ab.zip"
        );
    }

    #[test]
    fn diagnostics_configuration_prefers_blob_then_local_then_disabled() {
        assert!(matches!(
            DiagnosticsStorage::configured(
                Some("https://example.invalid/container?sig=secret"),
                Some("local")
            )
            .unwrap(),
            DiagnosticsStorage::Blob { .. }
        ));
        assert!(matches!(
            DiagnosticsStorage::configured(None, Some("local")).unwrap(),
            DiagnosticsStorage::Local(path) if path == Path::new("local")
        ));
        assert!(matches!(
            DiagnosticsStorage::configured(None, None).unwrap(),
            DiagnosticsStorage::Disabled
        ));
    }

    #[tokio::test]
    async fn diagnostics_requires_a_valid_bearer_session() {
        let temp = TempDir::new().unwrap();
        let app = app(DiagnosticsStorage::Local(temp.path().to_path_buf())).await;

        let mut missing = headers();
        missing.remove(header::AUTHORIZATION);
        assert_eq!(
            app.clone()
                .oneshot(request(missing, "zip"))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );

        let mut invalid = headers();
        invalid.insert(header::AUTHORIZATION, "Bearer expired".parse().unwrap());
        assert_eq!(
            app.oneshot(request(invalid, "zip")).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn diagnostics_rejects_malformed_content_type_and_metadata() {
        let temp = TempDir::new().unwrap();
        let app = app(DiagnosticsStorage::Local(temp.path().to_path_buf())).await;

        let mut bad_content_type = headers();
        bad_content_type.insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
        assert_eq!(
            app.clone()
                .oneshot(request(bad_content_type, "zip"))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );

        let mut bad_metadata = headers();
        bad_metadata.insert("x-orange-run", "../../escape".parse().unwrap());
        assert_eq!(
            app.oneshot(request(bad_metadata, "zip"))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert!(files_below(temp.path()).is_empty());
    }

    #[tokio::test]
    async fn diagnostics_authenticates_before_enforcing_the_body_limit() {
        let temp = TempDir::new().unwrap();
        let app = app(DiagnosticsStorage::Local(temp.path().to_path_buf())).await;
        let oversized = || vec![0_u8; DIAGNOSTICS_LIMIT + 1];

        let mut missing = headers();
        missing.remove(header::AUTHORIZATION);
        let response = app
            .clone()
            .oneshot(request(missing, oversized()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let mut invalid = headers();
        invalid.insert(header::AUTHORIZATION, "Bearer expired".parse().unwrap());
        let response = app
            .clone()
            .oneshot(request(invalid, oversized()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = app.oneshot(request(headers(), oversized())).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(files_below(temp.path()).is_empty());
    }

    #[tokio::test]
    async fn diagnostics_accepts_and_stores_exactly_eight_mib() {
        let temp = TempDir::new().unwrap();
        let response = app(DiagnosticsStorage::Local(temp.path().to_path_buf()))
            .await
            .oneshot(request(headers(), vec![0_u8; DIAGNOSTICS_LIMIT]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let files = files_below(temp.path());
        assert_eq!(files.len(), 1);
        assert_eq!(std::fs::metadata(&files[0]).unwrap().len(), 8 * 1024 * 1024);
    }

    #[tokio::test]
    async fn local_storage_rejects_parent_and_absolute_paths_before_writing() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("configured-root");
        let parent_target = temp.path().join("parent-escape.zip");
        assert!(store_local(&root, "../parent-escape.zip", b"secret")
            .await
            .is_err());
        assert!(!parent_target.exists());
        assert!(!root.exists());

        let absolute_target = temp.path().join("absolute-escape.zip");
        assert!(
            store_local(&root, absolute_target.to_str().unwrap(), b"secret")
                .await
                .is_err()
        );
        assert!(!absolute_target.exists());
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn diagnostics_returns_service_unavailable_when_storage_is_disabled() {
        let response = app(DiagnosticsStorage::Disabled)
            .await
            .oneshot(request(headers(), "zip"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn diagnostics_stores_exact_bytes_under_the_confined_layout() {
        let temp = TempDir::new().unwrap();
        let body = b"PK\x03\x04diagnostics";
        let response = app(DiagnosticsStorage::Local(temp.path().to_path_buf()))
            .await
            .oneshot(request(headers(), body.as_slice()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let files = files_below(temp.path());
        assert_eq!(files.len(), 1);
        assert_eq!(std::fs::read(&files[0]).unwrap(), body);
        let relative = files[0].strip_prefix(temp.path()).unwrap();
        let components: Vec<_> = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect();
        assert_eq!(components.len(), 5);
        assert_eq!(components[0], BUILD);
        assert_eq!(components[2], "d01927f07db83a3796cbab37");
        assert_eq!(components[3], DEVICE);
        assert_eq!(components[4], format!("{RUN}.zip"));
        assert_eq!(components[1].len(), 10);
        assert_eq!(components[1].as_bytes()[4], b'-');
        assert_eq!(components[1].as_bytes()[7], b'-');
    }

    #[tokio::test]
    async fn diagnostics_maps_backend_failures_without_exposing_details() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("not-a-directory");
        std::fs::write(&root, "occupied").unwrap();
        let response = app(DiagnosticsStorage::Local(root.clone()))
            .await
            .oneshot(request(headers(), "zip"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let response_body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert!(!String::from_utf8_lossy(&response_body).contains(&root.to_string_lossy()[..]));
    }

    #[tokio::test]
    async fn diagnostics_puts_a_block_blob_without_losing_the_sas_query() {
        let (observed_tx, mut observed_rx) = tokio::sync::mpsc::unbounded_channel();
        let blob_app = Router::new().fallback(
            move |uri: Uri, headers: HeaderMap, body: axum::body::Bytes| {
                let observed_tx = observed_tx.clone();
                async move {
                    observed_tx.send((uri, headers, body)).unwrap();
                    StatusCode::CREATED
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let blob_server =
            tokio::spawn(async move { axum::serve(listener, blob_app).await.unwrap() });
        let storage = DiagnosticsStorage::blob(&format!(
            "http://{address}/diagnostics-container?sp=cw&sig=do-not-log"
        ))
        .unwrap();

        let response = app(storage)
            .await
            .oneshot(request(headers(), "zip bytes"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let (uri, headers, body) =
            tokio::time::timeout(std::time::Duration::from_secs(2), observed_rx.recv())
                .await
                .unwrap()
                .unwrap();
        assert!(uri
            .path()
            .starts_with(&format!("/diagnostics-container/{BUILD}/")));
        assert!(uri
            .path()
            .ends_with(&format!("/d01927f07db83a3796cbab37/{DEVICE}/{RUN}.zip")));
        assert_eq!(uri.query(), Some("sp=cw&sig=do-not-log"));
        assert_eq!(headers["x-ms-blob-type"], "BlockBlob");
        assert_eq!(headers[header::CONTENT_TYPE], "application/zip");
        assert_eq!(body, "zip bytes");

        blob_server.abort();
    }
}
