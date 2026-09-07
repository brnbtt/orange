//! HTTP + WebSocket server for the relay.
//!
//! Both live on one port because Azure Container Apps exposes a single
//! ingress. `axum` handles the routing and the WebSocket upgrade.

use crate::auth::{Auth, DiscordConfig, PollResult};
use crate::diagnostics::{DiagnosticsUploadRequest, BODY_READ_TIMEOUT, MAX_BODY_BYTES};
use crate::relay::{handle_peer, presence_for, Found, Rooms};
use anyhow::{Context, Result};
use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{
        ws::{WebSocket, WebSocketUpgrade},
        Query, Request, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
#[cfg(test)]
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore};

const CONNECTION_CAPACITY: usize = 512;
static CONNECTIONS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(CONNECTION_CAPACITY)));

#[derive(Clone)]
pub struct AppState {
    pub rooms: Rooms,
    pub auth: Auth,
    pub(crate) social: crate::social::Social,
}

pub async fn serve(addr: &str) -> Result<()> {
    let auth = Auth::new(DiscordConfig::from_env());
    if auth.enabled() {
        println!("Discord login: configured");
    } else {
        println!("Discord login: not configured (set DISCORD_CLIENT_ID/SECRET/REDIRECT_URI)");
    }
    if auth.durable() {
        println!("Sessions: durable (Azure Table Storage)");
    } else {
        // Said at startup rather than discovered later: without this the only
        // symptom is that a deploy signs everyone out, which looks like a bug
        // in the client rather than missing configuration here.
        println!(
            "Sessions: in memory only, a restart signs everyone out \
             (set ORANGE_TABLE_ACCOUNT/KEY/NAME)"
        );
    }

    let state = AppState {
        rooms: Arc::new(Mutex::new(HashMap::new())),
        auth,
        social: crate::social::Social::new(crate::store::TableStore::from_env()),
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
    router_with_diagnostics(state, crate::diagnostics::Diagnostics::from_env())
}

fn router_with_diagnostics(
    state: AppState,
    diagnostics: crate::diagnostics::Diagnostics,
) -> Router {
    Router::new()
        .route("/", get(|| async { "orange relay" }))
        .route("/health", get(|| async { "ok" }))
        .route("/auth/start", get(auth_start))
        .route("/auth/callback", get(auth_callback))
        .route("/auth/poll", get(auth_poll))
        .route("/presence", get(presence))
        .route(
            "/friends",
            get(friends)
                .post(change_friend)
                .layer(axum::extract::DefaultBodyLimit::max(4096)),
        )
        .route(
            "/diagnostics",
            post(move |state, request| post_diagnostics(state, request, diagnostics.clone())),
        )
        .route("/ws", get(ws_upgrade))
        .with_state(state)
}

#[derive(Serialize)]
struct DiagnosticsReceipt {
    report_id: String,
}

async fn post_diagnostics(
    State(app): State<AppState>,
    request: Request,
    diagnostics: crate::diagnostics::Diagnostics,
) -> Response {
    let Some(_permit) = diagnostics.admit_request() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "too many diagnostics uploads; try again shortly",
        )
            .into_response();
    };
    let (parts, body) = request.into_parts();
    let identity = match authenticated_identity(&app, &parts.headers).await {
        Ok(identity) => identity,
        Err(response) => return *response,
    };

    if diagnostics.admit_account(&identity.id).await.is_err() {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "diagnostics upload limit reached; try again shortly",
        )
            .into_response();
    }
    if !diagnostics.enabled() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "diagnostics uploads are temporarily unavailable",
        )
            .into_response();
    }

    let bytes = match read_diagnostics_body(body, &parts.headers).await {
        Ok(bytes) => bytes,
        Err(error) => return error.into_response(),
    };
    let request: DiagnosticsUploadRequest = match serde_json::from_slice(&bytes) {
        Ok(request) => request,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                "invalid diagnostics upload payload",
            )
                .into_response()
        }
    };
    if crate::diagnostics::validate(&request).is_err() {
        return (
            StatusCode::BAD_REQUEST,
            "invalid diagnostics upload payload",
        )
            .into_response();
    }

    let report_id = crate::diagnostics::random_report_id();
    if let Err(error) = diagnostics.upload(&report_id, &identity.id, &request).await {
        eprintln!("[diagnostics] upload failed: {error:#}");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "diagnostics uploads are temporarily unavailable",
        )
            .into_response();
    }

    (StatusCode::CREATED, Json(DiagnosticsReceipt { report_id })).into_response()
}

