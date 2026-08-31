//! HTTP + WebSocket server for the relay.
//!
//! Both live on one port because Azure Container Apps exposes a single
//! ingress. `axum` handles the routing and the WebSocket upgrade.

use crate::auth::{Auth, DiscordConfig, PollResult};
use crate::diagnostics::{upload_diagnostics, DiagnosticsStorage, DIAGNOSTICS_LIMIT};
use crate::relay::{handle_peer, Rooms};
use anyhow::{Context, Result};
use axum::{
    extract::{
        ws::{WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, Query, State,
    },
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
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
fn page(title: &str, body: &str) -> String {
    let title = escape_html(title);
    let body = escape_html(body);
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
            diagnostics: DiagnosticsStorage::Disabled,
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
    async fn websocket_permit_is_held_until_the_connection_closes() {
        assert_eq!(CONNECTION_CAPACITY, 512);
        let connections = Arc::new(tokio::sync::Semaphore::new(1));
        let app = Router::new()
            .route("/ws", get(limited_ws_upgrade))
            .with_state((
                AppState {
                    rooms: Rooms::default(),
                    auth: Auth::new(None),
                    diagnostics: DiagnosticsStorage::Disabled,
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
