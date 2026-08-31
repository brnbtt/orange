use crate::{capture, tray};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const AVATAR_MAX_BYTES: usize = 4 * 1024 * 1024;
const THUMBNAIL_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
const AVATAR_JOIN_TIMEOUT: Duration = Duration::from_secs(35);

pub(super) struct ThumbnailJob {
    cancel: Arc<AtomicBool>,
    pub(super) receiver: mpsc::Receiver<(i64, capture::Thumbnail)>,
    worker: Option<JoinHandle<()>>,
}

impl ThumbnailJob {
    pub(super) fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub(super) fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            join_background_worker(worker, THUMBNAIL_JOIN_TIMEOUT, "thumbnail capture");
        }
    }
}

impl Drop for ThumbnailJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.join();
    }
}

pub(super) struct AvatarJob {
    cancel: Arc<AtomicBool>,
    pub(super) receiver: mpsc::Receiver<Option<capture::Thumbnail>>,
    worker: Option<JoinHandle<()>>,
}

impl AvatarJob {
    pub(super) fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub(super) fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            join_background_worker(worker, AVATAR_JOIN_TIMEOUT, "avatar fetch");
        }
    }
}

impl Drop for AvatarJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.join();
    }
}

fn join_background_worker(worker: JoinHandle<()>, timeout: Duration, name: &str) {
    let deadline = Instant::now() + timeout;
    while !worker.is_finished() {
        if Instant::now() >= deadline {
            tray::fail_fast(
                &format!("{name} worker did not terminate before its deadline"),
                &anyhow::anyhow!("timeout after {timeout:?}"),
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if worker.join().is_err() {
        eprintln!("[tray] {name} worker panicked");
    }
}

pub(super) fn replace_thumbnail_job(
    job: &mut Option<ThumbnailJob>,
    handles: Vec<i64>,
    capture: impl Fn(i64) -> Option<capture::Thumbnail> + Send + 'static,
) {
    stop_thumbnail_job(job);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let (sender, receiver) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        for hwnd in handles {
            if worker_cancel.load(Ordering::Acquire) {
                return;
            }
            let thumbnail = capture(hwnd);
            if worker_cancel.load(Ordering::Acquire) {
                return;
            }
            if let Some(thumbnail) = thumbnail {
                if worker_cancel.load(Ordering::Acquire) {
                    return;
                }
                if sender.send((hwnd, thumbnail)).is_err() {
                    return;
                }
            }
        }
    });
    *job = Some(ThumbnailJob {
        cancel,
        receiver,
        worker: Some(worker),
    });
}

pub(super) fn stop_thumbnail_job(job: &mut Option<ThumbnailJob>) {
    drop(job.take());
}

pub(super) fn replace_avatar_job(
    job: &mut Option<AvatarJob>,
    url: Option<String>,
    fetch: impl FnOnce(&str) -> Option<Vec<u8>> + Send + 'static,
) {
    stop_avatar_job(job);
    let Some(url) = url else {
        return;
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let (sender, receiver) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        if worker_cancel.load(Ordering::Acquire) {
            return;
        }
        let bytes = fetch(&url);
        if worker_cancel.load(Ordering::Acquire) {
            return;
        }
        let pixels = bytes.and_then(decode_avatar);
        if worker_cancel.load(Ordering::Acquire) {
            return;
        }
        if !worker_cancel.load(Ordering::Acquire) {
            let _ = sender.send(pixels);
        }
    });
    *job = Some(AvatarJob {
        cancel,
        receiver,
        worker: Some(worker),
    });
}

pub(super) fn stop_avatar_job(job: &mut Option<AvatarJob>) {
    drop(job.take());
}

pub(super) fn fetch_avatar(url: &str) -> Option<Vec<u8>> {
    let mut response = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("orange/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?
        .get(url)
        .send()
        .ok()?
        .error_for_status()
        .ok()?;
    if response
        .content_length()
        .is_some_and(|length| length > AVATAR_MAX_BYTES as u64)
    {
        return None;
    }
    read_avatar_response(&mut response)
}

fn read_avatar_response(reader: &mut impl std::io::Read) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(AVATAR_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= AVATAR_MAX_BYTES).then_some(bytes)
}

