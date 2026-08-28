//! The orange signalling relay.
//!
//! Deliberately tiny and media-free: it matches two peers by room code and
//! forwards their handshake. Video never passes through here, which is why a
//! minimal container on the cheapest tier is enough.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    // Container platforms inject the port to listen on.
    let port = std::env::var("PORT").unwrap_or_else(|_| "9000".to_string());
    let addr = format!("0.0.0.0:{port}");

    tokio::select! {
        result = orange_signal::serve(&addr) => result,
        _ = tokio::signal::ctrl_c() => {
            println!("shutting down");
            Ok(())
        }
    }
}
