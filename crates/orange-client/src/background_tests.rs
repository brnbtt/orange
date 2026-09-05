use super::*;

const WAIT: Duration = Duration::from_secs(3);

struct ReleaseOnDrop(Option<mpsc::Sender<()>>);

impl ReleaseOnDrop {
    fn release(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.release();
    }
}

fn window(hwnd: i64) -> crate::supervisor::WindowTarget {
    crate::supervisor::WindowTarget {
        hwnd,
        title: format!("Window {hwnd}"),
        process: "test.exe".into(),
        width: 100,
        height: 100,
    }
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(Instant::now() < deadline, "worker did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn picker_refresh_returns_during_enumeration_and_coalesces_replacements() {
    // The UI used to run orange list synchronously and join PrintWindow on
    // every refresh. Blocking the real worker proves both paths stay off it.
    let mut picker = PickerJobs::default();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mut release = ReleaseOnDrop(Some(release_tx));
    picker.request();
    assert!(picker
        .poll_with(
            move |_| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(WAIT).unwrap();
                Ok(vec![window(1)])
            },
            |_| panic!("cancelled enumeration must not capture")
        )
        .is_empty());
    entered_rx.recv_timeout(WAIT).unwrap();
    for _ in 0..20 {
        picker.request();
        assert!(picker
            .poll_with(|_| panic!("overlapping enumeration"), |_| None)
            .is_empty());
    }
    assert!(picker.is_loading());
    release.release();
    wait_until(|| picker.job.as_ref().unwrap().is_finished());
    let events = picker.poll_with(|_| Ok(vec![window(2)]), |_| Some((1, 1, vec![0; 4])));
    assert!(events.is_empty(), "cancelled list escaped");
    wait_until(|| picker.job.as_ref().unwrap().is_finished());
    let events = picker.poll_with(|_| panic!("duplicate refresh"), |_| None);
    assert!(matches!(&events[0], PickerEvent::Windows(windows) if windows[0].hwnd == 2));
    assert!(matches!(&events[1], PickerEvent::Thumbnail(2, _)));
    assert!(!picker.is_busy());
}

#[test]
fn leaving_the_picker_discards_queued_results_and_pending_refresh() {
    // Cancellation can happen after the sender's cancellation check, so the
    // consuming scheduler must reject already queued lists, errors and pixels.
    for fail in [false, true] {
        let mut picker = PickerJobs::default();
        picker.request();
        picker.poll_with(
            move |_| {
                if fail {
                    anyhow::bail!("old error")
                } else {
                    Ok(vec![window(1)])
                }
            },
            |_| Some((1, 1, vec![0; 4])),
        );
        wait_until(|| picker.job.as_ref().unwrap().is_finished());
        picker.request();
        picker.cancel();
        assert!(picker
            .poll_with(|_| panic!("navigation left pending work"), |_| None)
            .is_empty());
        assert!(!picker.is_busy());
        assert!(picker.job.is_none());
    }
}

#[test]
fn leaving_during_capture_returns_without_starting_another_capture() {
    let mut picker = PickerJobs::default();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mut release = ReleaseOnDrop(Some(release_tx));
    picker.request();
    picker.poll_with(
        |_| Ok(vec![window(1), window(2)]),
        move |hwnd| {
            assert_eq!(hwnd, 1, "cancelled batch continued capturing");
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(WAIT).unwrap();
            Some((1, 1, vec![0; 4]))
        },
    );
    entered_rx.recv_timeout(WAIT).unwrap();
    picker.cancel();
    assert!(!picker.is_busy());
    assert!(picker
        .poll_with(|_| panic!("unexpected restart"), |_| None)
        .is_empty());
    assert!(!picker.job.as_ref().unwrap().is_finished());
    release.release();
    wait_until(|| picker.job.as_ref().unwrap().is_finished());
    assert!(picker
        .poll_with(|_| panic!("unexpected restart"), |_| None)
        .is_empty());
}

