use crate::{capture, client, supervisor::WindowTarget};
use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const AVATAR_MAX_BYTES: usize = 4 * 1024 * 1024;
const THUMBNAIL_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
const AVATAR_JOIN_TIMEOUT: Duration = Duration::from_secs(35);
const FRIEND_AVATAR_RETRY: Duration = Duration::from_secs(30);

pub(super) enum PickerEvent {
    Windows(Vec<WindowTarget>),
    Thumbnail(i64, capture::Thumbnail),
    Failed(String),
}

struct ThumbnailJob {
    cancel: Arc<AtomicBool>,
    cancelled_at: Option<Instant>,
    receiver: mpsc::Receiver<PickerEvent>,
    worker: Option<JoinHandle<()>>,
}

impl ThumbnailJob {
    fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.cancelled_at.get_or_insert_with(Instant::now);
    }

    fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            join_background_worker(worker, THUMBNAIL_JOIN_TIMEOUT, "picker");
        }
    }
}

impl Drop for ThumbnailJob {
    fn drop(&mut self) {
        self.cancel();
        self.join();
    }
}

/// One worker covers enumeration and capture. Refresh only replaces a bit, not
/// a growing collection of retired PrintWindow calls that cannot be interrupted.
#[derive(Default)]
pub(super) struct PickerJobs {
    job: Option<ThumbnailJob>,
    pending: bool,
    loading: bool,
}

impl PickerJobs {
    pub(super) fn request(&mut self) {
        self.cancel();
        self.pending = true;
        self.loading = true;
    }

    pub(super) fn cancel(&mut self) {
        self.pending = false;
        self.loading = false;
        if let Some(job) = self.job.as_mut() {
            job.cancel();
        }
    }

    pub(super) fn is_busy(&self) -> bool {
        self.pending
            || self
                .job
                .as_ref()
                .is_some_and(|job| job.cancelled_at.is_none())
    }

    pub(super) fn is_loading(&self) -> bool {
        self.loading
    }

    pub(super) fn poll(&mut self) -> Vec<PickerEvent> {
        self.poll_with(crate::supervisor::picker_windows, |hwnd| {
            if hwnd == 0 {
                capture::screen_thumbnail(320, 180)
            } else {
                capture::thumbnail(hwnd as isize, 320, 180)
            }
        })
    }

    fn poll_with(
        &mut self,
        enumerate: impl FnOnce(&AtomicBool) -> anyhow::Result<Vec<WindowTarget>> + Send + 'static,
        capture: impl Fn(i64) -> Option<capture::Thumbnail> + Send + 'static,
    ) -> Vec<PickerEvent> {
        let mut events = Vec::new();
        if let Some(job) = self.job.as_mut() {
            // Observe completion before draining, so a last send cannot land
            // between an empty read and dropping the receiver.
            let finished = job.is_finished();
            if finished {
                job.join();
            } else {
                check_cancel_deadline(job.cancelled_at, THUMBNAIL_JOIN_TIMEOUT, "picker");
            }
            while let Ok(event) = job.receiver.try_recv() {
                if job.cancelled_at.is_none() {
                    if matches!(event, PickerEvent::Windows(_) | PickerEvent::Failed(_)) {
                        self.loading = false;
                    }
                    events.push(event);
                }
            }
            if !finished {
                return events;
            }
            self.job = None;
            self.loading = self.pending;
        }
        if self.pending {
            self.pending = false;
            let cancel = Arc::new(AtomicBool::new(false));
            let worker_cancel = Arc::clone(&cancel);
            let (sender, receiver) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                if worker_cancel.load(Ordering::Acquire) {
                    return;
                }
                let result = enumerate(&worker_cancel);
                if worker_cancel.load(Ordering::Acquire) {
                    return;
                }
                let windows = match result {
                    Ok(windows) => windows,
                    Err(error) => {
                        let _ = sender.send(PickerEvent::Failed(error.to_string()));
                        return;
                    }
                };
                let handles: Vec<_> = windows.iter().map(|window| window.hwnd).collect();
                if sender.send(PickerEvent::Windows(windows)).is_err() {
                    return;
                }
                for hwnd in handles {
                    if worker_cancel.load(Ordering::Acquire) {
                        return;
                    }
                    let thumbnail = capture(hwnd);
                    if worker_cancel.load(Ordering::Acquire) {
                        return;
                    }
                    if let Some(thumbnail) = thumbnail {
                        if sender
                            .send(PickerEvent::Thumbnail(hwnd, thumbnail))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            });
            self.job = Some(ThumbnailJob {
                cancel,
                cancelled_at: None,
                receiver,
                worker: Some(worker),
            });
        }
        events
    }
}

struct AvatarJob {
    cancel: Arc<AtomicBool>,
    cancelled_at: Option<Instant>,
    receiver: mpsc::Receiver<Option<capture::Thumbnail>>,
    worker: Option<JoinHandle<()>>,
}

impl AvatarJob {
    fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.cancelled_at.get_or_insert_with(Instant::now);
    }

    fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            join_background_worker(worker, AVATAR_JOIN_TIMEOUT, "avatar fetch");
        }
    }
}

impl Drop for AvatarJob {
    fn drop(&mut self) {
        self.cancel();
        self.join();
    }
}

#[derive(Default)]
pub(super) struct AvatarJobs {
    job: Option<AvatarJob>,
    pending: Option<String>,
}

impl AvatarJobs {
    pub(super) fn request(&mut self, url: Option<String>) {
        if let Some(job) = self.job.as_mut() {
            job.cancel();
        }
        self.pending = url;
    }

