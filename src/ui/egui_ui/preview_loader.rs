use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use eframe::egui::ColorImage;

use crate::fs::thumb_rel_path;

const PREVIEW_WORKERS: usize = 2;
const PREVIEW_QUEUE_CAPACITY: usize = 64;
const PREVIEW_MAX_SIDE: u32 = 400;
pub(super) const VIEWER_PROXY_MAX_SIDE: u32 = 4096;
pub(super) const ORIGINAL_MAX_RGBA_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ImageLoadMode {
    Preview,
    Original,
}

#[derive(Clone, Debug)]
pub(super) struct PreviewSource {
    pub(super) key: String,
    pub(super) image_id: i64,
    pub(super) base_dir: PathBuf,
    pub(super) thumb_dir: PathBuf,
    pub(super) local_file_path: Option<String>,
    pub(super) thumbnail_path: Option<String>,
    pub(super) mode: ImageLoadMode,
    /// Optional long-edge limit for a high-quality viewer proxy. `None` keeps
    /// the original dimensions.
    pub(super) max_side: Option<u32>,
}

#[derive(Clone, Debug)]
pub(super) struct DecodedPreview {
    pub(super) key: String,
    pub(super) image_id: i64,
    pub(super) image: ColorImage,
    pub(super) estimated_bytes: usize,
    pub(super) mode: ImageLoadMode,
    pub(super) timings: ImageLoadTimings,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ImageLoadTimings {
    pub(super) queue_wait: Duration,
    pub(super) header_read: Duration,
    pub(super) decode: Duration,
    pub(super) transform: Duration,
    pub(super) rgba_conversion: Duration,
    pub(super) color_image_conversion: Duration,
}

impl ImageLoadTimings {
    pub(super) fn before_texture(self) -> Duration {
        self.queue_wait
            + self.header_read
            + self.decode
            + self.transform
            + self.rgba_conversion
            + self.color_image_conversion
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PreviewRequestDisposition {
    Scheduled,
    AlreadyPending,
    QueueFull,
    Unavailable,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PreviewIdentity {
    generation: u64,
    key: String,
}

struct PreviewWorkItem {
    identity: PreviewIdentity,
    source: PreviewSource,
    repaint: egui::Context,
    queued_at: Instant,
}

enum PreviewDecodeOutcome {
    Ready(DecodedPreview),
    Unavailable,
}

struct PreviewWorkerResult {
    identity: PreviewIdentity,
    outcome: PreviewDecodeOutcome,
}

type PreviewDecoder = Arc<dyn Fn(PreviewSource) -> PreviewDecodeOutcome + Send + Sync>;

/// Bounded background image decoder.
///
/// Both request and result channels are bounded. This prevents rapid scrolling
/// from accumulating an unbounded number of full-size decodes or decoded RGBA
/// buffers while the UI thread is busy.
pub(super) struct PreviewLoader {
    request_tx: SyncSender<PreviewWorkItem>,
    _request_rx: Arc<Mutex<Receiver<PreviewWorkItem>>>,
    result_rx: Receiver<PreviewWorkerResult>,
    generation: u64,
    current_generation: Arc<AtomicU64>,
    in_flight: HashSet<PreviewIdentity>,
    unavailable: HashSet<PreviewIdentity>,
}

impl PreviewLoader {
    pub(super) fn new() -> Self {
        Self::with_decoder(
            PREVIEW_WORKERS,
            PREVIEW_QUEUE_CAPACITY,
            Arc::new(decode_preview),
        )
    }

    fn with_decoder(worker_count: usize, queue_capacity: usize, decoder: PreviewDecoder) -> Self {
        let queue_capacity = queue_capacity.max(1);
        let (request_tx, request_rx) = mpsc::sync_channel(queue_capacity);
        let request_rx = Arc::new(Mutex::new(request_rx));
        let (result_tx, result_rx) =
            mpsc::sync_channel(queue_capacity.saturating_add(worker_count.max(1)));
        let current_generation = Arc::new(AtomicU64::new(0));

        for worker_index in 0..worker_count {
            let request_rx = Arc::clone(&request_rx);
            let result_tx = result_tx.clone();
            let decoder = Arc::clone(&decoder);
            let current_generation = Arc::clone(&current_generation);
            let _ = thread::Builder::new()
                .name(format!("preview-loader-{worker_index}"))
                .spawn(move || preview_worker(request_rx, result_tx, decoder, current_generation));
        }

        Self {
            request_tx,
            _request_rx: request_rx,
            result_rx,
            generation: 0,
            current_generation,
            in_flight: HashSet::new(),
            unavailable: HashSet::new(),
        }
    }

    pub(super) fn request(
        &mut self,
        source: PreviewSource,
        repaint: &egui::Context,
    ) -> PreviewRequestDisposition {
        let identity = PreviewIdentity {
            generation: self.generation,
            key: source.key.clone(),
        };
        if self.unavailable.contains(&identity) {
            return PreviewRequestDisposition::Unavailable;
        }
        if self.in_flight.contains(&identity) {
            return PreviewRequestDisposition::AlreadyPending;
        }

        let item = PreviewWorkItem {
            identity: identity.clone(),
            source,
            repaint: repaint.clone(),
            queued_at: Instant::now(),
        };
        match self.request_tx.try_send(item) {
            Ok(()) => {
                self.in_flight.insert(identity);
                PreviewRequestDisposition::Scheduled
            }
            Err(TrySendError::Full(_)) => PreviewRequestDisposition::QueueFull,
            Err(TrySendError::Disconnected(_)) => PreviewRequestDisposition::Unavailable,
        }
    }

    pub(super) fn drain_ready(&mut self) -> Vec<DecodedPreview> {
        self.drain_ready_up_to(usize::MAX)
    }

    /// Returns at most `max_ready` decoded images while still consuming stale
    /// and unavailable worker results. Keeping the per-frame upload batch small
    /// prevents a burst of completed decodes from turning into one long UI
    /// frame. A limit of zero intentionally consumes no results.
    pub(super) fn drain_ready_up_to(&mut self, max_ready: usize) -> Vec<DecodedPreview> {
        let mut ready = Vec::with_capacity(max_ready.min(PREVIEW_QUEUE_CAPACITY));
        while ready.len() < max_ready {
            match self.result_rx.try_recv() {
                Ok(result) => {
                    self.in_flight.remove(&result.identity);
                    if result.identity.generation != self.generation {
                        continue;
                    }
                    match result.outcome {
                        PreviewDecodeOutcome::Ready(decoded) => ready.push(decoded),
                        PreviewDecodeOutcome::Unavailable => {
                            self.unavailable.insert(result.identity);
                        }
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        ready
    }

    /// Invalidates pending work after a new search so old results never allocate
    /// textures in the new result set.
    pub(super) fn advance_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.current_generation
            .store(self.generation, Ordering::Release);
        self.in_flight.clear();
        self.unavailable.clear();
        let _ = self.drain_ready();
    }
}

struct LatestOriginalQueue {
    pending: Option<PreviewWorkItem>,
    closed: bool,
}

/// Dedicated high-priority loader for full-resolution viewer images.
///
/// At most one image is being decoded and one image is waiting. A newer
/// navigation request replaces the waiting item, so the worker never spends
/// time decoding a backlog of originals the user has already moved past.
pub(super) struct OriginalLoader {
    queue: Arc<(Mutex<LatestOriginalQueue>, Condvar)>,
    result_rx: Receiver<PreviewWorkerResult>,
    generation: u64,
    current_generation: Arc<AtomicU64>,
    active: Option<PreviewIdentity>,
    unavailable: HashSet<String>,
}

impl OriginalLoader {
    pub(super) fn new() -> Self {
        Self::with_decoder(Arc::new(decode_preview))
    }

    fn with_decoder(decoder: PreviewDecoder) -> Self {
        let queue = Arc::new((
            Mutex::new(LatestOriginalQueue {
                pending: None,
                closed: false,
            }),
            Condvar::new(),
        ));
        let (result_tx, result_rx) = mpsc::sync_channel(2);
        let current_generation = Arc::new(AtomicU64::new(0));

        let worker_queue = Arc::clone(&queue);
        let worker_generation = Arc::clone(&current_generation);
        let _ = thread::Builder::new()
            .name("original-loader".to_string())
            .spawn(move || original_worker(worker_queue, result_tx, decoder, worker_generation));

        Self {
            queue,
            result_rx,
            generation: 0,
            current_generation,
            active: None,
            unavailable: HashSet::new(),
        }
    }

    pub(super) fn request(
        &mut self,
        source: PreviewSource,
        repaint: &egui::Context,
    ) -> PreviewRequestDisposition {
        debug_assert_eq!(source.mode, ImageLoadMode::Original);
        if self.unavailable.contains(&source.key) {
            return PreviewRequestDisposition::Unavailable;
        }
        if self
            .active
            .as_ref()
            .is_some_and(|identity| identity.key == source.key)
        {
            return PreviewRequestDisposition::AlreadyPending;
        }

        self.generation = self.generation.wrapping_add(1);
        self.current_generation
            .store(self.generation, Ordering::Release);
        let identity = PreviewIdentity {
            generation: self.generation,
            key: source.key.clone(),
        };
        let item = PreviewWorkItem {
            identity: identity.clone(),
            source,
            repaint: repaint.clone(),
            queued_at: Instant::now(),
        };

        let (queue, wake_worker) = &*self.queue;
        let mut state = queue.lock().unwrap();
        if state.closed {
            return PreviewRequestDisposition::Unavailable;
        }
        state.pending = Some(item);
        self.active = Some(identity);
        wake_worker.notify_one();
        PreviewRequestDisposition::Scheduled
    }

    pub(super) fn drain_ready(&mut self) -> Vec<DecodedPreview> {
        let mut ready = Vec::new();
        loop {
            match self.result_rx.try_recv() {
                Ok(result) => {
                    if self.active.as_ref() != Some(&result.identity) {
                        continue;
                    }
                    self.active = None;
                    match result.outcome {
                        PreviewDecodeOutcome::Ready(decoded) => ready.push(decoded),
                        PreviewDecodeOutcome::Unavailable => {
                            self.unavailable.insert(result.identity.key);
                        }
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        ready
    }

    pub(super) fn advance_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.current_generation
            .store(self.generation, Ordering::Release);
        self.active = None;
        self.unavailable.clear();
        let (queue, _) = &*self.queue;
        queue.lock().unwrap().pending = None;
        let _ = self.drain_ready();
    }
}

impl Drop for OriginalLoader {
    fn drop(&mut self) {
        let (queue, wake_worker) = &*self.queue;
        let mut state = queue.lock().unwrap();
        state.closed = true;
        state.pending = None;
        wake_worker.notify_one();
    }
}

fn original_worker(
    queue: Arc<(Mutex<LatestOriginalQueue>, Condvar)>,
    result_tx: SyncSender<PreviewWorkerResult>,
    decoder: PreviewDecoder,
    current_generation: Arc<AtomicU64>,
) {
    loop {
        let item = {
            let (queue, wake_worker) = &*queue;
            let mut state = queue.lock().unwrap();
            while state.pending.is_none() && !state.closed {
                state = wake_worker.wait(state).unwrap();
            }
            if state.closed {
                return;
            }
            state.pending.take().expect("pending original work")
        };

        let generation = item.identity.generation;
        if generation != current_generation.load(Ordering::Acquire) {
            continue;
        }
        let mut outcome = decode_with_queue_timing(&decoder, item.source, item.queued_at.elapsed());
        if generation != current_generation.load(Ordering::Acquire) {
            outcome = PreviewDecodeOutcome::Unavailable;
        }
        if result_tx
            .send(PreviewWorkerResult {
                identity: item.identity,
                outcome,
            })
            .is_err()
        {
            return;
        }
        item.repaint.request_repaint();
    }
}

fn preview_worker(
    request_rx: Arc<Mutex<Receiver<PreviewWorkItem>>>,
    result_tx: SyncSender<PreviewWorkerResult>,
    decoder: PreviewDecoder,
    current_generation: Arc<AtomicU64>,
) {
    loop {
        let item = {
            let receiver = request_rx.lock().unwrap();
            receiver.recv()
        };
        let Ok(item) = item else {
            break;
        };

        let outcome = if item.identity.generation == current_generation.load(Ordering::Acquire) {
            decode_with_queue_timing(&decoder, item.source, item.queued_at.elapsed())
        } else {
            PreviewDecodeOutcome::Unavailable
        };
        if result_tx
            .send(PreviewWorkerResult {
                identity: item.identity,
                outcome,
            })
            .is_err()
        {
            break;
        }
        item.repaint.request_repaint();
    }
}

fn decode_with_queue_timing(
    decoder: &PreviewDecoder,
    source: PreviewSource,
    queue_wait: Duration,
) -> PreviewDecodeOutcome {
    match decoder(source) {
        PreviewDecodeOutcome::Ready(mut decoded) => {
            decoded.timings.queue_wait = queue_wait;
            PreviewDecodeOutcome::Ready(decoded)
        }
        PreviewDecodeOutcome::Unavailable => PreviewDecodeOutcome::Unavailable,
    }
}

fn decode_preview(source: PreviewSource) -> PreviewDecodeOutcome {
    let mut timings = ImageLoadTimings::default();
    let lazy_thumbnail = source.thumb_dir.join(thumb_rel_path(source.image_id));
    let viewer_proxy = source
        .thumb_dir
        .join("viewer")
        .join(thumb_rel_path(source.image_id));
    let local_original = || {
        let local_path = source.local_file_path.as_deref()?;
        let local_path = PathBuf::from(local_path);
        let local_path = if local_path.is_absolute() {
            local_path
        } else {
            source.base_dir.join(&local_path)
        };
        local_path.is_file().then_some(local_path)
    };

    let (source_path, save_lazy_thumbnail, save_viewer_proxy) = match source.mode {
        ImageLoadMode::Original => {
            if source.max_side.is_some() && viewer_proxy.is_file() {
                (viewer_proxy.clone(), false, false)
            } else {
                let Some(local_path) = local_original() else {
                    return PreviewDecodeOutcome::Unavailable;
                };
                (local_path, false, source.max_side.is_some())
            }
        }
        ImageLoadMode::Preview => {
            if lazy_thumbnail.is_file() {
                (lazy_thumbnail.clone(), false, false)
            } else if let Some(thumbnail_path) = source
                .thumbnail_path
                .as_deref()
                .map(PathBuf::from)
                .filter(|path| path.is_file())
            {
                (thumbnail_path, false, false)
            } else if let Some(local_path) = local_original() {
                (local_path, true, false)
            } else {
                return PreviewDecodeOutcome::Unavailable;
            }
        }
    };

    if source.mode == ImageLoadMode::Original {
        let header_started = Instant::now();
        let Ok((width, height)) = image::image_dimensions(&source_path) else {
            return PreviewDecodeOutcome::Unavailable;
        };
        timings.header_read = header_started.elapsed();
        let Some(rgba_bytes) = rgba_byte_len(width, height) else {
            return PreviewDecodeOutcome::Unavailable;
        };
        if rgba_bytes > ORIGINAL_MAX_RGBA_BYTES {
            return PreviewDecodeOutcome::Unavailable;
        }
    }

    let decode_started = Instant::now();
    let Ok(raw) = image::open(&source_path) else {
        return PreviewDecodeOutcome::Unavailable;
    };
    timings.decode = decode_started.elapsed();
    let transform_started = Instant::now();
    let decoded = match source.mode {
        ImageLoadMode::Preview => raw.thumbnail(PREVIEW_MAX_SIDE, PREVIEW_MAX_SIDE),
        ImageLoadMode::Original => match source.max_side {
            Some(max_side) => raw.thumbnail(max_side, max_side),
            None => raw,
        },
    };
    timings.transform = transform_started.elapsed();

    if save_lazy_thumbnail {
        if let Some(parent) = lazy_thumbnail.parent() {
            if std::fs::create_dir_all(parent).is_ok() {
                let _ = decoded.save(&lazy_thumbnail);
            }
        }
    }
    if save_viewer_proxy {
        if let Some(parent) = viewer_proxy.parent() {
            if std::fs::create_dir_all(parent).is_ok() {
                let _ = decoded.save(&viewer_proxy);
            }
        }
    }

    let rgba_started = Instant::now();
    let rgba = decoded.into_rgba8();
    timings.rgba_conversion = rgba_started.elapsed();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let estimated_bytes = rgba.len();
    let color_image_started = Instant::now();
    let image = ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    timings.color_image_conversion = color_image_started.elapsed();
    PreviewDecodeOutcome::Ready(DecodedPreview {
        key: source.key,
        image_id: source.image_id,
        image,
        estimated_bytes,
        mode: source.mode,
        timings,
    })
}

fn rgba_byte_len(width: u32, height: u32) -> Option<usize> {
    (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Barrier;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use image::{ImageBuffer, Rgb};

    fn source(key: &str) -> PreviewSource {
        PreviewSource {
            key: key.to_string(),
            image_id: 1,
            base_dir: PathBuf::new(),
            thumb_dir: PathBuf::new(),
            local_file_path: None,
            thumbnail_path: None,
            mode: ImageLoadMode::Preview,
            max_side: None,
        }
    }

    fn ready_decoder() -> PreviewDecoder {
        Arc::new(|source| {
            PreviewDecodeOutcome::Ready(DecodedPreview {
                key: source.key,
                image_id: source.image_id,
                image: ColorImage::from_rgba_unmultiplied([1, 1], &[1, 2, 3, 255]),
                estimated_bytes: 4,
                mode: source.mode,
                timings: ImageLoadTimings::default(),
            })
        })
    }

    fn unique_test_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock drift")
            .as_nanos();
        std::env::temp_dir().join(format!("archivesift_preview_{label}_{nanos}"))
    }

    #[test]
    fn bounded_request_queue_rejects_excess_work() {
        let mut loader = PreviewLoader::with_decoder(0, 2, ready_decoder());
        let context = egui::Context::default();

        assert_eq!(
            loader.request(source("one"), &context),
            PreviewRequestDisposition::Scheduled
        );
        assert_eq!(
            loader.request(source("two"), &context),
            PreviewRequestDisposition::Scheduled
        );
        assert_eq!(
            loader.request(source("three"), &context),
            PreviewRequestDisposition::QueueFull
        );
    }

    #[test]
    fn identical_in_flight_request_is_deduplicated() {
        let mut loader = PreviewLoader::with_decoder(0, 2, ready_decoder());
        let context = egui::Context::default();

        assert_eq!(
            loader.request(source("same"), &context),
            PreviewRequestDisposition::Scheduled
        );
        assert_eq!(
            loader.request(source("same"), &context),
            PreviewRequestDisposition::AlreadyPending
        );
    }

    #[test]
    fn ready_results_are_drained_in_bounded_batches_without_loss() {
        let mut loader = PreviewLoader::with_decoder(1, 4, ready_decoder());
        let context = egui::Context::default();
        for key in ["one", "two", "three"] {
            assert_eq!(
                loader.request(source(key), &context),
                PreviewRequestDisposition::Scheduled
            );
        }

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut keys = Vec::new();
        while keys.len() < 3 && Instant::now() < deadline {
            let batch = loader.drain_ready_up_to(1);
            assert!(batch.len() <= 1);
            keys.extend(batch.into_iter().map(|decoded| decoded.key));
            if keys.len() < 3 {
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        assert_eq!(keys, ["one", "two", "three"]);
        assert!(loader.drain_ready_up_to(0).is_empty());
    }

    #[test]
    fn results_from_previous_generation_are_discarded() {
        let mut loader = PreviewLoader::with_decoder(1, 2, ready_decoder());
        let context = egui::Context::default();
        assert_eq!(
            loader.request(source("old"), &context),
            PreviewRequestDisposition::Scheduled
        );

        loader.advance_generation();
        std::thread::sleep(Duration::from_millis(20));

        assert!(loader.drain_ready().is_empty());
    }

    #[test]
    fn queued_work_from_previous_generation_is_skipped_before_decode() {
        let started = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let calls = Arc::new(AtomicUsize::new(0));
        let decoder: PreviewDecoder = {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let calls = Arc::clone(&calls);
            Arc::new(move |source| {
                calls.fetch_add(1, AtomicOrdering::Relaxed);
                if source.key == "blocker" {
                    started.wait();
                    release.wait();
                }
                PreviewDecodeOutcome::Ready(DecodedPreview {
                    key: source.key,
                    image_id: source.image_id,
                    image: ColorImage::from_rgba_unmultiplied([1, 1], &[1, 2, 3, 255]),
                    estimated_bytes: 4,
                    mode: source.mode,
                    timings: ImageLoadTimings::default(),
                })
            })
        };
        let mut loader = PreviewLoader::with_decoder(1, 4, decoder);
        let context = egui::Context::default();
        assert_eq!(
            loader.request(source("blocker"), &context),
            PreviewRequestDisposition::Scheduled
        );
        started.wait();
        assert_eq!(
            loader.request(source("stale-queued"), &context),
            PreviewRequestDisposition::Scheduled
        );

        loader.advance_generation();
        release.wait();
        std::thread::sleep(Duration::from_millis(20));
        let _ = loader.drain_ready();

        assert_eq!(calls.load(AtomicOrdering::Relaxed), 1);
    }

    #[test]
    fn original_loader_replaces_waiting_work_with_the_latest_request() {
        let started = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let decoded_keys = Arc::new(Mutex::new(Vec::<String>::new()));
        let decoder: PreviewDecoder = {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let decoded_keys = Arc::clone(&decoded_keys);
            Arc::new(move |source| {
                decoded_keys.lock().unwrap().push(source.key.clone());
                if source.key == "blocker" {
                    started.wait();
                    release.wait();
                }
                PreviewDecodeOutcome::Ready(DecodedPreview {
                    key: source.key,
                    image_id: source.image_id,
                    image: ColorImage::from_rgba_unmultiplied([1, 1], &[1, 2, 3, 255]),
                    estimated_bytes: 4,
                    mode: source.mode,
                    timings: ImageLoadTimings::default(),
                })
            })
        };
        let mut loader = OriginalLoader::with_decoder(decoder);
        let context = egui::Context::default();
        let mut blocker = source("blocker");
        blocker.mode = ImageLoadMode::Original;
        assert_eq!(
            loader.request(blocker, &context),
            PreviewRequestDisposition::Scheduled
        );
        started.wait();

        let mut stale = source("stale");
        stale.mode = ImageLoadMode::Original;
        assert_eq!(
            loader.request(stale, &context),
            PreviewRequestDisposition::Scheduled
        );
        let mut latest = source("latest");
        latest.mode = ImageLoadMode::Original;
        assert_eq!(
            loader.request(latest, &context),
            PreviewRequestDisposition::Scheduled
        );
        release.wait();

        let deadline = Instant::now() + Duration::from_secs(1);
        let ready = loop {
            let ready = loader.drain_ready();
            if !ready.is_empty() || Instant::now() >= deadline {
                break ready;
            }
            std::thread::sleep(Duration::from_millis(5));
        };

        assert_eq!(
            decoded_keys.lock().unwrap().as_slice(),
            ["blocker", "latest"]
        );
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].key, "latest");
    }

    #[test]
    fn original_loader_deduplicates_the_active_request() {
        let blocker = Arc::new(Barrier::new(2));
        let decoder: PreviewDecoder = {
            let blocker = Arc::clone(&blocker);
            Arc::new(move |source| {
                blocker.wait();
                PreviewDecodeOutcome::Ready(DecodedPreview {
                    key: source.key,
                    image_id: source.image_id,
                    image: ColorImage::from_rgba_unmultiplied([1, 1], &[1, 2, 3, 255]),
                    estimated_bytes: 4,
                    mode: source.mode,
                    timings: ImageLoadTimings::default(),
                })
            })
        };
        let mut loader = OriginalLoader::with_decoder(decoder);
        let context = egui::Context::default();
        let mut request = source("same-original");
        request.mode = ImageLoadMode::Original;

        assert_eq!(
            loader.request(request.clone(), &context),
            PreviewRequestDisposition::Scheduled
        );
        assert_eq!(
            loader.request(request, &context),
            PreviewRequestDisposition::AlreadyPending
        );
        blocker.wait();
    }

    #[test]
    fn queue_wait_is_attached_to_decoder_metrics() {
        let expected = Duration::from_millis(17);
        let PreviewDecodeOutcome::Ready(decoded) =
            decode_with_queue_timing(&ready_decoder(), source("timed"), expected)
        else {
            panic!("decoder should return a ready image");
        };

        assert_eq!(decoded.timings.queue_wait, expected);
    }

    #[test]
    fn full_image_is_resized_and_thumbnail_is_saved_by_worker_decoder() {
        let root = unique_test_root("decode");
        let original = root.join("downloads").join("large.png");
        std::fs::create_dir_all(original.parent().expect("original parent"))
            .expect("create original directory");
        ImageBuffer::from_pixel(800, 600, Rgb([10u8, 20, 30]))
            .save(&original)
            .expect("save source image");
        let thumb_dir = root.join("thumbs");
        let request = PreviewSource {
            key: "image-7".to_string(),
            image_id: 7,
            base_dir: root.clone(),
            thumb_dir: thumb_dir.clone(),
            local_file_path: Some("downloads/large.png".to_string()),
            thumbnail_path: None,
            mode: ImageLoadMode::Preview,
            max_side: None,
        };

        let PreviewDecodeOutcome::Ready(decoded) = decode_preview(request) else {
            panic!("preview should decode");
        };

        assert_eq!(decoded.mode, ImageLoadMode::Preview);
        assert!(decoded.image.size[0] <= PREVIEW_MAX_SIDE as usize);
        assert!(decoded.image.size[1] <= PREVIEW_MAX_SIDE as usize);
        assert_eq!(
            decoded.estimated_bytes,
            decoded.image.size[0] * decoded.image.size[1] * 4
        );
        assert!(thumb_dir.join(thumb_rel_path(7)).is_file());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn original_mode_preserves_dimensions_and_does_not_create_thumbnail() {
        let root = unique_test_root("original");
        let original = root.join("downloads").join("large.png");
        std::fs::create_dir_all(original.parent().expect("original parent"))
            .expect("create original directory");
        ImageBuffer::from_pixel(800, 600, Rgb([10u8, 20, 30]))
            .save(&original)
            .expect("save source image");
        let thumb_dir = root.join("thumbs");
        let request = PreviewSource {
            key: "original-image-7".to_string(),
            image_id: 7,
            base_dir: root.clone(),
            thumb_dir: thumb_dir.clone(),
            local_file_path: Some("downloads/large.png".to_string()),
            thumbnail_path: None,
            mode: ImageLoadMode::Original,
            max_side: None,
        };

        let PreviewDecodeOutcome::Ready(decoded) = decode_preview(request) else {
            panic!("original should decode");
        };

        assert_eq!(decoded.mode, ImageLoadMode::Original);
        assert_eq!(decoded.image.size, [800, 600]);
        assert_eq!(decoded.estimated_bytes, 800 * 600 * 4);
        assert!(!thumb_dir.join(thumb_rel_path(7)).exists());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn viewer_proxy_is_bounded_and_reused_from_disk() {
        let root = unique_test_root("viewer_proxy");
        let original = root.join("downloads").join("wide.png");
        std::fs::create_dir_all(original.parent().expect("original parent")).unwrap();
        ImageBuffer::from_pixel(5_000, 20, Rgb([10u8, 20, 30]))
            .save(&original)
            .unwrap();
        let request = PreviewSource {
            key: "proxy-image-11".to_string(),
            image_id: 11,
            base_dir: root.clone(),
            thumb_dir: root.join("thumbs"),
            local_file_path: Some("downloads/wide.png".to_string()),
            thumbnail_path: None,
            mode: ImageLoadMode::Original,
            max_side: Some(VIEWER_PROXY_MAX_SIDE),
        };

        let PreviewDecodeOutcome::Ready(first) = decode_preview(request.clone()) else {
            panic!("viewer proxy should decode");
        };
        assert_eq!(first.image.size[0], VIEWER_PROXY_MAX_SIDE as usize);
        let proxy = root.join("thumbs").join("viewer").join(thumb_rel_path(11));
        assert!(proxy.is_file());

        std::fs::remove_file(&original).unwrap();
        assert!(matches!(
            decode_preview(request),
            PreviewDecodeOutcome::Ready(_)
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn original_mode_ignores_existing_thumbnail() {
        let root = unique_test_root("original_ignores_thumb");
        let original = root.join("downloads").join("large.png");
        std::fs::create_dir_all(original.parent().expect("original parent"))
            .expect("create original directory");
        ImageBuffer::from_pixel(640, 480, Rgb([10u8, 20, 30]))
            .save(&original)
            .expect("save source image");

        let thumb_dir = root.join("thumbs");
        let thumbnail = thumb_dir.join(thumb_rel_path(8));
        std::fs::create_dir_all(thumbnail.parent().expect("thumbnail parent"))
            .expect("create thumbnail directory");
        ImageBuffer::from_pixel(40, 30, Rgb([30u8, 20, 10]))
            .save(&thumbnail)
            .expect("save thumbnail");

        let request = PreviewSource {
            key: "original-image-8".to_string(),
            image_id: 8,
            base_dir: root.clone(),
            thumb_dir,
            local_file_path: Some("downloads/large.png".to_string()),
            thumbnail_path: Some(thumbnail.to_string_lossy().into_owned()),
            mode: ImageLoadMode::Original,
            max_side: None,
        };

        let PreviewDecodeOutcome::Ready(decoded) = decode_preview(request) else {
            panic!("original should decode");
        };

        assert_eq!(decoded.image.size, [640, 480]);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn original_mode_never_falls_back_to_a_thumbnail() {
        let root = unique_test_root("original_no_fallback");
        let thumbnail = root.join("existing-thumb.png");
        std::fs::create_dir_all(&root).expect("create test directory");
        ImageBuffer::from_pixel(40, 30, Rgb([30u8, 20, 10]))
            .save(&thumbnail)
            .expect("save thumbnail");
        let request = PreviewSource {
            key: "missing-original".to_string(),
            image_id: 9,
            base_dir: root.clone(),
            thumb_dir: root.join("thumbs"),
            local_file_path: None,
            thumbnail_path: Some(thumbnail.to_string_lossy().into_owned()),
            mode: ImageLoadMode::Original,
            max_side: None,
        };

        assert!(matches!(
            decode_preview(request),
            PreviewDecodeOutcome::Unavailable
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rgba_size_calculation_rejects_overflow() {
        assert_eq!(rgba_byte_len(400, 300), Some(480_000));
        #[cfg(target_pointer_width = "32")]
        assert_eq!(rgba_byte_len(u32::MAX, u32::MAX), None);
    }
}