async fn read_diagnostics_body(
    body: Body,
    headers: &HeaderMap,
) -> std::result::Result<Bytes, (StatusCode, &'static str)> {
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_BODY_BYTES)
    {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "diagnostics upload exceeds the size limit",
        ));
    }

    match tokio::time::timeout(BODY_READ_TIMEOUT, to_bytes(body, MAX_BODY_BYTES)).await {
        Ok(Ok(bytes)) => Ok(bytes),
        Err(_) => Err((
            StatusCode::REQUEST_TIMEOUT,
            "diagnostics upload timed out; try again",
        )),
        Ok(Err(error)) if is_length_limit_error(&error) => Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "diagnostics upload exceeds the size limit",
        )),
        Ok(Err(_)) => Err((
            StatusCode::BAD_REQUEST,
            "invalid diagnostics upload payload",
        )),
    }
}

fn is_length_limit_error(error: &axum::Error) -> bool {
    let mut current = Some(error as &(dyn std::error::Error + 'static));
    while let Some(err) = current {
        if err
            .to_string()
            .to_ascii_lowercase()
            .contains("length limit")
        {
            return true;
        }
        current = err.source();
    }
    false
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
) -> Response {
    if let Some(error) = params.error {
        if let Some(state) = params.state.as_deref() {
            app.auth.fail(state).await;
        }
        return auth_page("Login cancelled", &error);
    }
    let (Some(code), Some(state)) = (params.code, params.state) else {
        return auth_page("Login failed", "Discord did not return a code.");
    };

    match app.auth.complete(&state, &code).await {
        Ok(identity) => auth_page(
            &format!("Signed in as {}", identity.name),
            "You can close this tab and go back to orange.",
        ),
        Err(err) => auth_page("Login failed", &err.to_string()),
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

/// Cap on ids per presence query. Far above any plausible friend list, but it
/// stops one request from pinning the room lock while it walks an attacker's
/// arbitrarily long id list.
const PRESENCE_QUERY_CAPACITY: usize = 256;
const PRESENCE_MAX_WAIT_SECS: u64 = 20;
const PRESENCE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Deserialize)]
struct PresenceParams {
    /// Comma-separated Discord ids.
    #[serde(default)]
    ids: String,
    #[serde(default)]
    wait: Option<u64>,
    #[serde(default)]
    since: Option<String>,
}

#[derive(Serialize)]
struct PresenceEntry {
    id: String,
    /// Omitted for an offline friend: the relay only holds a profile while
    /// its owner is connected. The caller keeps the last one it saw.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    avatar_url: Option<String>,
    #[serde(flatten)]
    presence: crate::relay::Presence,
}

#[derive(Serialize)]
struct PresenceBody {
    friends: Vec<PresenceEntry>,
}

#[derive(Serialize)]
struct PresenceWaitBody {
    friends: Vec<PresenceEntry>,
    revision: String,
}

#[derive(Clone)]
struct PresenceAuthz {
    accepted: Vec<String>,
    legacy: Vec<String>,
}

struct PresenceSnapshot {
    friends: Vec<PresenceEntry>,
    revision: String,
    authz: PresenceAuthz,
}

/// Who among the caller's friends is streaming right now.
///
/// Unlike `/ws`, this refuses an unauthenticated caller. Presence is answered
/// in terms of "who are you", so there is no anonymous form of the question,
/// and a silent empty answer would be indistinguishable from every friend
/// being offline.
async fn presence(
    State(app): State<AppState>,
    Query(params): Query<PresenceParams>,
    headers: HeaderMap,
) -> Response {
    let permit = match app.social.admit() {
        Ok(permit) => permit,
        Err(error) => return social_error(error),
    };
    let identity = match authenticated_identity(&app, &headers).await {
        Ok(identity) => identity,
        Err(response) => return *response,
    };
    let mut viewer_id = identity.id.clone();

    let ids: Vec<String> = params
        .ids
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
    if ids.len() > PRESENCE_QUERY_CAPACITY {
        return (StatusCode::BAD_REQUEST, "too many ids").into_response();
    }

    let wait_secs = params.wait.unwrap_or(0).min(PRESENCE_MAX_WAIT_SECS);
    let since = if wait_secs > 0 {
        match parse_since(params.since.as_deref()) {
            Ok(since) => since,
            Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
        }
    } else {
        None
    };

    if let Err(error) = app.social.admit_read(&identity.id).await {
        return social_error(error);
    }
    let mut snapshot = match snapshot_for_presence(&app, &identity, &ids).await {
        Ok(snapshot) => snapshot,
        Err(error) => return social_error(error),
    };

    if wait_secs == 0 {
        return Json(PresenceBody {
            friends: snapshot.friends,
        })
        .into_response();
    }

    let Some(since) = since else {
        return Json(PresenceWaitBody {
            friends: snapshot.friends,
            revision: snapshot.revision,
        })
        .into_response();
    };
    if snapshot.revision != since {
        return Json(PresenceWaitBody {
            friends: snapshot.friends,
            revision: snapshot.revision,
        })
        .into_response();
    }

    let Some(_waiter) = app.social.admit_presence_waiter() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    drop(permit);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait_secs);
    while tokio::time::Instant::now() < deadline {
        let now = tokio::time::Instant::now();
        let sleep_for = std::cmp::min(
            PRESENCE_POLL_INTERVAL,
            deadline.saturating_duration_since(now),
        );
        if sleep_for.is_zero() {
            break;
        }
        tokio::time::sleep(sleep_for).await;

        let candidate = candidate_revision(&app, &viewer_id, &ids, &snapshot.authz).await;
        if candidate != since {
            let _permit = match app.social.admit_presence_refresh().await {
                Ok(permit) => permit,
                Err(error) => return social_error(error),
            };
            let refreshed_identity = match authenticated_identity(&app, &headers).await {
                Ok(identity) => identity,
                Err(response) => return *response,
            };
            viewer_id = refreshed_identity.id.clone();
            snapshot = match snapshot_for_presence(&app, &refreshed_identity, &ids).await {
                Ok(snapshot) => snapshot,
                Err(error) => return social_error(error),
            };
            if snapshot.revision != since {
                return Json(PresenceWaitBody {
                    friends: snapshot.friends,
                    revision: snapshot.revision,
                })
                .into_response();
            }
        }
    }

    let _permit = match app.social.admit_presence_refresh().await {
        Ok(permit) => permit,
        Err(error) => return social_error(error),
    };
    let refreshed_identity = match authenticated_identity(&app, &headers).await {
        Ok(identity) => identity,
        Err(response) => return *response,
    };
    snapshot = match snapshot_for_presence(&app, &refreshed_identity, &ids).await {
        Ok(snapshot) => snapshot,
        Err(error) => return social_error(error),
    };
    Json(PresenceWaitBody {
        friends: snapshot.friends,
        revision: snapshot.revision,
    })
    .into_response()
}

fn parse_since(raw: Option<&str>) -> std::result::Result<Option<String>, &'static str> {
    let Some(value) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if value.len() != 64 || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err("invalid since revision");
    }
    Ok(Some(value.to_ascii_lowercase()))
}

