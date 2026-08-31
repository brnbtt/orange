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
mod diagnostics;
mod protocol;
mod server;

pub use auth::Identity;
pub use protocol::Signal;
pub use server::serve;

use anyhow::{bail, Result};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, Mutex};

const SIGNAL_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);
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
pub struct Room {
    host: Option<Tx>,
    host_name: Option<String>,
    diagnostic_session: String,
    viewers: HashMap<String, Tx>,
}

pub type Rooms = Arc<Mutex<HashMap<String, Room>>>;

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

// --- client -----------------------------------------------------------------

/// Client side of the relay, shared by host and viewer.
pub struct SignalClient {
    pub outgoing: mpsc::UnboundedSender<Signal>,
    pub incoming: mpsc::UnboundedReceiver<Signal>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    shutdown_done: Option<tokio::sync::oneshot::Receiver<()>>,
    tasks: ClientTasks,
}

struct ClientTasks {
    writer: Option<tokio::task::JoinHandle<()>>,
    reader: Option<tokio::task::JoinHandle<()>>,
}

impl ClientTasks {
    fn take(
        &mut self,
    ) -> (
        Option<tokio::task::JoinHandle<()>>,
        Option<tokio::task::JoinHandle<()>>,
    ) {
        (self.writer.take(), self.reader.take())
    }
}

impl Drop for ClientTasks {
    fn drop(&mut self) {
        if let Some(task) = &self.writer {
            task.abort();
        }
        if let Some(task) = &self.reader {
            task.abort();
        }
    }
}

struct StopOnDrop(tokio::sync::watch::Sender<bool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

async fn run_until_stopped(
    stop: &mut tokio::sync::watch::Receiver<bool>,
    operation: impl std::future::Future<Output = ()>,
) {
    tokio::select! {
        _ = operation => {}
        _ = stop.changed() => {}
    }
}

#[cfg(test)]
struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

#[cfg(test)]
impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(signal) = self.0.take() {
            let _ = signal.send(());
        }
    }
}

#[cfg(test)]
tokio::task_local! {
    static CLIENT_TASK_DROP_SIGNALS:
        std::cell::RefCell<Option<(DropSignal, DropSignal)>>;
}

impl SignalClient {
    /// Send a WebSocket close frame before the media process exits, then reap
    /// both socket tasks after a bounded grace period.
    pub async fn close(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(done) = self.shutdown_done.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), done).await;
        }
        let (writer, reader) = self.tasks.take();
        if let Some(task) = &writer {
            if !task.is_finished() {
                task.abort();
            }
        }
        if let Some(task) = &reader {
            if !task.is_finished() {
                task.abort();
            }
        }
        if let Some(task) = writer {
            let _ = task.await;
        }
        if let Some(task) = reader {
            let _ = task.await;
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
    let (stop_tx, mut writer_stop_rx) = tokio::sync::watch::channel(false);
    let mut reader_stop_rx = stop_tx.subscribe();
    let writer_stop_tx = stop_tx.clone();

    #[cfg(test)]
    let (writer_drop_signal, reader_drop_signal) = CLIENT_TASK_DROP_SIGNALS
        .try_with(|signals| signals.borrow_mut().take())
        .ok()
        .flatten()
        .map_or((None, None), |(writer, reader)| {
            (Some(writer), Some(reader))
        });

    let writer = tokio::spawn(async move {
        #[cfg(test)]
        let _drop_signal = writer_drop_signal;
        let _stop_on_drop = StopOnDrop(writer_stop_tx);
        run_until_stopped(&mut writer_stop_rx, async move {
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
        })
        .await;
        let _ = shutdown_done_tx.send(());
    });

    let reader = tokio::spawn(async move {
        #[cfg(test)]
        let _drop_signal = reader_drop_signal;
        let _stop_on_drop = StopOnDrop(stop_tx);
        run_until_stopped(&mut reader_stop_rx, async move {
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
        })
        .await;
    });

    Ok(SignalClient {
        outgoing: out_tx,
        incoming: in_rx,
        shutdown: Some(shutdown_tx),
        shutdown_done: Some(shutdown_done_rx),
        tasks: ClientTasks {
            writer: Some(writer),
            reader: Some(reader),
        },
    })
}

#[cfg(test)]
async fn start_unresponsive_close_relay() -> (
    String,
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Receiver<()>,
) {
    use axum::{extract::ws::WebSocketUpgrade, routing::get, Router};

    let (close_tx, close_rx) = tokio::sync::oneshot::channel();
    let close_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(close_tx)));
    let websocket = move |ws: WebSocketUpgrade| {
        let close_tx = close_tx.lock().expect("close signal poisoned").take();
        async move {
            ws.on_upgrade(|mut socket| async move {
                let mut close_tx = close_tx;
                while let Some(Ok(message)) = socket.recv().await {
                    if matches!(message, Message::Close(_)) {
                        if let Some(close_tx) = close_tx.take() {
                            let _ = close_tx.send(());
                        }
                        std::future::pending::<()>().await;
                    }
                }
            })
        }
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind test relay");
    let address = listener.local_addr().expect("test relay has no address");
    let relay = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/ws", get(websocket)))
            .await
            .expect("test relay failed");
    });
    (format!("ws://{address}/ws"), relay, close_rx)
}

