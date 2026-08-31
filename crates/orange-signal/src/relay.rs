use crate::{
    auth::{self, Identity},
    Signal,
};
#[cfg(test)]
use crate::{diagnostics, server};
use anyhow::{bail, Result};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, Mutex};

const INBOUND_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);
const INBOUND_MESSAGE_LIMIT: usize = 256;
const ROOM_VIEWER_CAPACITY: usize = 16;

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

#[cfg(test)]
#[test]
fn hosting_collision_preserves_live_room_and_uses_next_code() {
    let (messages, _rx) = mpsc::channel(1);
    let (disconnect, _disconnect_rx) = tokio::sync::watch::channel(false);
    let mut rooms = HashMap::from([(
        "ABC-234".to_string(),
        Room {
            host: Some(Tx {
                messages,
                disconnect,
            }),
            host_name: Some("existing host".into()),
            diagnostic_session: "existing session".into(),
            viewers: HashMap::new(),
        },
    )]);
    let mut codes = ["ABC-234", "XYZ-789"].into_iter();

    let code = insert_room_with_code(&mut rooms, Room::default(), || {
        codes.next().expect("code generator exhausted").to_string()
    });

    assert_eq!(code, "XYZ-789");
    assert_eq!(rooms.len(), 2);
    let existing = rooms.get("ABC-234").expect("existing room was removed");
    assert!(existing.host.is_some());
    assert_eq!(existing.host_name.as_deref(), Some("existing host"));
    assert_eq!(existing.diagnostic_session, "existing session");
    assert!(rooms.contains_key("XYZ-789"));
}

const OUTBOUND_QUEUE_CAPACITY: usize = 64;

struct FixedWindow {
    started: Instant,
    count: usize,
}

impl FixedWindow {
    fn allow(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.started) >= INBOUND_WINDOW {
            self.started = now;
            self.count = 0;
        }
        if self.count >= INBOUND_MESSAGE_LIMIT {
            return false;
        }
        self.count += 1;
        true
    }
}

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

    fn try_send_to_host(&self, message: Message) -> Result<(), mpsc::error::TrySendError<Message>> {
        self.messages.try_send(message)
    }
}

#[derive(Default)]
pub(crate) struct Room {
    host: Option<Tx>,
    host_name: Option<String>,
    diagnostic_session: String,
    viewers: HashMap<String, Tx>,
}

pub(crate) type Rooms = Arc<Mutex<HashMap<String, Room>>>;

fn insert_room_with_code(
    rooms: &mut HashMap<String, Room>,
    room: Room,
    mut generate: impl FnMut() -> String,
) -> String {
    loop {
        if let std::collections::hash_map::Entry::Vacant(entry) = rooms.entry(generate()) {
            let code = entry.key().clone();
            entry.insert(room);
            return code;
        }
    }
}

enum Role {
    Host,
    Viewer(String),
}

