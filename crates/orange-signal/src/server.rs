//! HTTP + WebSocket server for the relay.
//!
//! Both live on one port because Azure Container Apps exposes a single
//! ingress. `axum` handles the routing and the WebSocket upgrade.

use crate::auth::{Auth, DiscordConfig, PollResult};
use crate::relay::{handle_peer, presence_for, Found, Rooms};
use anyhow::{Context, Result};
use axum::{
    extract::{
        ws::{WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
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
        .route("/presence", get(presence))
        .route("/ws", get(ws_upgrade))
        .with_state(state)
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

#[derive(Deserialize)]
struct PresenceParams {
    /// Comma-separated Discord ids.
    #[serde(default)]
    ids: String,
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
    let Some(token) = bearer_token(&headers) else {
        return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
    };
    let Some(identity) = app.auth.identify(token).await else {
        return (StatusCode::UNAUTHORIZED, "unknown or expired session").into_response();
    };

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

    let friends = presence_for(&app.rooms, &identity.id, &ids)
        .await
        .into_iter()
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
        .collect();
    Json(PresenceBody { friends }).into_response()
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
        body::{to_bytes, Body},
        http::{header, Request, StatusCode},
    };
    use tower::ServiceExt;

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

    /// `/ws` deliberately accepts anonymous peers, so it would be easy to give
    /// `/presence` the same treatment. It must not have it: the answer is
    /// "which of *your* friends are live", and an anonymous caller returning an
    /// empty list is indistinguishable from every friend being offline.
    #[tokio::test]
    async fn presence_refuses_a_caller_without_a_valid_session() {
        let state = AppState {
            rooms: Default::default(),
            auth: Auth::new(None),
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
    async fn websocket_permit_is_held_until_the_connection_closes() {
        assert_eq!(CONNECTION_CAPACITY, 512);
        let connections = Arc::new(tokio::sync::Semaphore::new(1));
        let app = Router::new()
            .route("/ws", get(limited_ws_upgrade))
            .with_state((
                AppState {
                    rooms: Rooms::default(),
                    auth: Auth::new(None),
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
}
