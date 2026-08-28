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
    Hosting { code: String },
    /// Viewer -> server: join a room.
    Join { code: String },
    /// Server -> viewer: whose stream this is.
    StreamInfo {
        #[serde(default)]
        host_name: Option<String>,
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

type Tx = mpsc::UnboundedSender<Message>;

#[derive(Default)]
pub struct Room {
    host: Option<Tx>,
    host_name: Option<String>,
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
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    let mut joined: Option<(String, Role)> = None;
    let mut identity: Option<Identity> = None;

    while let Some(msg) = source.next().await {
        let Message::Text(text) = msg? else { continue };
        let signal: Signal = match serde_json::from_str(&text) {
            Ok(s) => s,
            Err(err) => {
                let _ = tx.send(Message::Text(
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
                        let _ = tx.send(Message::Text(
                            Signal::Authenticated {
                                name: found.name.clone(),
                            }
                            .to_json(),
                        ));
                        identity = Some(found);
                    }
                    None => {
                        // Not fatal: anonymous peers are still allowed.
                        let _ = tx.send(Message::Text(
                            Signal::Error {
                                message: "session expired, continuing anonymously".into(),
                            }
                            .to_json(),
                        ));
                    }
                }
            }
            Signal::Host => {
                let code = generate_code();
                let name = identity.as_ref().map(|i| i.name.clone());
                rooms.lock().await.insert(
                    code.clone(),
                    Room {
                        host: Some(tx.clone()),
                        host_name: name,
                        viewers: HashMap::new(),
                    },
                );
                joined = Some((code.clone(), Role::Host));
                tx.send(Message::Text(Signal::Hosting { code }.to_json()))?;
            }
            Signal::Join { code } => {
                let code = code.trim().to_ascii_uppercase();
                let peer = generate_peer_id();
                let mut rooms = rooms.lock().await;
                match rooms.get_mut(&code) {
                    Some(room) if room.host.is_some() => {
                        room.viewers.insert(peer.clone(), tx.clone());
                        let host_name = room.host_name.clone();
                        joined = Some((code.clone(), Role::Viewer(peer.clone())));

                        let _ = tx.send(Message::Text(
                            Signal::StreamInfo { host_name }.to_json(),
                        ));
                        if let Some(host) = &room.host {
                            let _ = host.send(Message::Text(
                                Signal::ViewerJoined {
                                    peer,
                                    name: identity.as_ref().map(|i| i.name.clone()),
                                }
                                .to_json(),
                            ));
                        }
                    }
                    _ => {
                        tx.send(Message::Text(
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
                let Some(room) = rooms.get(code) else { continue };

                match role {
                    Role::Host => {
                        if let Some(target) = other.peer_id().and_then(|id| room.viewers.get(id)) {
                            let _ = target.send(Message::Text(other.to_json()));
                        }
                    }
                    Role::Viewer(peer) => {
                        if let Some(host) = &room.host {
                            let _ = host.send(Message::Text(other.with_peer(peer).to_json()));
                        }
                    }
                }
            }
        }
    }

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
                    let _ = host.send(Message::Text(Signal::ViewerLeft { peer }.to_json()));
                }
            }
        }
        None => {}
    }
    Ok(())
}

// --- client -----------------------------------------------------------------

/// Client side of the relay, shared by host and viewer.
pub struct SignalClient {
    pub outgoing: mpsc::UnboundedSender<Signal>,
    pub incoming: mpsc::UnboundedReceiver<Signal>,
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

    tokio::spawn(async move {
        while let Some(signal) = out_rx.recv().await {
            let text = tokio_tungstenite::tungstenite::Message::Text(signal.to_json());
            if sink.send(text).await.is_err() {
                break;
            }
        }
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
    })
}
