//! Tests for [`super`], kept out of `relay.rs` so the room and routing rules and the
//! cases that pin it down can be read separately.

use super::*;
use crate::client::DropSignal;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
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
        social: crate::social::Social::new(None),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let relay = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("ws://{address}/ws");
    let (mut host, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

    host.send(tokio_tungstenite::tungstenite::Message::Text(
        Signal::Host {
            visible_to: Vec::new(),
        }
        .to_json(),
    ))
    .await
    .unwrap();
    assert!(matches!(
        receive_signal(&mut host).await,
        Signal::Hosting { .. }
    ));
    host.send(tokio_tungstenite::tungstenite::Message::Text(
        Signal::Host {
            visible_to: Vec::new(),
        }
        .to_json(),
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
        social: crate::social::Social::new(None),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let relay = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("ws://{address}/ws");

    tokio::time::timeout(Duration::from_secs(10), async {
        for cycle in 0..64 {
            let (mut host, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
            host.send(tokio_tungstenite::tungstenite::Message::Text(
                Signal::Host {
                    visible_to: Vec::new(),
                }
                .to_json(),
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

async fn accepted_friendship_server() -> (
    tokio::task::JoinHandle<()>,
    reqwest::Client,
    String,
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) {
    let auth = auth::Auth::new(None);
    for id in ["1", "2", "3"] {
        auth.insert_session_for_test(
            id,
            Identity {
                id: id.into(),
                name: format!("User {id}"),
                avatar_url: None,
            },
        )
        .await;
    }
    let app = server::router(server::AppState {
        rooms: Rooms::default(),
        auth,
        social: crate::social::Social::new(None),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let relay = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://{address}");
    let http = reqwest::Client::new();

    for id in ["1", "2"] {
        assert!(http
            .get(format!("{base}/friends"))
            .bearer_auth(id)
            .send()
            .await
            .unwrap()
            .status()
            .is_success());
    }
    let request = http
        .post(format!("{base}/friends"))
        .bearer_auth("2")
        .json(&serde_json::json!({"action":"request","target_id":"1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(request.status(), reqwest::StatusCode::NO_CONTENT);
    let inbox: Value = http
        .get(format!("{base}/friends"))
        .bearer_auth("1")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let revision = inbox["incoming"][0]["revision"]
        .as_str()
        .unwrap()
        .to_string();
    let accepted = http
        .post(format!("{base}/friends"))
        .bearer_auth("1")
        .json(&serde_json::json!({"action":"accept","target_id":"2","revision":revision}))
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), reqwest::StatusCode::NO_CONTENT);

    let (mut host, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .unwrap();
    host.send(tokio_tungstenite::tungstenite::Message::Text(
        Signal::Authenticate {
            session: "1".into(),
        }
        .to_json(),
    ))
    .await
    .unwrap();
    receive_signal(&mut host).await;

    (relay, http, base, host)
}

async fn presence_json(http: &reqwest::Client, url: String, bearer: &str) -> Value {
    http.get(url)
        .bearer_auth(bearer)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn viewer_rate_limit_counts_only_text_and_cleans_up_its_role() {
    let rooms = Rooms::default();
    let app = server::router(server::AppState {
        rooms: rooms.clone(),
        auth: auth::Auth::new(None),
        social: crate::social::Social::new(None),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let relay = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("ws://{address}/ws");

    let (mut host, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    host.send(tokio_tungstenite::tungstenite::Message::Text(
        Signal::Host {
            visible_to: Vec::new(),
        }
        .to_json(),
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
        social: crate::social::Social::new(None),
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

/// The whole feature in one test: a host goes live over the WebSocket
/// naming who may see it, and a friend learns the join code over HTTP
/// without anybody pasting anything. The unit tests around `presence_for`
/// check the filtering in isolation; this checks that the two transports
/// actually agree, which is where a real deployment would break.
#[tokio::test]
async fn a_friend_is_handed_the_join_code_over_http_without_ever_being_told_it() {
    let auth = auth::Auth::new(None);
    auth.insert_session_for_test(
        "host-token",
        Identity {
            id: "host-id".into(),
            name: "Host".into(),
            avatar_url: Some("https://cdn.discordapp.com/avatars/host-id/h.png".into()),
        },
    )
    .await;
    auth.insert_session_for_test(
        "friend-token",
        Identity {
            id: "friend-id".into(),
            name: "Friend".into(),
            avatar_url: None,
        },
    )
    .await;
    auth.insert_session_for_test(
        "stranger-token",
        Identity {
            id: "stranger-id".into(),
            name: "Stranger".into(),
            avatar_url: None,
        },
    )
    .await;

    let rooms = Rooms::default();
    let app = server::router(server::AppState {
        rooms: rooms.clone(),
        auth,
        social: crate::social::Social::new(None),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let relay = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (mut host, _) = tokio_tungstenite::connect_async(&format!("ws://{address}/ws"))
        .await
        .unwrap();
    host.send(tokio_tungstenite::tungstenite::Message::Text(
        Signal::Authenticate {
            session: "host-token".into(),
        }
        .to_json(),
    ))
    .await
    .unwrap();
    assert!(matches!(
        receive_signal(&mut host).await,
        Signal::Authenticated { .. }
    ));
    host.send(tokio_tungstenite::tungstenite::Message::Text(
        Signal::Host {
            visible_to: vec!["friend-id".into()],
        }
        .to_json(),
    ))
    .await
    .unwrap();
    let Signal::Hosting { code, .. } = receive_signal(&mut host).await else {
        panic!("host did not receive a room code");
    };

    let ask = |token: &'static str| async move {
        let body = reqwest::Client::new()
            .get(format!("http://{address}/presence"))
            .query(&[("ids", "host-id")])
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        body
    };

    assert_eq!(
        ask("friend-token").await,
        format!(
            r#"{{"friends":[{{"id":"host-id","name":"Host","avatar_url":"https://cdn.discordapp.com/avatars/host-id/h.png","state":"live","code":"{code}"}}]}}"#
        ),
        "a listed friend was not handed the code and profile"
    );
    assert_eq!(
        ask("stranger-token").await,
        r#"{"friends":[{"id":"host-id","state":"offline"}]}"#,
        "an unlisted caller learned the host was live"
    );

    // The room is the host's connection. Dropping it must take presence
    // with it, or friends keep a stale code that no longer joins anything.
    drop(host);
    for _ in 0..50 {
        if rooms.lock().await.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        ask("friend-token").await,
        r#"{"friends":[{"id":"host-id","state":"offline"}]}"#,
        "presence outlived the host connection"
    );

    relay.abort();
}

#[tokio::test]
async fn accepting_a_request_discovers_an_existing_stream_and_removal_revokes_it() {
    // Acceptance must work against a room opened before the request, and a
    // removed friendship must override even an older client's visible_to flag.
    let auth = auth::Auth::new(None);
    for id in ["1", "2", "3"] {
        auth.insert_session_for_test(
            id,
            Identity {
                id: id.into(),
                name: format!("User {id}"),
                avatar_url: None,
            },
        )
        .await;
    }
    let rooms = Rooms::default();
    let app = server::router(server::AppState {
        rooms: rooms.clone(),
        auth,
        social: crate::social::Social::new(None),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let relay = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = reqwest::Client::new();
    let base = format!("http://{address}");
    for id in ["1", "2"] {
        assert!(http
            .get(format!("{base}/friends"))
            .bearer_auth(id)
            .send()
            .await
            .unwrap()
            .status()
            .is_success());
    }
    let (mut host, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .unwrap();
    host.send(tokio_tungstenite::tungstenite::Message::Text(
        Signal::Authenticate {
            session: "1".into(),
        }
        .to_json(),
    ))
    .await
    .unwrap();
    receive_signal(&mut host).await;
    host.send(tokio_tungstenite::tungstenite::Message::Text(
        Signal::Host { visible_to: vec![] }.to_json(),
    ))
    .await
    .unwrap();
    let Signal::Hosting { code, .. } = receive_signal(&mut host).await else {
        panic!("no room");
    };
    let request = http
        .post(format!("{base}/friends"))
        .bearer_auth("2")
        .json(&serde_json::json!({"action":"request","target_id":"1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(request.status(), reqwest::StatusCode::NO_CONTENT);
    let before: serde_json::Value = http
        .get(format!("{base}/presence?ids=1"))
        .bearer_auth("2")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before["friends"][0]["state"], "offline");
    let inbox: serde_json::Value = http
        .get(format!("{base}/friends"))
        .bearer_auth("1")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let revision = inbox["incoming"][0]["revision"].as_str().unwrap();
    // The sender cannot accept on the recipient's behalf.
    let denied = http
        .post(format!("{base}/friends"))
        .bearer_auth("2")
        .json(&serde_json::json!({"action":"accept","target_id":"1","revision":revision}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::FORBIDDEN);
    let accepted = http
        .post(format!("{base}/friends"))
        .bearer_auth("1")
        .json(&serde_json::json!({"action":"accept","target_id":"2","revision":revision}))
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), reqwest::StatusCode::NO_CONTENT);
    let after: serde_json::Value = http
        .get(format!("{base}/presence?ids=1"))
        .bearer_auth("2")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["friends"][0]["code"], code);
    let stranger: serde_json::Value = http
        .get(format!("{base}/presence?ids=1"))
        .bearer_auth("3")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stranger["friends"][0]["state"], "offline");
    rooms
        .lock()
        .await
        .values_mut()
        .next()
        .unwrap()
        .visible_to
        .push("2".into());
    let removed = http
        .post(format!("{base}/friends"))
        .bearer_auth("2")
        .json(&serde_json::json!({"action":"remove","target_id":"1","revision":revision}))
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), reqwest::StatusCode::NO_CONTENT);
    let after: serde_json::Value = http
        .get(format!("{base}/presence?ids=1"))
        .bearer_auth("2")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["friends"][0]["state"], "offline");
    for id in ["1", "2"] {
        let snapshot: serde_json::Value = http
            .get(format!("{base}/friends"))
            .bearer_auth(id)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(snapshot["friends"], serde_json::json!([]));
    }
    drop(host);
    relay.abort();
}

#[tokio::test]
async fn abrupt_viewer_disconnect_notifies_host() {
    let rooms = Rooms::default();
    let app = server::router(server::AppState {
        rooms: rooms.clone(),
        auth: auth::Auth::new(None),
        social: crate::social::Social::new(None),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let relay = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("ws://{address}/ws");

    let (mut host, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    host.send(tokio_tungstenite::tungstenite::Message::Text(
        Signal::Host {
            visible_to: Vec::new(),
        }
        .to_json(),
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
        social: crate::social::Social::new(None),
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
        Signal::Host {
            visible_to: Vec::new(),
        }
        .to_json(),
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
        social: crate::social::Social::new(None),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let relay = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("ws://{address}/ws");

    let (mut host, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    host.send(tokio_tungstenite::tungstenite::Message::Text(
        Signal::Host {
            visible_to: Vec::new(),
        }
        .to_json(),
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
        social: crate::social::Social::new(None),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let relay = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("ws://{address}/ws");

    let (mut host, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    host.send(tokio_tungstenite::tungstenite::Message::Text(
        Signal::Host {
            visible_to: Vec::new(),
        }
        .to_json(),
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

#[tokio::test]
async fn waited_presence_returns_quickly_when_a_friend_goes_live_then_offline() {
    let (relay, http, base, mut host) = accepted_friendship_server().await;

    let first = presence_json(&http, format!("{base}/presence?ids=1&wait=1"), "2").await;
    assert_eq!(first["friends"][0]["state"], "offline");
    let first_revision = first["revision"].as_str().unwrap().to_string();

    let http_live = http.clone();
    let live_url = format!("{base}/presence?ids=1&wait=20&since={first_revision}");
    let live_wait = tokio::spawn(async move { presence_json(&http_live, live_url, "2").await });

    tokio::time::sleep(Duration::from_millis(250)).await;
    host.send(tokio_tungstenite::tungstenite::Message::Text(
        Signal::Host { visible_to: vec![] }.to_json(),
    ))
    .await
    .unwrap();
    let Signal::Hosting { code, .. } = receive_signal(&mut host).await else {
        panic!("host did not receive a room code");
    };

    let live = tokio::time::timeout(Duration::from_secs(1), live_wait)
        .await
        .expect("waited presence did not wake when friend went live")
        .unwrap();
    assert_eq!(live["friends"][0]["state"], "live");
    assert_eq!(live["friends"][0]["code"], code);
    let live_revision = live["revision"].as_str().unwrap().to_string();
    assert_ne!(live_revision, first_revision);

    let http_offline = http.clone();
    let offline_url = format!("{base}/presence?ids=1&wait=20&since={live_revision}");
    let offline_wait =
        tokio::spawn(async move { presence_json(&http_offline, offline_url, "2").await });

    drop(host);
    let offline = tokio::time::timeout(Duration::from_secs(1), offline_wait)
        .await
        .expect("waited presence did not wake when friend went offline")
        .unwrap();
    assert_eq!(offline["friends"][0]["state"], "offline");
    assert!(offline["friends"][0].get("code").is_none());
    assert_ne!(offline["revision"].as_str().unwrap(), live_revision);

    relay.abort();
}

#[tokio::test]
async fn waited_presence_releases_social_permits_and_times_out_without_changes() {
    let (relay, http, base, _host) = accepted_friendship_server().await;
    let first = presence_json(&http, format!("{base}/presence?ids=1&wait=1"), "2").await;
    let revision = first["revision"].as_str().unwrap().to_string();

    let mut waiters = Vec::new();
    for _ in 0..32 {
        let http_wait = http.clone();
        let url = format!("{base}/presence?ids=1&wait=20&since={revision}");
        waiters.push(tokio::spawn(async move {
            http_wait.get(url).bearer_auth("2").send().await
        }));
    }
    tokio::time::sleep(Duration::from_millis(250)).await;

    let friends_status = tokio::time::timeout(
        Duration::from_secs(1),
        http.get(format!("{base}/friends")).bearer_auth("2").send(),
    )
    .await
    .expect("/friends blocked while waited /presence requests were idle")
    .unwrap()
    .status();
    assert_eq!(friends_status, reqwest::StatusCode::OK);

    for waiter in &waiters {
        waiter.abort();
    }

    let started = tokio::time::Instant::now();
    let timed_out = presence_json(
        &http,
        format!("{base}/presence?ids=1&wait=1&since={revision}"),
        "2",
    )
    .await;
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(900));
    assert!(elapsed < Duration::from_secs(3));
    assert_eq!(timed_out["revision"].as_str().unwrap(), revision);
    assert_eq!(timed_out["friends"][0]["state"], "offline");

    relay.abort();
}

#[tokio::test]
async fn waited_presence_rechecks_relationships_before_returning_after_candidate_change() {
    let (relay, http, base, mut host) = accepted_friendship_server().await;
    host.send(tokio_tungstenite::tungstenite::Message::Text(
        Signal::Host { visible_to: vec![] }.to_json(),
    ))
    .await
    .unwrap();
    let Signal::Hosting { .. } = receive_signal(&mut host).await else {
        panic!("host did not receive a room code");
    };

    let live = presence_json(&http, format!("{base}/presence?ids=1&wait=1"), "2").await;
    assert_eq!(live["friends"][0]["state"], "live");
    let live_revision = live["revision"].as_str().unwrap().to_string();

    let waiting_http = http.clone();
    let waiting_url = format!("{base}/presence?ids=1&wait=20&since={live_revision}");
    let waiting = tokio::spawn(async move { presence_json(&waiting_http, waiting_url, "2").await });

    let inbox: Value = http
        .get(format!("{base}/friends"))
        .bearer_auth("2")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let remove_revision = inbox["friends"][0]["revision"].as_str().unwrap();
    let removed = http
        .post(format!("{base}/friends"))
        .bearer_auth("2")
        .json(&serde_json::json!({"action":"remove","target_id":"1","revision":remove_revision}))
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), reqwest::StatusCode::NO_CONTENT);

    drop(host);
    let result = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("waited presence did not wake when room changed")
        .unwrap();
    assert_eq!(result["friends"][0]["state"], "offline");
    assert!(result["friends"][0].get("code").is_none());

    relay.abort();
}