    pub(super) fn poll(&mut self) -> Option<capture::Thumbnail> {
        self.poll_with(avatar_fetcher())
    }

    fn poll_with(
        &mut self,
        fetch: impl FnOnce(&str) -> Option<Vec<u8>> + Send + 'static,
    ) -> Option<capture::Thumbnail> {
        let mut result = None;
        if let Some(job) = self.job.as_mut() {
            if !job.is_finished() {
                check_cancel_deadline(job.cancelled_at, AVATAR_JOIN_TIMEOUT, "avatar fetch");
                return None;
            }
            job.join();
            if job.cancelled_at.is_none() {
                result = job.receiver.try_recv().ok().flatten();
            }
            self.job = None;
        }
        if let Some(url) = self.pending.take() {
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
                if !worker_cancel.load(Ordering::Acquire) {
                    let _ = sender.send(pixels);
                }
            });
            self.job = Some(AvatarJob {
                cancel,
                cancelled_at: None,
                receiver,
                worker: Some(worker),
            });
        }
        result
    }
}

type FriendAvatarResult = (String, String, Option<capture::Thumbnail>);

struct FriendAvatarJob {
    cancel: Arc<AtomicBool>,
    receiver: mpsc::Receiver<FriendAvatarResult>,
    worker: Option<JoinHandle<()>>,
}

impl FriendAvatarJob {
    fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            join_background_worker(worker, AVATAR_JOIN_TIMEOUT, "friend avatar fetch");
        }
    }
}

impl Drop for FriendAvatarJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.join();
    }
}

/// Sequential batches bound sockets and concurrent decode memory independently
/// of roster size. A failed URL must not become a new request every UI tick.
#[derive(Default)]
pub(super) struct FriendAvatarJobs {
    job: Option<FriendAvatarJob>,
    retries: HashMap<String, (String, Instant)>,
}

impl FriendAvatarJobs {
    pub(super) fn cancel(&mut self) {
        if let Some(job) = self.job.as_ref() {
            job.cancel.store(true, Ordering::Release);
        }
    }

    pub(super) fn poll(
        &mut self,
        wanted: &[(String, String)],
        now: Instant,
    ) -> Vec<FriendAvatarResult> {
        self.poll_with(wanted, now, avatar_fetcher())
    }

    fn poll_with(
        &mut self,
        wanted: &[(String, String)],
        now: Instant,
        mut fetch: impl FnMut(&str) -> Option<Vec<u8>> + Send + 'static,
    ) -> Vec<FriendAvatarResult> {
        let current: HashMap<_, _> = wanted
            .iter()
            .map(|(id, url)| (id.as_str(), url.as_str()))
            .collect();
        self.retries.retain(|id, (url, _)| {
            current
                .get(id.as_str())
                .is_some_and(|wanted| *wanted == url)
        });
        if let Some(job) = self.job.as_mut() {
            let finished = job.is_finished();
            if finished {
                job.join();
            }
            let mut results = Vec::new();
            while let Ok((id, url, pixels)) = job.receiver.try_recv() {
                if job.cancel.load(Ordering::Acquire)
                    || current.get(id.as_str()).is_none_or(|wanted| *wanted != url)
                {
                    continue;
                }
                if pixels.is_some() {
                    self.retries.remove(&id);
                } else {
                    self.retries
                        .insert(id.clone(), (url.clone(), now + FRIEND_AVATAR_RETRY));
                }
                results.push((id, url, pixels));
            }
            if finished {
                self.job = None;
            }
            // Let the consumer install successful results before selecting the
            // next batch, otherwise that batch would fetch them again.
            return results;
        }
        let wanted: Vec<_> = wanted
            .iter()
            .filter(|(id, _)| self.retries.get(id).is_none_or(|(_, due)| now >= *due))
            .cloned()
            .collect();
        if wanted.is_empty() {
            return Vec::new();
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            for (id, url) in wanted {
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
                if sender.send((id, url, pixels)).is_err() {
                    return;
                }
            }
        });
        self.job = Some(FriendAvatarJob {
            cancel,
            receiver,
            worker: Some(worker),
        });
        Vec::new()
    }
}

pub(super) fn check_cancel_deadline(cancelled_at: Option<Instant>, timeout: Duration, name: &str) {
    if cancelled_at.is_some_and(|at| at.elapsed() >= timeout) {
        client::fail_fast(
            &format!("{name} worker did not terminate before its deadline"),
            &anyhow::anyhow!("timeout after {timeout:?}"),
        );
    }
}

pub(super) fn join_background_worker(worker: JoinHandle<()>, timeout: Duration, name: &str) {
    let deadline = Instant::now() + timeout;
    while !worker.is_finished() {
        if Instant::now() >= deadline {
            client::fail_fast(
                &format!("{name} worker did not terminate before its deadline"),
                &anyhow::anyhow!("timeout after {timeout:?}"),
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if worker.join().is_err() {
        eprintln!("[client] {name} worker panicked");
    }
}

/// The closure is made on the UI thread, but its client is constructed lazily
/// on the worker and reused for the whole sequential batch.
fn avatar_fetcher() -> impl FnMut(&str) -> Option<Vec<u8>> + Send {
    let mut client = None;
    move |url| {
        if client.is_none() {
            client = reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .user_agent(concat!("orange/", env!("CARGO_PKG_VERSION")))
                .build()
                .ok();
        }
        fetch_avatar(client.as_ref()?, url)
    }
}

fn fetch_avatar(client: &reqwest::blocking::Client, url: &str) -> Option<Vec<u8>> {
    let mut response = client.get(url).send().ok()?.error_for_status().ok()?;
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
#[path = "background_tests.rs"]
pub(crate) mod tests;
