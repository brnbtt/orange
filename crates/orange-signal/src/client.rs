use crate::Signal;
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;

const SIGNAL_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);

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
pub(super) struct DropSignal(pub(super) Option<tokio::sync::oneshot::Sender<()>>);

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
                    if matches!(message, axum::extract::ws::Message::Close(_)) {
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
