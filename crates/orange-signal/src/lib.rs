//! Signalling: how two peers find each other and exchange SDP.
//!
//! The relay is deliberately dumb about media. It matches peers by room code
//! and forwards a few kilobytes of handshake; video goes directly peer to
//! peer. Identity is layered on top: peers may authenticate with a Discord
//! session so that names appear instead of opaque ids.
//!
//! Note that identity is **not** an access control boundary here. Possession
//! of the room code still grants access; logging in only attaches a name to
//! whoever turns up.

pub mod auth;
mod server;

pub use auth::Identity;
pub use server::serve;

use anyhow::{bail, Result};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

const SIGNAL_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);

/// Messages exchanged between peers and the relay.
///
/// Anything carrying a `peer` field is routed: a host may be talking to several
/// viewers at once, so SDP and ICE must say which conversation they belong to.
/// Viewers do not know their own id - the relay stamps it on the way through.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Signal {
    /// Peer -> server, optional first message: prove who you are.
    Authenticate { session: String },
    /// Server -> peer: identity accepted.
    Authenticated { name: String },
    /// Host -> server: open a room.
    Host,
    /// Server -> host: the room is open under this code.
    Hosting {
        code: String,
        #[serde(default)]
        diagnostic_session: Option<String>,
    },
    /// Viewer -> server: join a room.
    Join { code: String },
    /// Server -> viewer: whose stream this is.
    StreamInfo {
        #[serde(default)]
        host_name: Option<String>,
        #[serde(default)]
        diagnostic_session: Option<String>,
    },
    /// Server -> host: a viewer arrived, start negotiating with it.
    ViewerJoined {
        peer: String,
        #[serde(default)]
        name: Option<String>,
    },
    /// Server -> host: a viewer disconnected, tear its branch down.
    ViewerLeft { peer: String },
    /// Either direction: session description.
    Sdp {
        #[serde(default)]
        peer: String,
        kind: String,
        sdp: String,
    },
    /// Either direction: ICE candidate.
    Ice {
        #[serde(default)]
        peer: String,
        mline: u32,
        candidate: String,
    },
    /// Server -> peer: something went wrong.
    Error { message: String },
}

impl Signal {
    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Overwrite the routing id, so a viewer cannot claim to be another peer.
    fn with_peer(self, id: &str) -> Self {
        match self {
            Signal::Sdp { kind, sdp, .. } => Signal::Sdp {
                peer: id.to_string(),
                kind,
                sdp,
            },
            Signal::Ice {
                mline, candidate, ..
            } => Signal::Ice {
                peer: id.to_string(),
                mline,
                candidate,
            },
            other => other,
        }
    }

    fn peer_id(&self) -> Option<&str> {
        match self {
            Signal::Sdp { peer, .. } | Signal::Ice { peer, .. } => Some(peer),
            _ => None,
        }
    }
}

/// Human-friendly room code. Avoids characters that are easy to misread aloud
/// (0/O, 1/I), because these get shared over voice chat.
fn generate_code() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    let pick = |rng: &mut rand::rngs::ThreadRng| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char;
    let a: String = (0..3).map(|_| pick(&mut rng)).collect();
    let b: String = (0..3).map(|_| pick(&mut rng)).collect();
    format!("{a}-{b}")
}

fn generate_peer_id() -> String {
    use rand::Rng;
    format!("{:08x}", rand::thread_rng().gen::<u32>())
}

fn generate_diagnostic_session() -> String {
    use rand::Rng;
    format!("{:032x}", rand::thread_rng().gen::<u128>())
}

const OUTBOUND_QUEUE_CAPACITY: usize = 64;

#[derive(Clone)]
struct Tx {
    messages: mpsc::Sender<Message>,
    disconnect: tokio::sync::watch::Sender<bool>,
}

impl Tx {
    fn try_send(&self, message: Message) -> Result<(), mpsc::error::TrySendError<Message>> {
        let result = self.messages.try_send(message);
        if result.is_err() {
            self.disconnect.send_replace(true);
        }
        result
    }
}

