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
mod client;
mod diagnostics;
mod protocol;
mod relay;
mod server;

pub use auth::Identity;
pub use client::{connect, SignalClient};
pub use protocol::Signal;
pub use relay::{handle_peer, Room, Rooms};
pub use server::serve;