async fn snapshot_for_presence(
    app: &AppState,
    identity: &crate::auth::Identity,
    ids: &[String],
) -> std::result::Result<PresenceSnapshot, crate::social::Error> {
    let relationships = app.social.relationships(&identity.id).await?;
    let states: HashMap<_, _> = relationships
        .iter()
        .map(|r| {
            let other = if r.sender.id == identity.id {
                &r.recipient.id
            } else {
                &r.sender.id
            };
            (other.as_str(), r.state)
        })
        .collect();
    let accepted: Vec<_> = ids
        .iter()
        .filter(|id| states.get(id.as_str()) == Some(&crate::social::RelationshipState::Accepted))
        .cloned()
        .collect();
    let legacy: Vec<_> = ids
        .iter()
        .filter(|id| !states.contains_key(id.as_str()))
        .cloned()
        .collect();

    let mut found: HashMap<_, _> = presence_for(&app.rooms, &identity.id, &legacy)
        .await
        .into_iter()
        .collect();
    found.extend(crate::relay::presence_for_friends(&app.rooms, &accepted).await);
    let friends = materialize_presence_entries(ids, &mut found);
    let revision = revision_for_presence(&friends);
    Ok(PresenceSnapshot {
        friends,
        revision,
        authz: PresenceAuthz { accepted, legacy },
    })
}

async fn candidate_revision(
    app: &AppState,
    viewer_id: &str,
    ids: &[String],
    authz: &PresenceAuthz,
) -> String {
    let mut found: HashMap<_, _> = presence_for(&app.rooms, viewer_id, &authz.legacy)
        .await
        .into_iter()
        .collect();
    found.extend(crate::relay::presence_for_friends(&app.rooms, &authz.accepted).await);
    let friends = materialize_presence_entries(ids, &mut found);
    revision_for_presence(&friends)
}