fn png() -> Vec<u8> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]))
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

#[test]
fn avatar_replacement_keeps_only_the_latest_request_without_overlapping() {
    // Login and signout used to wait for a possibly 30-second fetch.
    let mut avatars = AvatarJobs::default();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mut release = ReleaseOnDrop(Some(release_tx));
    avatars.request(Some("old".into()));
    avatars.poll_with(move |_| {
        entered_tx.send(()).unwrap();
        release_rx.recv_timeout(WAIT).unwrap();
        Some(png())
    });
    entered_rx.recv_timeout(WAIT).unwrap();
    for url in ["intermediate", "latest"] {
        avatars.request(Some(url.into()));
        assert!(avatars.poll_with(|_| panic!("overlapping fetch")).is_none());
    }
    release.release();
    wait_until(|| avatars.job.as_ref().unwrap().is_finished());
    assert!(avatars
        .poll_with(|url| {
            assert_eq!(url, "latest");
            Some(png())
        })
        .is_none());
    wait_until(|| avatars.job.as_ref().unwrap().is_finished());
    assert!(avatars.poll_with(|_| panic!("duplicate fetch")).is_some());
    assert!(avatars
        .poll_with(|_| panic!("automatic own-avatar retry"))
        .is_none());
}

#[test]
fn signout_discards_an_avatar_already_queued_by_the_worker() {
    let mut avatars = AvatarJobs::default();
    avatars.request(Some("old".into()));
    avatars.poll_with(|_| Some(png()));
    wait_until(|| avatars.job.as_ref().unwrap().is_finished());
    avatars.request(None);
    assert!(avatars
        .poll_with(|_| panic!("signout left a request"))
        .is_none());
    assert!(avatars.job.is_none());
}