fn decode_avatar(bytes: Vec<u8>) -> Option<capture::Thumbnail> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes));
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?
        .into_rgba8();
    let mut raw =
        image::imageops::resize(&image, 64, 64, image::imageops::FilterType::Lanczos3).into_raw();
    // GPUI's image renderer expects BGRA.
    for pixel in raw.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Some((64, 64, raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ReleaseOnDrop(Option<mpsc::Sender<()>>);

    impl ReleaseOnDrop {
        fn new(sender: mpsc::Sender<()>) -> Self {
            Self(Some(sender))
        }

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

    #[test]
    fn thumbnail_background_job_replacement_cancels_and_joins_the_old_worker() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (cancelled_tx, cancelled_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let exited = Arc::new(AtomicBool::new(false));
        let worker_exited = Arc::clone(&exited);
        let mut job = None;

        replace_thumbnail_job(&mut job, vec![1, 2], move |_| {
            entered_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(1));
            worker_exited.store(true, Ordering::Release);
            Some((1, 1, vec![0; 4]))
        });
        let old_cancel = Arc::clone(&job.as_ref().unwrap().cancel);
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("old capture did not start");

        std::thread::scope(|scope| {
            let (caller_done_tx, caller_done_rx) = mpsc::channel();
            let observer = scope.spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(1);
                while !old_cancel.load(Ordering::Acquire) && Instant::now() < deadline {
                    std::thread::yield_now();
                }
                let _ = cancelled_tx.send(old_cancel.load(Ordering::Acquire));
            });
            let job = &mut job;
            let caller = scope.spawn(move || {
                replace_thumbnail_job(job, vec![3], |_| None);
                let _ = caller_done_tx.send(());
            });
            let mut release = ReleaseOnDrop::new(release_tx);

            assert!(cancelled_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("worker did not report cancellation"));
            assert!(matches!(
                caller_done_rx.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
            release.release();
            caller_done_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("replacement did not return after worker release");
            assert!(exited.load(Ordering::Acquire));
            caller.join().unwrap();
            observer.join().unwrap();
        });
        stop_thumbnail_job(&mut job);
    }

    #[test]
    fn avatar_background_job_replacement_cancels_and_joins_the_old_worker() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (cancelled_tx, cancelled_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let exited = Arc::new(AtomicBool::new(false));
        let worker_exited = Arc::clone(&exited);
        let mut job = None;

        replace_avatar_job(
            &mut job,
            Some("https://example.com/old.png".into()),
            move |_| {
                entered_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(1));
                worker_exited.store(true, Ordering::Release);
                Some(Vec::new())
            },
        );
        let old_cancel = Arc::clone(&job.as_ref().unwrap().cancel);
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("old fetch did not start");

        std::thread::scope(|scope| {
            let (caller_done_tx, caller_done_rx) = mpsc::channel();
            let observer = scope.spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(1);
                while !old_cancel.load(Ordering::Acquire) && Instant::now() < deadline {
                    std::thread::yield_now();
                }
                let _ = cancelled_tx.send(old_cancel.load(Ordering::Acquire));
            });
            let job = &mut job;
            let caller = scope.spawn(move || {
                replace_avatar_job(job, Some("https://example.com/new.png".into()), |_| None);
                let _ = caller_done_tx.send(());
            });
            let mut release = ReleaseOnDrop::new(release_tx);

            assert!(cancelled_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("worker did not report cancellation"));
            assert!(matches!(
                caller_done_rx.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
            release.release();
            caller_done_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("replacement did not return after worker release");
            assert!(exited.load(Ordering::Acquire));
            caller.join().unwrap();
            observer.join().unwrap();
        });
        stop_avatar_job(&mut job);
    }

    #[test]
    fn avatar_response_is_rejected_before_exceeding_its_memory_cap() {
        let mut oversized = std::io::Cursor::new(vec![0; AVATAR_MAX_BYTES + 1]);

        assert!(read_avatar_response(&mut oversized).is_none());
    }
}