fn materialize_presence_entries(
    ids: &[String],
    found: &mut HashMap<String, Found>,
) -> Vec<PresenceEntry> {
    ids.iter()
        .map(|id| {
            let value = found.remove(id).unwrap_or(Found {
                presence: crate::relay::Presence::Offline,
                name: None,
                avatar_url: None,
            });
            (id.clone(), value)
        })
        .map(|(id, found)| {
            let Found {
                presence,
                name,
                avatar_url,
            } = found;
            PresenceEntry {
                id,
                name,
                avatar_url,
                presence,
            }
        })
        .collect()
}

fn revision_for_presence(entries: &[PresenceEntry]) -> String {
    let mut hasher = Sha256::new();
    for entry in entries {
        let encoded = serde_json::to_vec(entry)
            .expect("presence entries are always serializable for revision hashing");
        hasher.update((encoded.len() as u64).to_le_bytes());
        hasher.update(&encoded);
    }
    format!("{:x}", hasher.finalize())
}

async fn friends(State(app): State<AppState>, headers: HeaderMap) -> Response {
    let _permit = match app.social.admit() {
        Ok(permit) => permit,
        Err(error) => return social_error(error),
    };
    let identity = match authenticated_identity(&app, &headers).await {
        Ok(identity) => identity,
        Err(response) => return *response,
    };
    if let Err(error) = app.social.admit_read(&identity.id).await {
        return social_error(error);
    }
    match app.social.snapshot(&identity).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(error) => social_error(error),
    }
}

async fn change_friend(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(change): Json<crate::social::Change>,
) -> Response {
    let _permit = match app.social.admit() {
        Ok(permit) => permit,
        Err(error) => return social_error(error),
    };
    let identity = match authenticated_identity(&app, &headers).await {
        Ok(identity) => identity,
        Err(response) => return *response,
    };
    match app.social.change(&identity, &change).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => social_error(error),
    }
}

fn social_error(error: crate::social::Error) -> Response {
    use crate::social::Error;
    let (status, message) = match error {
        Error::Invalid(message) => (StatusCode::BAD_REQUEST, message),
        Error::Forbidden => (
            StatusCode::FORBIDDEN,
            "only the recipient can answer this request",
        ),
        Error::Conflict => (
            StatusCode::CONFLICT,
            "this request changed; refresh and try again",
        ),
        Error::NotFound => (
            StatusCode::NOT_FOUND,
            "friend or request not found; your friend may need to open the updated Orange once",
        ),
        Error::Capacity => (
            StatusCode::TOO_MANY_REQUESTS,
            "friend or request limit reached",
        ),
        Error::Storage(error) => {
            eprintln!("[friends] storage operation failed: {error:#}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "could not save or load friends; try again shortly",
            )
        }
    };
    (status, message).into_response()
}

async fn authenticated_identity(
    app: &AppState,
    headers: &HeaderMap,
) -> std::result::Result<crate::auth::Identity, Box<Response>> {
    let token =
        bearer_token(headers).ok_or_else(|| Box::new(StatusCode::UNAUTHORIZED.into_response()))?;
    match app.auth.identify_checked(token).await {
        Ok(Some(identity)) => Ok(identity),
        Ok(None) => Err(Box::new(
            (StatusCode::UNAUTHORIZED, "unknown or expired session").into_response(),
        )),
        Err(error) => Err(Box::new(social_error(crate::social::Error::Storage(error)))),
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(app): State<AppState>) -> impl IntoResponse {
    ws_upgrade_with_limit(ws, app, CONNECTIONS.clone())
}

fn ws_upgrade_with_limit(
    ws: WebSocketUpgrade,
    app: AppState,
    connections: Arc<Semaphore>,
) -> Response {
    let Ok(permit) = connections.try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    ws.max_message_size(64 * 1024)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            handle_socket(socket, app).await;
        })
        .into_response()
}

async fn handle_socket(socket: WebSocket, app: AppState) {
    if let Err(err) = handle_peer(socket, app.rooms, app.auth).await {
        eprintln!("[signal] peer disconnected: {err}");
    }
}

