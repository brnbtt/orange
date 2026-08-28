//! Signalling: how two peers find each other and exchange SDP.
//!
//! The server is deliberately dumb. It knows nothing about video, holds no
//! media, and only relays JSON between the two members of a room. That is what
//! keeps hosting costs near zero - the actual stream goes peer to peer, and
//! this only carries a few kilobytes of handshake.
//!
//! A room is identified by a short code that the host shares. Possession of
//! the code is the only credential, which is the deliberate "no accounts"
//! tradeoff: anyone you give it to can watch, and can pass it on.

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

/// Messages exchanged between peers and the relay.
///
/// Anything carrying a `peer` field is routed: a host may be talking to several
/// viewers at once, so SDP and ICE must say which conversation they belong to.
/// Viewers do not know their own id - the relay stamps it on the way through.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Signal {
    /// Host -> server: open a room.
    Host,
    /// Server -> host: the room is open under this code.
    Hosting { code: String },
    /// Viewer -> server: join a room.
    Join { code: String },
    /// Server -> host: a viewer arrived, start negotiating with it.
    ViewerJoined { peer: String },
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
    pub fn to_text(&self) -> Result<Message> {
        Ok(Message::Text(serde_json::to_string(self)?))
    }

    /// Overwrite the routing id. Used by the relay so a viewer cannot claim to
    /// be a different peer.
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

type Tx = mpsc::UnboundedSender<Message>;

#[derive(Default)]
struct Room {
    host: Option<Tx>,
    /// Several viewers can watch one host simultaneously, each with its own
    /// peer connection.
    viewers: HashMap<String, Tx>,
}

type Rooms = Arc<Mutex<HashMap<String, Room>>>;

/// Identifier for one viewer's connection within a room.
fn generate_peer_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    format!("{:08x}", rng.gen::<u32>())
}

/// Run the relay. One process, no state beyond the live rooms.
pub async fn serve(addr: &str) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("could not bind {addr}"))?;
    println!("orange signalling server listening on {addr}");

    let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));

    while let Ok((stream, peer)) = listener.accept().await {
        let rooms = rooms.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, rooms).await {
                eprintln!("[signal] {peer} disconnected: {err}");
            }
        });
    }
    Ok(())
}

async fn handle_connection(stream: TcpStream, rooms: Rooms) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut source) = ws.split();

    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Which room this connection belongs to. Viewers also carry their peer id.
    let mut joined: Option<(String, Role)> = None;

    while let Some(msg) = source.next().await {
        let msg = msg?;
        let Message::Text(text) = msg else { continue };
        let signal: Signal = match serde_json::from_str(&text) {
            Ok(s) => s,
            Err(err) => {
                let _ = tx.send(
                    Signal::Error {
                        message: format!("bad message: {err}"),
                    }
                    .to_text()?,
                );
                continue;
            }
        };

        match signal {
            Signal::Host => {
                let code = generate_code();
                rooms.lock().await.insert(
                    code.clone(),
                    Room {
                        host: Some(tx.clone()),
                        viewers: HashMap::new(),
                    },
                );
                joined = Some((code.clone(), Role::Host));
                tx.send(Signal::Hosting { code }.to_text()?)?;
            }
            Signal::Join { code } => {
                let code = code.trim().to_ascii_uppercase();
                let peer = generate_peer_id();
                let mut rooms = rooms.lock().await;
                match rooms.get_mut(&code) {
                    Some(room) if room.host.is_some() => {
                        room.viewers.insert(peer.clone(), tx.clone());
                        joined = Some((code.clone(), Role::Viewer(peer.clone())));
                        // Only the host offers, so it must be told to start.
                        if let Some(host) = &room.host {
                            let _ = host.send(Signal::ViewerJoined { peer }.to_text()?);
                        }
                    }
                    _ => {
                        tx.send(
                            Signal::Error {
                                message: format!("no stream with code {code}"),
                            }
                            .to_text()?,
                        )?;
                    }
                }
            }
            // Everything else is relayed to the other party in the conversation.
            other => {
                let Some((code, role)) = &joined else {
                    bail!("message before joining a room");
                };
                let rooms = rooms.lock().await;
                let Some(room) = rooms.get(code) else { continue };

                match role {
                    // Host addresses a specific viewer.
                    Role::Host => {
                        if let Some(target) = other.peer_id().and_then(|id| room.viewers.get(id)) {
                            let _ = target.send(other.to_text()?);
                        }
                    }
                    // Viewer always talks to the host, stamped with its own id
                    // so it cannot impersonate another viewer.
                    Role::Viewer(peer) => {
                        if let Some(host) = &room.host {
                            let _ = host.send(other.with_peer(peer).to_text()?);
                        }
                    }
                }
            }
        }
    }

    // Clean up. A room without a host is useless, so it goes; a departing
    // viewer just frees its branch on the host.
    match joined {
        Some((code, Role::Host)) => {
            rooms.lock().await.remove(&code);
            println!("[signal] room {code} closed");
        }
        Some((code, Role::Viewer(peer))) => {
            let mut rooms = rooms.lock().await;
            if let Some(room) = rooms.get_mut(&code) {
                room.viewers.remove(&peer);
                if let Some(host) = &room.host {
                    let _ = host.send(Signal::ViewerLeft { peer }.to_text()?);
                }
            }
        }
        None => {}
    }
    Ok(())
}

enum Role {
    Host,
    Viewer(String),
}

/// Client side of the relay, shared by host and viewer.
pub struct SignalClient {
    pub outgoing: mpsc::UnboundedSender<Signal>,
    pub incoming: mpsc::UnboundedReceiver<Signal>,
}

/// rustls refuses to pick a crypto backend for you when more than one could
/// apply, and panics at first use rather than failing gracefully. Install one
/// explicitly, once, before any TLS happens.
fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub async fn connect(url: &str) -> Result<SignalClient> {
    install_crypto_provider();

    // `connect_async` handles ws:// directly; wss:// needs the TLS connector,
    // which is what Azure's ingress terminates on.
    let (ws, _) = tokio_tungstenite::connect_async_tls_with_config(url, None, false, None)
        .await
        .with_context(|| format!("could not reach signalling server at {url}"))?;
    let (mut sink, mut source) = ws.split();

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Signal>();
    let (in_tx, in_rx) = mpsc::unbounded_channel::<Signal>();

    tokio::spawn(async move {
        while let Some(signal) = out_rx.recv().await {
            match signal.to_text() {
                Ok(msg) => {
                    if sink.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(err) => eprintln!("[signal] encode failed: {err}"),
            }
        }
    });

    tokio::spawn(async move {
        while let Some(Ok(msg)) = source.next().await {
            if let Message::Text(text) = msg {
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
    })
}