#[derive(Default)]
pub struct Room {
    host: Option<Tx>,
    host_name: Option<String>,
    diagnostic_session: String,
    viewers: HashMap<String, Tx>,
}

pub type Rooms = Arc<Mutex<HashMap<String, Room>>>;

enum Role {
    Host,
    Viewer(String),
}

/// Serve one connected peer for its lifetime.
pub async fn handle_peer(socket: WebSocket, rooms: Rooms, auth: auth::Auth) -> Result<()> {
    let (mut sink, mut source) = socket.split();
    let (messages, mut rx) = mpsc::channel::<Message>(OUTBOUND_QUEUE_CAPACITY);
    let (disconnect, mut disconnect_rx) = tokio::sync::watch::channel(false);
    let tx = Tx {
        messages,
        disconnect,
    };

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    let mut joined: Option<(String, Role)> = None;
    let mut identity: Option<Identity> = None;

    let session_result: Result<()> = async {
        loop {
            let msg = tokio::select! {
                changed = disconnect_rx.changed() => {
                    if changed.is_err() || *disconnect_rx.borrow() {
                        break;
                    }
                    continue;
                }
                msg = source.next() => {
                    let Some(msg) = msg else { break };
                    msg
                }
            };
            let Message::Text(text) = msg? else { continue };
            let signal: Signal = match serde_json::from_str(&text) {
                Ok(s) => s,
                Err(err) => {
                    let _ = tx.try_send(Message::Text(
                        Signal::Error {
                            message: format!("bad message: {err}"),
                        }
                        .to_json(),
                    ));
                    continue;
                }
            };

            match signal {
                Signal::Authenticate { session } => {
                    match auth.identify(&session).await {
                        Some(found) => {
                            let _ = tx.try_send(Message::Text(
                                Signal::Authenticated {
                                    name: found.name.clone(),
                                }
                                .to_json(),
                            ));
                            identity = Some(found);
                        }
                        None => {
                            // Identity is optional. An expired token must not
                            // prevent older clients from hosting or watching.
                        }
                    }
                }
                Signal::Host => {
                    if joined.is_some() {
                        bail!("peer attempted to change signalling role");
                    }
                    let code = generate_code();
                    let diagnostic_session = generate_diagnostic_session();
                    let name = identity.as_ref().map(|i| i.name.clone());
                    rooms.lock().await.insert(
                        code.clone(),
                        Room {
                            host: Some(tx.clone()),
                            host_name: name,
                            diagnostic_session: diagnostic_session.clone(),
                            viewers: HashMap::new(),
                        },
                    );
                    joined = Some((code.clone(), Role::Host));
                    tx.try_send(Message::Text(
                        Signal::Hosting {
                            code,
                            diagnostic_session: Some(diagnostic_session),
                        }
                        .to_json(),
                    ))?;
                }
                Signal::Join { code } => {
                    if joined.is_some() {
                        bail!("peer attempted to change signalling role");
                    }
                    let code = code.trim().to_ascii_uppercase();
                    let peer = generate_peer_id();
                    let mut rooms = rooms.lock().await;
                    match rooms.get_mut(&code) {
                        Some(room) if room.host.is_some() => {
                            room.viewers.insert(peer.clone(), tx.clone());
                            let host_name = room.host_name.clone();
                            let diagnostic_session = room.diagnostic_session.clone();
                            joined = Some((code.clone(), Role::Viewer(peer.clone())));

                            let _ = tx.try_send(Message::Text(
                                Signal::StreamInfo {
                                    host_name,
                                    diagnostic_session: Some(diagnostic_session),
                                }
                                .to_json(),
                            ));
                            if let Some(host) = &room.host {
                                let _ = host.try_send(Message::Text(
                                    Signal::ViewerJoined {
                                        peer,
                                        name: identity.as_ref().map(|i| i.name.clone()),
                                    }
                                    .to_json(),
                                ));
                            }
                        }
                        _ => {
                            tx.try_send(Message::Text(
                                Signal::Error {
                                    message: format!("no stream with code {code}"),
                                }
                                .to_json(),
                            ))?;
                        }
                    }
                }
                other => {
                    let Some((code, role)) = &joined else {
                        bail!("message before joining a room");
                    };
                    let rooms = rooms.lock().await;
                    let Some(room) = rooms.get(code) else {
                        continue;
                    };

                    match role {
                        Role::Host => {
                            if let Some(target) =
                                other.peer_id().and_then(|id| room.viewers.get(id))
                            {
                                let _ = target.try_send(Message::Text(other.to_json()));
                            }
                        }
                        Role::Viewer(peer) => {
                            if let Some(host) = &room.host {
                                let _ =
                                    host.try_send(Message::Text(other.with_peer(peer).to_json()));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
    .await;

    match joined {
        Some((code, Role::Host)) => {
            let room = rooms.lock().await.remove(&code);
            if let Some(room) = room {
                for viewer in room.viewers.into_values() {
                    let _ = viewer.try_send(Message::Text(
                        Signal::Error {
                            message: "The stream ended".into(),
                        }
                        .to_json(),
                    ));
                }
            }
            println!("[signal] room {code} closed");
        }
        Some((code, Role::Viewer(peer))) => {
            let mut rooms = rooms.lock().await;
            if let Some(room) = rooms.get_mut(&code) {
                room.viewers.remove(&peer);
                if let Some(host) = &room.host {
                    let _ = host.try_send(Message::Text(Signal::ViewerLeft { peer }.to_json()));
                }
            }
        }
        None => {}
    }
    stop_writer(writer).await;
    session_result
}

async fn stop_writer(writer: tokio::task::JoinHandle<()>) {
    writer.abort();
    let _ = writer.await;
}

// --- client -----------------------------------------------------------------

/// Client side of the relay, shared by host and viewer.
pub struct SignalClient {
    pub outgoing: mpsc::UnboundedSender<Signal>,
    pub incoming: mpsc::UnboundedReceiver<Signal>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    shutdown_done: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl SignalClient {
    /// Send a WebSocket close frame before the media process exits. Waiting
    /// only for the writer keeps shutdown prompt while ensuring the relay can
    /// remove viewer state immediately instead of waiting for a TCP timeout.
    pub async fn close(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(done) = self.shutdown_done.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), done).await;
        }
    }
}

/// rustls refuses to pick a crypto backend when more than one could apply, and
/// panics at first use rather than failing gracefully. Install one explicitly.
fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub async fn connect(url: &str) -> Result<SignalClient> {
    install_crypto_provider();

    let (ws, _) = tokio_tungstenite::connect_async_tls_with_config(url, None, false, None)
        .await
        .map_err(|e| anyhow::anyhow!("could not reach signalling server at {url}: {e}"))?;
    let (mut sink, mut source) = ws.split();

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Signal>();
    let (in_tx, in_rx) = mpsc::unbounded_channel::<Signal>();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let (shutdown_done_tx, shutdown_done_rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let mut heartbeat = tokio::time::interval(SIGNAL_HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        heartbeat.tick().await;
        loop {
            tokio::select! {
                signal = out_rx.recv() => {
                    let Some(signal) = signal else { break };
                    let text = tokio_tungstenite::tungstenite::Message::Text(signal.to_json());
                    if sink.send(text).await.is_err() {
                        break;
                    }
                }
                _ = &mut shutdown_rx => {
                    let _ = sink
                        .send(tokio_tungstenite::tungstenite::Message::Close(None))
                        .await;
                    break;
                }
                _ = heartbeat.tick() => {
                    if sink
                        .send(tokio_tungstenite::tungstenite::Message::Ping(Vec::new()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = shutdown_done_tx.send(());
    });

    tokio::spawn(async move {
        while let Some(Ok(msg)) = source.next().await {
            if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
                match serde_json::from_str::<Signal>(&text) {
                    Ok(signal) => {
                        if in_tx.send(signal).is_err() {
                            break;
                        }
                    }
                    Err(err) => eprintln!("[signal] decode failed: {err}"),
                }
            }
        }
    });

    Ok(SignalClient {
        outgoing: out_tx,
        incoming: in_rx,
        shutdown: Some(shutdown_tx),
        shutdown_done: Some(shutdown_done_rx),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::time::Duration;

    struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(signal) = self.0.take() {
                let _ = signal.send(());
            }
        }
    }

    #[test]
    fn relay_outbound_queue_is_bounded() {
        let (messages, _rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let (disconnect, disconnect_rx) = tokio::sync::watch::channel(false);
        let tx = Tx {
            messages,
            disconnect,
        };
        for _ in 0..OUTBOUND_QUEUE_CAPACITY {
            tx.try_send(Message::Text("signal".into())).unwrap();
        }
        assert!(matches!(
            tx.try_send(Message::Text("overflow".into())),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        assert!(*disconnect_rx.borrow());
    }

    #[tokio::test]
    async fn stopping_a_peer_drops_its_blocked_writer_task() {
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let writer = tokio::spawn(async move {
            let _signal = DropSignal(Some(dropped_tx));
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;

        stop_writer(writer).await;

        tokio::time::timeout(Duration::from_secs(1), dropped_rx)
            .await
            .expect("writer resources were not dropped")
            .expect("writer drop signal was lost");
    }

    #[tokio::test]
    async fn repeated_host_command_cleans_the_original_room() {
        let rooms = Rooms::default();
        let app = server::router(server::AppState {
            rooms: rooms.clone(),
            auth: auth::Auth::new(None),
            diagnostics: server::DiagnosticsStorage::Disabled,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let relay = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = format!("ws://{address}/ws");
        let (mut host, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        host.send(tokio_tungstenite::tungstenite::Message::Text(
            Signal::Host.to_json(),
        ))
        .await
        .unwrap();
        assert!(matches!(
            receive_signal(&mut host).await,
            Signal::Hosting { .. }
        ));
        host.send(tokio_tungstenite::tungstenite::Message::Text(
            Signal::Host.to_json(),
        ))
        .await
        .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), host.next()).await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !rooms.lock().await.is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(rooms.lock().await.is_empty());
        relay.abort();
    }

    async fn receive_signal(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> Signal {
        let message = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("timed out waiting for relay")
            .expect("relay closed unexpectedly")
            .expect("websocket read failed");
        let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
            panic!("expected text signal");
        };
        serde_json::from_str(&text).expect("relay sent malformed signal")
    }

    #[tokio::test]
    async fn abrupt_viewer_disconnect_notifies_host() {
        let rooms = Rooms::default();
        let app = server::router(server::AppState {
            rooms: rooms.clone(),
            auth: auth::Auth::new(None),
            diagnostics: server::DiagnosticsStorage::Disabled,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let relay = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = format!("ws://{address}/ws");

        let (mut host, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        host.send(tokio_tungstenite::tungstenite::Message::Text(
            Signal::Host.to_json(),
        ))
        .await
        .unwrap();
        let Signal::Hosting { code, .. } = receive_signal(&mut host).await else {
            panic!("host did not receive a room code");
        };

        let (mut viewer, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        viewer
            .send(tokio_tungstenite::tungstenite::Message::Text(
                Signal::Join { code }.to_json(),
            ))
            .await
            .unwrap();
        let _ = receive_signal(&mut viewer).await;
        let Signal::ViewerJoined { peer, .. } = receive_signal(&mut host).await else {
            panic!("host did not receive viewer join");
        };

        drop(viewer);

        let Signal::ViewerLeft { peer: departed } = receive_signal(&mut host).await else {
            panic!("host did not receive viewer departure");
        };
        assert_eq!(departed, peer);
        assert!(rooms
            .lock()
            .await
            .values()
            .all(|room| room.viewers.is_empty()));

        relay.abort();
    }

    #[tokio::test]
    async fn expired_authentication_still_allows_anonymous_hosting() {
        let rooms = Rooms::default();
        let app = server::router(server::AppState {
            rooms,
            auth: auth::Auth::new(None),
            diagnostics: server::DiagnosticsStorage::Disabled,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let relay = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = format!("ws://{address}/ws");

        let (mut host, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        host.send(tokio_tungstenite::tungstenite::Message::Text(
            Signal::Authenticate {
                session: "expired".into(),
            }
            .to_json(),
        ))
        .await
        .unwrap();
        host.send(tokio_tungstenite::tungstenite::Message::Text(
            Signal::Host.to_json(),
        ))
        .await
        .unwrap();

        assert!(matches!(
            receive_signal(&mut host).await,
            Signal::Hosting { .. }
        ));

        relay.abort();
    }

    #[tokio::test]
    async fn host_and_viewer_receive_same_opaque_diagnostic_session() {
        let rooms = Rooms::default();
        let app = server::router(server::AppState {
            rooms,
            auth: auth::Auth::new(None),
            diagnostics: server::DiagnosticsStorage::Disabled,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let relay = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = format!("ws://{address}/ws");

        let (mut host, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        host.send(tokio_tungstenite::tungstenite::Message::Text(
            Signal::Host.to_json(),
        ))
        .await
        .unwrap();
        let Signal::Hosting {
            code,
            diagnostic_session: host_session,
        } = receive_signal(&mut host).await
        else {
            panic!("host did not receive hosting details");
        };
        let host_session = host_session.expect("host did not receive a diagnostic session");

        let (mut viewer, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        viewer
            .send(tokio_tungstenite::tungstenite::Message::Text(
                Signal::Join { code: code.clone() }.to_json(),
            ))
            .await
            .unwrap();
        let Signal::StreamInfo {
            diagnostic_session: viewer_session,
            ..
        } = receive_signal(&mut viewer).await
        else {
            panic!("viewer did not receive stream details");
        };

        assert!(!host_session.is_empty());
        assert_ne!(host_session, code);
        assert!(!host_session.contains(&code));
        assert_eq!(viewer_session.as_deref(), Some(host_session.as_str()));

        relay.abort();
    }

    #[test]
    fn legacy_session_messages_deserialize_without_diagnostic_session() {
        let hosting: Signal =
            serde_json::from_str(r#"{"type":"hosting","code":"ABC-234"}"#).unwrap();
        let Signal::Hosting {
            diagnostic_session: hosting_session,
            ..
        } = hosting
        else {
            panic!("expected hosting signal");
        };
        assert_eq!(hosting_session, None);

        let stream_info: Signal =
            serde_json::from_str(r#"{"type":"streaminfo","host_name":null}"#).unwrap();
        let Signal::StreamInfo {
            diagnostic_session: viewer_session,
            ..
        } = stream_info
        else {
            panic!("expected stream info signal");
        };
        assert_eq!(viewer_session, None);
    }

    #[tokio::test]
    async fn abrupt_host_disconnect_notifies_existing_viewers() {
        let rooms = Rooms::default();
        let app = server::router(server::AppState {
            rooms: rooms.clone(),
            auth: auth::Auth::new(None),
            diagnostics: server::DiagnosticsStorage::Disabled,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let relay = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = format!("ws://{address}/ws");

        let (mut host, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        host.send(tokio_tungstenite::tungstenite::Message::Text(
            Signal::Host.to_json(),
        ))
        .await
        .unwrap();
        let Signal::Hosting { code, .. } = receive_signal(&mut host).await else {
            panic!("host did not receive a room code");
        };
        let (mut viewer, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        viewer
            .send(tokio_tungstenite::tungstenite::Message::Text(
                Signal::Join { code }.to_json(),
            ))
            .await
            .unwrap();
        let _ = receive_signal(&mut viewer).await;
        let _ = receive_signal(&mut host).await;

        drop(host);

        let Signal::Error { message } = receive_signal(&mut viewer).await else {
            panic!("viewer did not receive stream termination");
        };
        assert_eq!(message, "The stream ended");
        assert!(rooms.lock().await.is_empty());

        relay.abort();
    }
}