/// Minimal styled page for the browser leg of the login.
///
/// The palette is the app's, so the tab that opens mid-sign-in does not look
/// like somebody else's site. The faces are the system's: the CSP on this
/// response is `default-src 'none'`, and widening it to fetch a webfont for
/// two seconds of copy is not a trade worth making.
fn page(title: &str, body: &str) -> String {
    let title = escape_html(title);
    let body = escape_html(body);
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>orange</title>
<style>
 body{{background:#070708;color:#e6e0d1;font:15px/1.6 system-ui,sans-serif;
      display:grid;place-items:center;height:100vh;margin:0;text-align:center}}
 h1{{color:#ff5a1f;font-size:1.35rem;margin:0 0 .75rem}}
 hr{{width:28px;height:2px;border:0;background:#ff5a1f;margin:0 auto .75rem}}
 p{{color:#99948a;margin:0}}
</style></head><body><div><h1>{title}</h1><hr><p>{body}</p></div></body></html>"#
    )
}

fn auth_page(title: &str, body: &str) -> Response {
    (
        [
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; frame-ancestors 'none'",
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        Html(page(title, body)),
    )
        .into_response()
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body, Bytes},
        extract::State,
        http::{header, Request, StatusCode},
        response::IntoResponse,
        routing::put,
        Router,
    };
    use base64::Engine;
    use futures_util::stream;
    use serde_json::json;
    use serde_json::Value;
    use std::sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
    };
    use tokio::sync::Mutex as AsyncMutex;
    use tower::ServiceExt;

    #[derive(Clone)]
    struct BlobCapture {
        status: Arc<AtomicU16>,
        paths: Arc<AsyncMutex<Vec<String>>>,
        bodies: Arc<AsyncMutex<Vec<Vec<u8>>>>,
    }

    impl Default for BlobCapture {
        fn default() -> Self {
            Self {
                status: Arc::new(AtomicU16::new(StatusCode::CREATED.as_u16())),
                paths: Arc::new(AsyncMutex::new(Vec::new())),
                bodies: Arc::new(AsyncMutex::new(Vec::new())),
            }
        }
    }

    async fn fake_blob(
        State(capture): State<BlobCapture>,
        request: Request<Body>,
    ) -> impl IntoResponse {
        capture
            .paths
            .lock()
            .await
            .push(request.uri().path().to_string());
        capture.bodies.lock().await.push(
            to_bytes(request.into_body(), 3 * 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        );
        StatusCode::from_u16(capture.status.load(Ordering::Relaxed)).unwrap()
    }

    async fn diagnostics_router_for_test(
        diagnostics: crate::diagnostics::Diagnostics,
    ) -> (Router, Auth) {
        let auth = Auth::new(None);
        auth.insert_session_for_test(
            "good-token",
            crate::auth::Identity {
                id: "42".into(),
                name: "Me".into(),
                avatar_url: None,
            },
        )
        .await;
        let app = router_with_diagnostics(
            AppState {
                rooms: Default::default(),
                auth: auth.clone(),
                social: crate::social::Social::new(None),
            },
            diagnostics,
        );
        (app, auth)
    }

    async fn limited_ws_upgrade(
        ws: WebSocketUpgrade,
        State((app, connections)): State<(AppState, Arc<tokio::sync::Semaphore>)>,
    ) -> Response {
        ws_upgrade_with_limit(ws, app, connections)
    }

    #[tokio::test]
    async fn oauth_callback_escapes_untrusted_html_and_sets_csp() {
        let app = router(AppState {
            rooms: Default::default(),
            auth: crate::auth::Auth::new(None),
            social: crate::social::Social::new(None),
        });
        let response = app
            .oneshot(
                Request::get("/auth/callback?error=%3Cscript%3Ealert%281%29%3C%2Fscript%3E")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .headers()
            .contains_key(header::CONTENT_SECURITY_POLICY));
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[tokio::test]
    async fn diagnostics_requires_a_valid_bearer_session() {
        let diagnostics = crate::diagnostics::Diagnostics::for_test(None);
        let (app, _) = diagnostics_router_for_test(diagnostics).await;
        for authorization in [None, Some("Bearer expired"), Some("good-token")] {
            let mut request = Request::post("/diagnostics")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"schema":1,"report":"ok","logs":[]}"#))
                .unwrap();
            if let Some(value) = authorization {
                request.headers_mut().insert(
                    header::AUTHORIZATION,
                    header::HeaderValue::from_str(value).unwrap(),
                );
            }
            let response = app.clone().oneshot(request).await.unwrap();
            let expected = if authorization == Some("Bearer good-token") {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::UNAUTHORIZED
            };
            assert_eq!(response.status(), expected);
        }
    }

    #[tokio::test]
    async fn diagnostics_returns_201_and_persists_the_envelope_after_a_successful_upload() {
        let capture = BlobCapture::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let blob_app = Router::new()
            .route("/*path", put(fake_blob))
            .with_state(capture.clone());
        let blob_server =
            tokio::spawn(async move { axum::serve(listener, blob_app).await.unwrap() });

        let store = crate::store::DiagnosticsStore::for_test(
            "acct",
            &base64::engine::general_purpose::STANDARD.encode(b"key"),
            "diagnostics",
            &endpoint,
        );
        let diagnostics = crate::diagnostics::Diagnostics::for_test(Some(store));
        let (app, _) = diagnostics_router_for_test(diagnostics).await;
        let response = app
            .oneshot(
                Request::post("/diagnostics")
                    .header(header::AUTHORIZATION, "Bearer good-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"schema":1,"report":"full report","logs":[{"name":"orange-media-123.jsonl","contents":"{}","truncated":true}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024).await.unwrap()).unwrap();
        let report_id = body
            .get("report_id")
            .and_then(Value::as_str)
            .expect("report_id should be present");
        assert_eq!(report_id.len(), 32);
        assert!(report_id
            .chars()
            .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()));

        let paths = capture.paths.lock().await.clone();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], format!("/diagnostics/reports/{report_id}.json"));

        let bodies = capture.bodies.lock().await.clone();
        let envelope: Value = serde_json::from_slice(&bodies[0]).unwrap();
        assert!(envelope["received_at_unix_ms"].as_u64().is_some());
        assert_eq!(
            envelope["account_id_hash"].as_str().unwrap(),
            format!("{:x}", sha2::Sha256::digest(b"42"))
        );
        assert_eq!(
            envelope["request"],
            json!({"schema":1,"report":"full report","logs":[{"name":"orange-media-123.jsonl","contents":"{}","truncated":true}]})
        );

        blob_server.abort();
    }

    #[tokio::test]
    async fn diagnostics_rejects_invalid_json_schema_and_filenames() {
        let capture = BlobCapture::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let blob_app = Router::new()
            .route("/*path", put(fake_blob))
            .with_state(capture.clone());
        let blob_server =
            tokio::spawn(async move { axum::serve(listener, blob_app).await.unwrap() });
        let store = crate::store::DiagnosticsStore::for_test(
            "acct",
            &base64::engine::general_purpose::STANDARD.encode(b"key"),
            "diagnostics",
            &endpoint,
        );
        let diagnostics = crate::diagnostics::Diagnostics::for_test(Some(store));
        let (app, _) = diagnostics_router_for_test(diagnostics).await;

        let invalid_cases = [
            r#"{"schema":2,"report":"ok","logs":[]}"#,
            r#"{"schema":1,"report":"ok","logs":[{"name":"bad.jsonl","contents":"x","truncated":false}]}"#,
            "{",
        ];
        for body in invalid_cases {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/diagnostics")
                        .header(header::AUTHORIZATION, "Bearer good-token")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }

        assert!(capture.paths.lock().await.is_empty());
        blob_server.abort();
    }

    #[tokio::test]
    async fn diagnostics_rejects_requests_larger_than_two_mebibytes() {
        let capture = BlobCapture::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let blob_app = Router::new()
            .route("/*path", put(fake_blob))
            .with_state(capture.clone());
        let blob_server =
            tokio::spawn(async move { axum::serve(listener, blob_app).await.unwrap() });
        let store = crate::store::DiagnosticsStore::for_test(
            "acct",
            &base64::engine::general_purpose::STANDARD.encode(b"key"),
            "diagnostics",
            &endpoint,
        );
        let diagnostics = crate::diagnostics::Diagnostics::for_test(Some(store));
        let (app, _) = diagnostics_router_for_test(diagnostics).await;

        let too_large = vec![b'x'; crate::diagnostics::MAX_BODY_BYTES + 1];
        let response = app
            .oneshot(
                Request::post("/diagnostics")
                    .header(header::AUTHORIZATION, "Bearer good-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(too_large))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(capture.paths.lock().await.is_empty());
        blob_server.abort();
    }

    #[tokio::test]
    async fn diagnostics_body_read_timeout_returns_408_instead_of_guessing_size() {
        let capture = BlobCapture::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let blob_app = Router::new()
            .route("/*path", put(fake_blob))
            .with_state(capture);
        let blob_server =
            tokio::spawn(async move { axum::serve(listener, blob_app).await.unwrap() });
        let store = crate::store::DiagnosticsStore::for_test(
            "acct",
            &base64::engine::general_purpose::STANDARD.encode(b"key"),
            "diagnostics",
            &endpoint,
        );
        let diagnostics = crate::diagnostics::Diagnostics::for_test(Some(store));
        let (app, _) = diagnostics_router_for_test(diagnostics).await;
        let delayed_body = Body::from_stream(stream::once(async {
            tokio::time::sleep(BODY_READ_TIMEOUT + Duration::from_millis(200)).await;
            Ok::<Bytes, std::io::Error>(Bytes::from_static(
                br#"{"schema":1,"report":"ok","logs":[]}"#,
            ))
        }));
        let response = app
            .oneshot(
                Request::post("/diagnostics")
                    .header(header::AUTHORIZATION, "Bearer good-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(delayed_body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        blob_server.abort();
    }

    #[tokio::test]
    async fn diagnostics_malformed_stream_body_returns_400() {
        let capture = BlobCapture::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let blob_app = Router::new()
            .route("/*path", put(fake_blob))
            .with_state(capture);
        let blob_server =
            tokio::spawn(async move { axum::serve(listener, blob_app).await.unwrap() });
        let store = crate::store::DiagnosticsStore::for_test(
            "acct",
            &base64::engine::general_purpose::STANDARD.encode(b"key"),
            "diagnostics",
            &endpoint,
        );
        let diagnostics = crate::diagnostics::Diagnostics::for_test(Some(store));
        let (app, _) = diagnostics_router_for_test(diagnostics).await;
        let broken_body = Body::from_stream(stream::once(async {
            Err::<Bytes, std::io::Error>(std::io::Error::other("broken body"))
        }));
        let response = app
            .oneshot(
                Request::post("/diagnostics")
                    .header(header::AUTHORIZATION, "Bearer good-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(broken_body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        blob_server.abort();
    }

    #[tokio::test]
    async fn diagnostics_are_rate_limited_to_three_uploads_per_minute_per_account() {
        let diagnostics = crate::diagnostics::Diagnostics::for_test(None);
        let (app, _) = diagnostics_router_for_test(diagnostics).await;

        for _ in 0..3 {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/diagnostics")
                        .header(header::AUTHORIZATION, "Bearer good-token")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"schema":1,"report":"ok","logs":[]}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }

        let fourth = app
            .oneshot(
                Request::post("/diagnostics")
                    .header(header::AUTHORIZATION, "Bearer good-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"schema":1,"report":"ok","logs":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(fourth.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn diagnostics_return_service_unavailable_when_storage_upload_fails() {
        let capture = BlobCapture::default();
        capture
            .status
            .store(StatusCode::SERVICE_UNAVAILABLE.as_u16(), Ordering::Relaxed);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let blob_app = Router::new()
            .route("/*path", put(fake_blob))
            .with_state(capture.clone());
        let blob_server =
            tokio::spawn(async move { axum::serve(listener, blob_app).await.unwrap() });
        let store = crate::store::DiagnosticsStore::for_test(
            "acct",
            &base64::engine::general_purpose::STANDARD.encode(b"key"),
            "diagnostics",
            &endpoint,
        );
        let diagnostics = crate::diagnostics::Diagnostics::for_test(Some(store));
        let (app, _) = diagnostics_router_for_test(diagnostics).await;

        let response = app
            .oneshot(
                Request::post("/diagnostics")
                    .header(header::AUTHORIZATION, "Bearer good-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"schema":1,"report":"ok","logs":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        blob_server.abort();
    }

    /// `/ws` deliberately accepts anonymous peers, so it would be easy to give
    /// `/presence` the same treatment. It must not have it: the answer is
    /// "which of *your* friends are live", and an anonymous caller returning an
    /// empty list is indistinguishable from every friend being offline.
    #[tokio::test]
    async fn presence_refuses_a_caller_without_a_valid_session() {
        let state = AppState {
            rooms: Default::default(),
            auth: Auth::new(None),
            social: crate::social::Social::new(None),
        };
        state
            .auth
            .insert_session_for_test(
                "good-token",
                crate::auth::Identity {
                    id: "me".into(),
                    name: "Me".into(),
                    avatar_url: None,
                },
            )
            .await;

        for authorization in [None, Some("Bearer expired"), Some("good-token")] {
            let mut request = Request::get("/presence?ids=friend");
            if let Some(value) = authorization {
                request = request.header(header::AUTHORIZATION, value);
            }
            let response = router(state.clone())
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "unauthenticated presence query was answered: {authorization:?}"
            );
        }

        let response = router(state.clone())
            .oneshot(
                Request::get("/presence?ids=friend")
                    .header(header::AUTHORIZATION, "Bearer good-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        assert_eq!(
            String::from_utf8(body.to_vec()).unwrap(),
            r#"{"friends":[{"id":"friend","state":"offline"}]}"#
        );
    }

    /// A friend list is unbounded but the room lock is shared with every
    /// signalling message, so one caller must not be able to hold it while the
    /// relay walks an arbitrarily long id list.
    #[tokio::test]
    async fn presence_rejects_a_query_longer_than_any_real_friend_list() {
        let state = AppState {
            rooms: Default::default(),
            auth: Auth::new(None),
            social: crate::social::Social::new(None),
        };
        state
            .auth
            .insert_session_for_test(
                "good-token",
                crate::auth::Identity {
                    id: "me".into(),
                    name: "Me".into(),
                    avatar_url: None,
                },
            )
            .await;

        let ids = vec!["1"; PRESENCE_QUERY_CAPACITY + 1].join(",");
        let response = router(state)
            .oneshot(
                Request::get(format!("/presence?ids={ids}"))
                    .header(header::AUTHORIZATION, "Bearer good-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn waited_presence_rejects_non_sha256_since_revisions() {
        let state = AppState {
            rooms: Default::default(),
            auth: Auth::new(None),
            social: crate::social::Social::new(None),
        };
        state
            .auth
            .insert_session_for_test(
                "good-token",
                crate::auth::Identity {
                    id: "me".into(),
                    name: "Me".into(),
                    avatar_url: None,
                },
            )
            .await;

        let response = router(state)
            .oneshot(
                Request::get("/presence?ids=1&wait=1&since=xyz")
                    .header(header::AUTHORIZATION, "Bearer good-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn wait_zero_keeps_the_legacy_presence_json_shape() {
        let state = AppState {
            rooms: Default::default(),
            auth: Auth::new(None),
            social: crate::social::Social::new(None),
        };
        state
            .auth
            .insert_session_for_test(
                "good-token",
                crate::auth::Identity {
                    id: "me".into(),
                    name: "Me".into(),
                    avatar_url: None,
                },
            )
            .await;

        let response = router(state)
            .oneshot(
                Request::get("/presence?ids=friend&wait=0&since=not-checked")
                    .header(header::AUTHORIZATION, "Bearer good-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        assert_eq!(
            String::from_utf8(body.to_vec()).unwrap(),
            r#"{"friends":[{"id":"friend","state":"offline"}]}"#
        );
    }

    #[tokio::test]
    async fn websocket_permit_is_held_until_the_connection_closes() {
        assert_eq!(CONNECTION_CAPACITY, 512);
        let connections = Arc::new(tokio::sync::Semaphore::new(1));
        let app = Router::new()
            .route("/ws", get(limited_ws_upgrade))
            .with_state((
                AppState {
                    rooms: Rooms::default(),
                    auth: Auth::new(None),
                    social: crate::social::Social::new(None),
                },
                connections.clone(),
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let relay = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = format!("ws://{address}/ws");

        let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        assert_eq!(connections.available_permits(), 0);
        let error = tokio_tungstenite::connect_async(&url).await.unwrap_err();
        let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
            panic!("capacity rejection was not an HTTP response: {error}");
        };
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(connections.available_permits(), 0);

        socket.close(None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while connections.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("connection permit was not released");
        assert_eq!(connections.available_permits(), 1);
        relay.abort();
    }

    #[tokio::test]
    async fn waited_presence_refresh_uses_queued_admission_after_wait() {
        let state = AppState {
            rooms: Default::default(),
            auth: Auth::new(None),
            social: crate::social::Social::new(None),
        };
        state
            .auth
            .insert_session_for_test(
                "good-token",
                crate::auth::Identity {
                    id: "me".into(),
                    name: "Me".into(),
                    avatar_url: None,
                },
            )
            .await;

        let app = router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let relay = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();

        let baseline = client
            .get(format!("http://{address}/presence?ids=friend&wait=1"))
            .bearer_auth("good-token")
            .send()
            .await
            .unwrap();
        assert_eq!(baseline.status(), StatusCode::OK);
        let baseline: Value = baseline.json().await.unwrap();
        let since = baseline
            .get("revision")
            .and_then(Value::as_str)
            .expect("waited presence response should carry a revision")
            .to_owned();

        let mut permits: Vec<_> = (0..31).map(|_| state.social.admit().unwrap()).collect();
        let waited = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .get(format!(
                        "http://{address}/presence?ids=friend&wait=1&since={since}"
                    ))
                    .bearer_auth("good-token")
                    .send()
                    .await
                    .unwrap()
            }
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !waited.is_finished(),
            "presence wait finished before entering long-poll"
        );

        let extra_permit = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(permit) = state.social.admit() {
                    break permit;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("waited presence did not release its initial request permit in time");
        permits.push(extra_permit);

        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(
            !waited.is_finished(),
            "waited presence returned immediately when post-wait refresh capacity was full"
        );

        drop(permits.pop());
        let response = tokio::time::timeout(Duration::from_secs(2), waited)
            .await
            .expect("waited presence did not resume after request capacity was released")
            .expect("waited presence task panicked");
        assert_eq!(response.status(), StatusCode::OK);

        drop(permits);
        relay.abort();
    }
}