#[test]
fn final_drop_joins_a_cancelled_avatar_instead_of_detaching_it() {
    let mut avatars = AvatarJobs::default();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let mut release = ReleaseOnDrop(Some(release_tx));
    avatars.request(Some("old".into()));
    avatars.poll_with(move |_| {
        entered_tx.send(()).unwrap();
        release_rx.recv_timeout(WAIT).unwrap();
        None
    });
    entered_rx.recv_timeout(WAIT).unwrap();
    avatars.request(None);
    std::thread::scope(|scope| {
        let owner = scope.spawn(move || {
            drop(avatars);
            done_tx.send(()).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        release.release();
        done_rx.recv_timeout(WAIT).unwrap();
        owner.join().unwrap();
    });
}

#[test]
fn friend_avatar_failures_wait_for_the_retry_deadline() {
    // A missing CDN image previously created a new request every other tick.
    let mut friends = FriendAvatarJobs::default();
    let wanted = vec![("a".into(), "url".into())];
    let now = Instant::now();
    friends.poll_with(&wanted, now, |_| None);
    wait_until(|| friends.job.as_ref().unwrap().is_finished());
    friends.poll_with(&wanted, now, |_| panic!("overlapping fetch"));
    for elapsed in [
        Duration::ZERO,
        Duration::from_secs(1),
        Duration::from_secs(29),
    ] {
        assert!(friends
            .poll_with(&wanted, now + elapsed, |_| panic!("retry before deadline"))
            .is_empty());
        assert!(friends.job.is_none());
    }
    friends.poll_with(&wanted, now + Duration::from_secs(30), |_| Some(png()));
    wait_until(|| friends.job.as_ref().unwrap().is_finished());
    let results = friends.poll_with(&wanted, now + Duration::from_secs(30), |_| {
        panic!("duplicate fetch")
    });
    assert_eq!(results.len(), 1);
    assert!(results[0].2.is_some());
    assert!(friends.retries.is_empty());
}

#[test]
fn changed_or_removed_friend_urls_do_not_accept_old_results_or_backoff() {
    let mut friends = FriendAvatarJobs::default();
    let now = Instant::now();
    friends.poll_with(&[("a".into(), "old".into())], now, |_| Some(png()));
    wait_until(|| friends.job.as_ref().unwrap().is_finished());
    assert!(friends
        .poll_with(&[], now, |_| panic!("removed friend fetched"))
        .is_empty());
    friends.poll_with(&[("a".into(), "old".into())], now, |_| None);
    wait_until(|| friends.job.as_ref().unwrap().is_finished());
    friends.poll_with(&[("a".into(), "old".into())], now, |_| None);
    friends.poll_with(&[("a".into(), "new".into())], now, |url| {
        assert_eq!(url, "new");
        Some(png())
    });
    wait_until(|| friends.job.as_ref().unwrap().is_finished());
    assert_eq!(
        friends
            .poll_with(&[("a".into(), "new".into())], now, |_| None)
            .len(),
        1
    );
}

#[test]
fn avatar_response_is_rejected_before_exceeding_its_memory_cap() {
    let mut oversized = std::io::Cursor::new(vec![0; AVATAR_MAX_BYTES + 1]);
    assert!(read_avatar_response(&mut oversized).is_none());
}

// A real HTTP peer is needed here: comparing Client pointers would miss a
// factory accidentally invoked inside the per-friend/per-poll loop.
pub(crate) struct HttpServer {
    address: std::net::SocketAddr,
    requests: mpsc::Receiver<Vec<(std::net::SocketAddr, String)>>,
    worker: Option<JoinHandle<()>>,
}

impl HttpServer {
    pub(crate) fn new(responses: Vec<(u16, Vec<u8>)>) -> Self {
        use std::io::{BufRead, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (sender, requests) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + WAIT;
            let mut connection: Option<std::io::BufReader<std::net::TcpStream>> = None;
            let mut requests = Vec::new();
            for (status, body) in responses {
                let request = loop {
                    assert!(Instant::now() < deadline, "HTTP requests did not arrive");
                    if connection.is_none() {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                stream
                                    .set_read_timeout(Some(Duration::from_millis(100)))
                                    .unwrap();
                                stream.set_write_timeout(Some(WAIT)).unwrap();
                                connection = Some(std::io::BufReader::new(stream));
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(1));
                                continue;
                            }
                            Err(error) => panic!("accept: {error}"),
                        }
                    }
                    let mut line = String::new();
                    match connection.as_mut().unwrap().read_line(&mut line) {
                        Ok(0) => {
                            connection = None;
                            continue;
                        }
                        Ok(_) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            continue
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {
                            connection = None;
                            continue;
                        }
                        Err(error) => panic!("request: {error}"),
                    }
                    let mut request = line;
                    let mut content_length = 0;
                    loop {
                        let mut header = String::new();
                        connection.as_mut().unwrap().read_line(&mut header).unwrap();
                        if header == "\r\n" {
                            break;
                        }
                        assert!(!header.is_empty(), "incomplete headers");
                        if let Some((name, value)) = header.split_once(':') {
                            if name.eq_ignore_ascii_case("content-length") {
                                content_length = value.trim().parse::<usize>().unwrap();
                            }
                        }
                        request.push_str(&header);
                    }
                    if content_length > 0 {
                        assert!(content_length <= 4096, "unexpectedly large test request");
                        let mut body = vec![0; content_length];
                        std::io::Read::read_exact(connection.as_mut().unwrap(), &mut body).unwrap();
                        request.push_str("\r\n");
                        request.push_str(std::str::from_utf8(&body).unwrap());
                    }
                    break request;
                };
                let stream = connection.as_mut().unwrap().get_mut();
                requests.push((stream.peer_addr().unwrap(), request));
                write!(stream, "HTTP/1.1 {status} Result\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n", body.len()).unwrap();
                if let Err(error) = stream.write_all(&body) {
                    // Size-limited clients legitimately close before consuming
                    // an oversized body. Still report the observed request.
                    assert!(
                        matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::BrokenPipe
                        ),
                        "response: {error}"
                    );
                    sender.send(requests).unwrap();
                    return;
                }
                stream.flush().unwrap();
            }
            sender.send(requests).unwrap();
        });
        Self {
            address,
            requests,
            worker: Some(worker),
        }
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    pub(crate) fn finish(mut self) -> Vec<(std::net::SocketAddr, String)> {
        let requests = self.requests.recv_timeout(WAIT).unwrap();
        self.worker.take().unwrap().join().unwrap();
        requests
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            join_background_worker(worker, Duration::from_secs(5), "test HTTP server");
        }
    }
}

