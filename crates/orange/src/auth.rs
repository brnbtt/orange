//! Desktop side of Discord login.
//!
//! The app never handles Discord's `client_secret`; it asks the relay to start
//! a login, opens the browser, then polls until the relay reports a session.
//! Polling avoids needing a fixed loopback port for the OAuth redirect, which
//! Discord's exact-match redirect rule would otherwise force on us.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Gives up if the browser leg is never completed.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub token: String,
    pub id: String,
    pub name: String,
    pub avatar_url: Option<String>,
}

fn session_path() -> Result<PathBuf> {
    let dir = std::env::var("APPDATA").context("APPDATA is not set")?;
    Ok(PathBuf::from(dir).join("orange").join("session.json"))
}

pub fn load_session() -> Option<Session> {
    let path = session_path().ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_session(session: &Session) -> Result<()> {
    let path = session_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(session)?)?;
    Ok(())
}

pub fn clear_session() -> Result<()> {
    let path = session_path()?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Turn a WebSocket relay URL into its HTTP origin, so one `--server` value
/// configures both the signalling socket and the auth endpoints.
pub fn http_base(server: &str) -> String {
    let base = server
        .trim_end_matches('/')
        .trim_end_matches("/ws")
        .to_string();
    if let Some(rest) = base.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = base.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        base
    }
}

#[derive(Deserialize)]
struct StartResponse {
    url: String,
    state: String,
}

#[derive(Deserialize)]
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

/// Run the whole login: ask the relay, open the browser, wait for the result.
pub async fn login(server: &str) -> Result<Session> {
    let base = http_base(server);
    let http = reqwest::Client::new();

    let start: StartResponse = http
        .get(format!("{base}/auth/start"))
        .send()
        .await
        .context("could not reach the relay")?
        .error_for_status()
        .context("the relay does not have Discord login configured")?
        .json()
        .await?;

    println!("Opening your browser to sign in with Discord...");
    if let Err(err) = open_in_browser(&start.url) {
        println!("Could not open a browser ({err}). Visit this URL:\n{}", start.url);
    }

    let began = Instant::now();
    loop {
        if began.elapsed() > LOGIN_TIMEOUT {
            bail!("login timed out");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;

        let body: PollBody = match http
            .get(format!("{base}/auth/poll"))
            .query(&[("state", &start.state)])
            .send()
            .await
        {
            Ok(response) => response.json().await?,
            // A transient network blip should not end the login.
            Err(_) => continue,
        };

        match body {
            PollBody::Waiting => continue,
            PollBody::Ready {
                session,
                id,
                name,
                avatar_url,
            } => {
                let session = Session {
                    token: session,
                    id,
                    name,
                    avatar_url,
                };
                save_session(&session)?;
                return Ok(session);
            }
            PollBody::Failed { message } => bail!("{message}"),
            PollBody::Unknown => bail!("login attempt expired"),
        }
    }
}

fn open_in_browser(url: &str) -> Result<()> {
    // `start` is a shell builtin, hence cmd; the empty title argument is
    // required or the first quoted argument is taken as the window title.
    std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()?;
    Ok(())
}