/// Serve one connected peer for its lifetime.
pub(crate) async fn handle_peer(socket: WebSocket, rooms: Rooms, auth: auth::Auth) -> Result<()> {
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
    let mut inbound_budget = FixedWindow {
        started: Instant::now(),
        count: 0,
    };

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
            if !inbound_budget.allow(Instant::now()) {
                bail!("signalling rate limit exceeded");
            }
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
                    let diagnostic_session = generate_diagnostic_session();
                    let name = identity.as_ref().map(|i| i.name.clone());
                    let code = {
                        let mut rooms = rooms.lock().await;
                        insert_room_with_code(
                            &mut rooms,
                            Room {
                                host: Some(tx.clone()),
                                host_name: name,
                                diagnostic_session: diagnostic_session.clone(),
                                viewers: HashMap::new(),
                            },
                            generate_code,
                        )
                    };
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
                    let mut rooms = rooms.lock().await;
                    match rooms.get_mut(&code) {
                        Some(room)
                            if room.host.is_some() && room.viewers.len() < ROOM_VIEWER_CAPACITY =>
                        {
                            let peer = loop {
                                let peer = generate_peer_id();
                                if !room.viewers.contains_key(&peer) {
                                    break peer;
                                }
                            };
                            room.viewers.insert(peer.clone(), tx.clone());
                            let host_name = room.host_name.clone();
                            let diagnostic_session = room.diagnostic_session.clone();
                            joined = Some((code.clone(), Role::Viewer(peer.clone())));

                            if let Some(host) = &room.host {
                                host.try_send_to_host(Message::Text(
                                    Signal::ViewerJoined {
                                        peer,
                                        name: identity.as_ref().map(|i| i.name.clone()),
                                    }
                                    .to_json(),
                                ))?;
                            }
                            let _ = tx.try_send(Message::Text(
                                Signal::StreamInfo {
                                    host_name,
                                    diagnostic_session: Some(diagnostic_session),
                                }
                                .to_json(),
                            ));
                        }
                        Some(room) if room.host.is_some() => {
                            tx.try_send(Message::Text(
                                Signal::Error {
                                    message: "stream is full".into(),
                                }
                                .to_json(),
                            ))?;
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
                                host.try_send_to_host(Message::Text(
                                    other.with_peer(peer).to_json(),
                                ))?;
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
            println!("[signal] room closed");
        }
        Some((code, Role::Viewer(peer))) => {
            let mut rooms = rooms.lock().await;
            if let Some(room) = rooms.get_mut(&code) {
                room.viewers.remove(&peer);
                if let Some(host) = &room.host {
                    let _ =
                        host.try_send_to_host(Message::Text(Signal::ViewerLeft { peer }.to_json()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DropSignal;
    use futures_util::{SinkExt, StreamExt};
    use std::time::Duration;

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

    #[test]
    fn inbound_budget_allows_256_messages_and_resets_at_ten_seconds() {
        let started = std::time::Instant::now();
        let mut budget = FixedWindow { started, count: 0 };

        for _ in 0..256 {
            assert!(budget.allow(started));
        }
        assert!(!budget.allow(started));
        assert!(budget.allow(started + Duration::from_secs(10)));
    }

    #[test]
    fn host_forwarding_queue_errors_do_not_disconnect_host() {
        let (messages, _rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        for _ in 0..OUTBOUND_QUEUE_CAPACITY {
            messages.try_send(Message::Text("signal".into())).unwrap();
        }
        let (disconnect, disconnect_rx) = tokio::sync::watch::channel(false);
        let tx = Tx {
            messages,
            disconnect,
        };
        assert!(matches!(
            tx.try_send_to_host(Message::Text("overflow".into())),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        assert!(!*disconnect_rx.borrow());

        let (messages, rx) = mpsc::channel(1);
        drop(rx);
        let (disconnect, disconnect_rx) = tokio::sync::watch::channel(false);
        let tx = Tx {
            messages,
            disconnect,
        };
        assert!(matches!(
            tx.try_send_to_host(Message::Text("closed".into())),
            Err(mpsc::error::TrySendError::Closed(_))
        ));
        assert!(!*disconnect_rx.borrow());
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
            diagnostics: diagnostics::DiagnosticsStorage::Disabled,
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

    #[tokio::test]
    async fn sixty_four_sequential_host_connections_leave_no_rooms() {
        let rooms = Rooms::default();
        let app = server::router(server::AppState {
            rooms: rooms.clone(),
            auth: auth::Auth::new(None),
            diagnostics: diagnostics::DiagnosticsStorage::Disabled,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let relay = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = format!("ws://{address}/ws");

        tokio::time::timeout(Duration::from_secs(10), async {
            for cycle in 0..64 {
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
                host.close(None).await.unwrap();

                while !rooms.lock().await.is_empty() {
                    tokio::task::yield_now().await;
                }
                assert!(
                    rooms.lock().await.is_empty(),
                    "room remained after cycle {cycle}"
                );
            }
        })
        .await
        .expect("64 host create/close cycles timed out");

        assert!(rooms.lock().await.is_empty());
        relay.abort();
    }

    async fn receive_signal(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> Signal {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .expect("timed out waiting for relay")
                .expect("relay closed unexpectedly")
                .expect("websocket read failed");
            if let tokio_tungstenite::tungstenite::Message::Text(text) = message {
                return serde_json::from_str(&text).expect("relay sent malformed signal");
            }
        }
    }

    #[tokio::test]
    async fn viewer_rate_limit_counts_only_text_and_cleans_up_its_role() {
        let rooms = Rooms::default();
        let app = server::router(server::AppState {
            rooms: rooms.clone(),
            auth: auth::Auth::new(None),
            diagnostics: diagnostics::DiagnosticsStorage::Disabled,
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
        .expect("failed to send host command");
        let Signal::Hosting { code, .. } = receive_signal(&mut host).await else {
            panic!("host did not receive a room code");
        };

        let (mut viewer, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        viewer
            .send(tokio_tungstenite::tungstenite::Message::Text(
                Signal::Join { code: code.clone() }.to_json(),
            ))
            .await
            .expect("failed to send viewer join");
        let _ = receive_signal(&mut viewer).await;
        let Signal::ViewerJoined { peer, .. } = receive_signal(&mut host).await else {
            panic!("host did not receive viewer join");
        };

        viewer
            .send(tokio_tungstenite::tungstenite::Message::Ping(vec![]))
            .await
            .expect("failed to send viewer ping");
        viewer
            .send(tokio_tungstenite::tungstenite::Message::Binary(vec![0]))
            .await
            .expect("failed to send viewer binary frame");

        let invalid_auth = Signal::Authenticate {
            session: "invalid".into(),
        }
        .to_json();
        for _ in 0..254 {
            viewer
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    invalid_auth.clone(),
                ))
                .await
                .expect("failed to send invalid authentication");
        }
        viewer
            .send(tokio_tungstenite::tungstenite::Message::Text("{".into()))
            .await
            .expect("failed to send malformed message 256");
        let Signal::Error { message } = receive_signal(&mut viewer).await else {
            panic!("malformed text did not return an error");
        };
        assert!(message.starts_with("bad message:"));

        viewer
            .send(tokio_tungstenite::tungstenite::Message::Text(invalid_auth))
            .await
            .expect("failed to send rate-limit trigger message 257");

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match viewer.next().await {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))
                    | Some(Err(_))
                    | None => break,
                    Some(Ok(_)) => {}
                }
            }
        })
        .await
        .expect("rate-limited viewer stayed connected");

        let Signal::ViewerLeft { peer: departed } = receive_signal(&mut host).await else {
            panic!("host did not receive rate-limited viewer departure");
        };
        assert_eq!(departed, peer);
        let rooms = rooms.lock().await;
        assert_eq!(rooms.len(), 1);
        assert!(rooms.get(&code).unwrap().viewers.is_empty());

        relay.abort();
    }

    #[tokio::test]
    async fn viewer_can_retry_after_a_full_room_on_the_same_socket() {
        let rooms = Rooms::default();
        let (messages, _rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let (disconnect, _disconnect_rx) = tokio::sync::watch::channel(false);
        let dummy = Tx {
            messages,
            disconnect,
        };
        rooms.lock().await.extend([
            (
                "FULL".into(),
                Room {
                    host: Some(dummy.clone()),
                    diagnostic_session: "full-session".into(),
                    viewers: (0..16)
                        .map(|peer| (peer.to_string(), dummy.clone()))
                        .collect(),
                    ..Room::default()
                },
            ),
            (
                "OPEN".into(),
                Room {
                    host: Some(dummy),
                    diagnostic_session: "open-session".into(),
                    ..Room::default()
                },
            ),
        ]);
        let app = server::router(server::AppState {
            rooms: rooms.clone(),
            auth: auth::Auth::new(None),
            diagnostics: diagnostics::DiagnosticsStorage::Disabled,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let relay = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = format!("ws://{address}/ws");
        let (mut viewer, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        viewer
            .send(tokio_tungstenite::tungstenite::Message::Text(
                Signal::Join {
                    code: "FULL".into(),
                }
                .to_json(),
            ))
            .await
            .expect("failed to send full-room join");
        let Signal::Error { message } = receive_signal(&mut viewer).await else {
            panic!("full room did not return an error");
        };
        assert_eq!(message, "stream is full");

        viewer
            .send(tokio_tungstenite::tungstenite::Message::Text(
                Signal::Join {
                    code: "OPEN".into(),
                }
                .to_json(),
            ))
            .await
            .expect("failed to send open-room retry");
        assert!(matches!(
            receive_signal(&mut viewer).await,
            Signal::StreamInfo { .. }
        ));

        let rooms = rooms.lock().await;
        assert_eq!(rooms.get("FULL").unwrap().viewers.len(), 16);
        assert_eq!(rooms.get("OPEN").unwrap().viewers.len(), 1);

        relay.abort();
    }

    #[tokio::test]
    async fn abrupt_viewer_disconnect_notifies_host() {
        let rooms = Rooms::default();
        let app = server::router(server::AppState {
            rooms: rooms.clone(),
            auth: auth::Auth::new(None),
            diagnostics: diagnostics::DiagnosticsStorage::Disabled,
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
            diagnostics: diagnostics::DiagnosticsStorage::Disabled,
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
            diagnostics: diagnostics::DiagnosticsStorage::Disabled,
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

    #[tokio::test]
    async fn abrupt_host_disconnect_notifies_existing_viewers() {
        let rooms = Rooms::default();
        let app = server::router(server::AppState {
            rooms: rooms.clone(),
            auth: auth::Auth::new(None),
            diagnostics: diagnostics::DiagnosticsStorage::Disabled,
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