#[test]
fn a_friend_avatar_batch_reuses_one_http_connection() {
    let server = HttpServer::new(vec![(200, png()), (200, png())]);
    let mut friends = FriendAvatarJobs::default();
    let wanted = vec![
        ("a".into(), server.url("/a")),
        ("b".into(), server.url("/b")),
    ];
    let now = Instant::now();
    friends.poll(&wanted, now);
    wait_until(|| friends.job.as_ref().unwrap().is_finished());
    let results = friends.poll(&wanted, now);
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|entry| entry.2.is_some()));
    let requests = server.finish();
    assert_eq!(
        requests[0].0, requests[1].0,
        "batch opened a second connection"
    );
    assert!(requests[0].1.starts_with("GET /a "));
    assert!(requests[1].1.starts_with("GET /b "));
}

#[test]
fn a_failed_own_avatar_does_not_schedule_an_automatic_retry() {
    let mut avatars = AvatarJobs::default();
    avatars.request(Some("missing".into()));
    avatars.poll_with(|_| None);
    wait_until(|| avatars.job.as_ref().unwrap().is_finished());
    assert!(avatars.poll_with(|_| panic!("unrequested retry")).is_none());
    assert!(avatars.poll_with(|_| panic!("unrequested retry")).is_none());
    assert!(avatars.job.is_none());
}

#[test]
fn final_picker_drop_joins_the_capture_and_does_not_run_pending_refresh() {
    let mut picker = PickerJobs::default();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let mut release = ReleaseOnDrop(Some(release_tx));
    picker.request();
    picker.poll_with(
        |_| Ok(vec![window(1)]),
        move |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(WAIT).unwrap();
            None
        },
    );
    entered_rx.recv_timeout(WAIT).unwrap();
    picker.request();
    std::thread::scope(|scope| {
        let owner = scope.spawn(move || {
            drop(picker);
            done_tx.send(()).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        release.release();
        done_rx.recv_timeout(WAIT).unwrap();
        owner.join().unwrap();
    });
}

#[test]
fn friend_batch_drop_cancels_the_remaining_fetches_and_joins_the_current_one() {
    let mut friends = FriendAvatarJobs::default();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let mut release = ReleaseOnDrop(Some(release_tx));
    friends.poll_with(
        &[("a".into(), "first".into()), ("b".into(), "second".into())],
        Instant::now(),
        move |url| {
            assert_eq!(url, "first");
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(WAIT).unwrap();
            None
        },
    );
    entered_rx.recv_timeout(WAIT).unwrap();
    friends.cancel();
    std::thread::scope(|scope| {
        let owner = scope.spawn(move || {
            drop(friends);
            done_tx.send(()).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        release.release();
        done_rx.recv_timeout(WAIT).unwrap();
        owner.join().unwrap();
    });
}

#[test]
fn oversized_avatar_http_bodies_and_decoded_dimensions_are_rejected() {
    let server = HttpServer::new(vec![(200, vec![0; AVATAR_MAX_BYTES + 1])]);
    assert!(avatar_fetcher()(&server.url("/large")).is_none());
    // The body is small but the declared width is outside the decoder policy.
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::RgbaImage::new(4097, 1)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    assert!(decode_avatar(bytes.into_inner()).is_none());
    // The client may close early when Content-Length exceeds its policy.
    drop(server);
}