#[cfg(test)]
async fn start_abrupt_eof_relay() -> (String, tokio::task::JoinHandle<()>) {
    use axum::{extract::ws::WebSocketUpgrade, response::IntoResponse, routing::get, Router};

    async fn websocket(ws: WebSocketUpgrade) -> impl IntoResponse {
        ws.on_upgrade(|socket| async move {
            drop(socket);
        })
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind test relay");
    let address = listener.local_addr().expect("test relay has no address");
    let relay = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/ws", get(websocket)))
            .await
            .expect("test relay failed");
    });
    (format!("ws://{address}/ws"), relay)
}

#[cfg(test)]
fn move_public_channels(
    client: SignalClient,
) -> (
    mpsc::UnboundedSender<Signal>,
    mpsc::UnboundedReceiver<Signal>,
) {
    (client.outgoing, client.incoming)
}

#[cfg(test)]
#[tokio::test]
async fn signal_client_public_channels_remain_movable() {
    let (url, relay, _close_observed) = start_unresponsive_close_relay().await;
    let (client, writer_dropped, reader_dropped) = connect_with_task_drop_signals(&url).await;

    let channels = move_public_channels(client);

    expect_socket_tasks_dropped(writer_dropped, reader_dropped).await;
    drop(channels);
    relay.abort();
}

#[cfg(test)]
#[tokio::test]
async fn stop_interrupts_an_in_flight_writer_operation() {
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (dropped_tx, mut dropped_rx) = tokio::sync::oneshot::channel();
    let writer = tokio::spawn(async move {
        run_until_stopped(&mut stop_rx, async move {
            let _drop_signal = DropSignal(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        })
        .await;
    });
    started_rx.await.expect("writer operation did not start");

    stop_tx.send_replace(true);
    writer.await.expect("writer task panicked");

    assert_eq!(dropped_rx.try_recv(), Ok(()));
}

#[cfg(test)]
#[tokio::test]
async fn task_panic_notifies_its_sibling() {
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    let sibling = tokio::spawn(async move {
        stop_rx.changed().await.expect("stop notification was lost");
        *stop_rx.borrow()
    });
    tokio::task::yield_now().await;
    let panic = std::panic::catch_unwind(|| {
        let _stop_on_drop = StopOnDrop(stop_tx);
        panic!("test panic");
    });

    assert!(panic.is_err());
    assert!(sibling.await.expect("sibling task panicked"));
}

#[cfg(test)]
#[tokio::test]
async fn task_cancellation_notifies_its_sibling() {
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _stop_on_drop = StopOnDrop(stop_tx);
        let _ = started_tx.send(());
        std::future::pending::<()>().await;
    });
    started_rx.await.expect("task did not start");

    task.abort();
    assert!(task
        .await
        .expect_err("task was not cancelled")
        .is_cancelled());

    stop_rx.changed().await.expect("stop notification was lost");
    assert!(*stop_rx.borrow());
}

#[cfg(test)]
async fn connect_with_task_drop_signals(
    url: &str,
) -> (
    SignalClient,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Receiver<()>,
) {
    let (writer_tx, writer_rx) = tokio::sync::oneshot::channel();
    let (reader_tx, reader_rx) = tokio::sync::oneshot::channel();
    let signals = (DropSignal(Some(writer_tx)), DropSignal(Some(reader_tx)));
    let client = CLIENT_TASK_DROP_SIGNALS
        .scope(std::cell::RefCell::new(Some(signals)), connect(url))
        .await
        .expect("failed to connect to test relay");
    (client, writer_rx, reader_rx)
}

#[cfg(test)]
async fn expect_socket_tasks_dropped(
    writer: tokio::sync::oneshot::Receiver<()>,
    reader: tokio::sync::oneshot::Receiver<()>,
) {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        writer.await.expect("writer drop signal was lost");
        reader.await.expect("reader drop signal was lost");
    })
    .await
    .expect("socket task resources were not dropped");
}

#[cfg(test)]
#[tokio::test]
async fn signal_client_close_reaps_both_socket_tasks() {
    let (url, relay, close_observed) = start_unresponsive_close_relay().await;
    let (client, mut writer_dropped, mut reader_dropped) =
        connect_with_task_drop_signals(&url).await;

    client.close().await;

    assert_eq!(writer_dropped.try_recv(), Ok(()));
    assert_eq!(reader_dropped.try_recv(), Ok(()));
    tokio::time::timeout(std::time::Duration::from_secs(1), close_observed)
        .await
        .expect("relay did not observe a close frame")
        .expect("close observation signal was lost");
    relay.abort();
}

#[cfg(test)]
#[tokio::test]
async fn dropping_signal_client_aborts_both_socket_tasks() {
    let (url, relay, _close_observed) = start_unresponsive_close_relay().await;
    let (client, writer_dropped, reader_dropped) = connect_with_task_drop_signals(&url).await;

    drop(client);

    expect_socket_tasks_dropped(writer_dropped, reader_dropped).await;
    relay.abort();
}

#[cfg(test)]
#[tokio::test]
async fn remote_close_releases_both_socket_tasks() {
    let (url, relay) = start_abrupt_eof_relay().await;
    let (client, writer_dropped, reader_dropped) = connect_with_task_drop_signals(&url).await;

    expect_socket_tasks_dropped(writer_dropped, reader_dropped).await;

    drop(client);
    relay.abort();
}

#[cfg(test)]
mod tests {
    use super::*;
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
