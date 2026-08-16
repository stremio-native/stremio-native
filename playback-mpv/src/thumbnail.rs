//! Persistent secondary libmpv worker for timeline thumbnail previews.

use std::{
    collections::VecDeque,
    ffi::{CStr, c_void},
    fmt, slice,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::ffi::{
    EVENT_END_FILE, EVENT_FILE_LOADED, EVENT_NONE, EVENT_PLAYBACK_RESTART, EVENT_SHUTDOWN,
    FORMAT_BYTE_ARRAY, FORMAT_INT64, FORMAT_NODE_MAP, FORMAT_STRING, MpvApi, MpvClient, MpvError,
    MpvEventEndFile, MpvNode, MpvNodeList,
};

const CACHE_BUCKET_SECONDS: f64 = 0.1;
const FAST_SEEK_MINIMUM_DURATION: f64 = 30.0;

/// Resource and scheduling limits for the thumbnail decoder.
#[derive(Clone, Debug)]
pub struct ThumbnailConfig {
    pub max_width: u32,
    pub max_height: u32,
    pub cache_capacity_bytes: usize,
    pub fast_seek_interval: Duration,
    pub exact_seek_delay: Duration,
    pub load_timeout: Duration,
    pub seek_timeout: Duration,
    pub hardware_decoding: bool,
    pub max_source_attempts: u8,
    pub max_request_attempts: u8,
    pub retry_delay: Duration,
}

impl Default for ThumbnailConfig {
    fn default() -> Self {
        Self {
            max_width: 320,
            max_height: 200,
            cache_capacity_bytes: 16 * 1024 * 1024,
            fast_seek_interval: Duration::from_millis(50),
            exact_seek_delay: Duration::from_millis(100),
            load_timeout: Duration::from_secs(15),
            seek_timeout: Duration::from_secs(15),
            hardware_decoding: false,
            max_source_attempts: 2,
            max_request_attempts: 2,
            retry_delay: Duration::from_millis(250),
        }
    }
}

/// A media source assigned to the thumbnail worker.
#[derive(Clone, Debug)]
pub struct ThumbnailSource {
    pub generation: u64,
    pub url: String,
    pub initial_position: f64,
}

/// A request for the frame nearest a playback timestamp.
#[derive(Clone, Copy, Debug)]
pub struct ThumbnailRequest {
    pub generation: u64,
    pub request_id: u64,
    pub seconds: f64,
}

/// How a returned thumbnail was obtained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThumbnailQuality {
    Fast,
    Exact,
    Cached,
}

/// A tightly packed, top-down RGBA thumbnail.
#[derive(Clone)]
pub struct ThumbnailFrame {
    pub generation: u64,
    pub request_id: u64,
    pub requested_seconds: f64,
    pub decoded_seconds: f64,
    pub width: u32,
    pub height: u32,
    pub quality: ThumbnailQuality,
    pub rgba: Arc<[u8]>,
}

impl fmt::Debug for ThumbnailFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ThumbnailFrame")
            .field("generation", &self.generation)
            .field("request_id", &self.request_id)
            .field("requested_seconds", &self.requested_seconds)
            .field("decoded_seconds", &self.decoded_seconds)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("quality", &self.quality)
            .field("rgba_bytes", &self.rgba.len())
            .finish()
    }
}

/// Why thumbnails are unavailable for a source or request.
#[derive(Clone, Debug)]
pub enum ThumbnailUnavailableReason {
    NoVideo,
    NotSeekable,
    LoadFailed(String),
    SeekFailed(String),
    ScreenshotFailed(String),
    InvalidFrame(String),
}

/// Events emitted from the thumbnail worker thread.
#[derive(Clone, Debug)]
pub enum ThumbnailEvent {
    WorkerReady,
    SourceReady {
        generation: u64,
        duration: f64,
    },
    SourceUnavailable {
        generation: u64,
        reason: ThumbnailUnavailableReason,
    },
    Frame(ThumbnailFrame),
    RequestFailed {
        generation: u64,
        request_id: u64,
        reason: ThumbnailUnavailableReason,
    },
    Shutdown,
}

#[derive(Clone)]
pub struct ThumbnailController {
    shared: Arc<SharedMailbox>,
}

impl ThumbnailController {
    /// Replaces the worker's media source and invalidates all older work.
    pub fn load_source(&self, source: ThumbnailSource) -> Result<(), MpvError> {
        let mut mailbox = lock_mailbox(&self.shared);
        ensure_open(&mailbox)?;
        mailbox.source_revision = mailbox.source_revision.wrapping_add(1);
        mailbox.request_revision = mailbox.request_revision.wrapping_add(1);
        mailbox.latest_request = None;
        let source_revision = mailbox.source_revision;
        mailbox
            .controls
            .push_back(Control::Load(source_revision, source));
        drop(mailbox);
        self.shared.condvar.notify_one();
        Ok(())
    }

    /// Replaces any pending hover request with the newest timestamp.
    pub fn request(&self, request: ThumbnailRequest) -> Result<(), MpvError> {
        let mut mailbox = lock_mailbox(&self.shared);
        ensure_open(&mailbox)?;
        mailbox.request_revision = mailbox.request_revision.wrapping_add(1);
        if mailbox.latest_request.is_some() {
            mailbox.coalesced_requests = mailbox.coalesced_requests.saturating_add(1);
        }
        mailbox.latest_request = Some(VersionedRequest {
            revision: mailbox.request_revision,
            received_at: Instant::now(),
            request,
        });
        drop(mailbox);
        self.shared.condvar.notify_one();
        Ok(())
    }

    /// Cancels pending/refining requests while keeping the source prewarmed.
    pub fn clear(&self) -> Result<(), MpvError> {
        let mut mailbox = lock_mailbox(&self.shared);
        ensure_open(&mailbox)?;
        mailbox.request_revision = mailbox.request_revision.wrapping_add(1);
        mailbox.latest_request = None;
        mailbox.controls.push_back(Control::Clear);
        drop(mailbox);
        self.shared.condvar.notify_one();
        Ok(())
    }

    /// Stops and releases the active source, requests, and frame cache.
    pub fn unload(&self) -> Result<(), MpvError> {
        let mut mailbox = lock_mailbox(&self.shared);
        ensure_open(&mailbox)?;
        mailbox.source_revision = mailbox.source_revision.wrapping_add(1);
        mailbox.request_revision = mailbox.request_revision.wrapping_add(1);
        mailbox.latest_request = None;
        let source_revision = mailbox.source_revision;
        mailbox.controls.push_back(Control::Unload(source_revision));
        drop(mailbox);
        self.shared.condvar.notify_one();
        Ok(())
    }

    fn shutdown(&self) {
        let mut mailbox = lock_mailbox(&self.shared);
        if mailbox.closed {
            return;
        }
        mailbox.closed = true;
        mailbox.source_revision = mailbox.source_revision.wrapping_add(1);
        mailbox.request_revision = mailbox.request_revision.wrapping_add(1);
        mailbox.latest_request = None;
        mailbox.controls.push_back(Control::Shutdown);
        drop(mailbox);
        self.shared.condvar.notify_all();
    }
}

/// Owns the persistent decoder thread and joins it on shutdown or drop.
pub struct ThumbnailRuntime {
    controller: ThumbnailController,
    worker: Option<JoinHandle<()>>,
}

impl ThumbnailRuntime {
    /// Initializes a separate MPV client and starts its worker.
    ///
    /// The worker decodes video with audio disabled, so it always uses the
    /// pinned runtime rather than whichever module playback selected: a
    /// screenshot defect in a swapped-in build would otherwise crash the whole
    /// application while a viewer merely hovers the timeline.
    pub fn start(
        config: ThumbnailConfig,
        event_sink: impl Fn(ThumbnailEvent) + Send + Sync + 'static,
    ) -> Result<Self, MpvError> {
        let started_at = Instant::now();
        validate_config(&config)?;
        let api = MpvApi::pinned_runtime()?;
        let client = MpvClient::create(api)?;
        configure_client(&client, &config)?;
        client.initialize()?;

        let shared = Arc::new(SharedMailbox::default());
        client.set_wakeup_callback(
            Some(wakeup_thumbnail),
            Arc::as_ptr(&shared).cast_mut().cast::<c_void>(),
        );
        let controller = ThumbnailController {
            shared: shared.clone(),
        };
        let sink: Arc<dyn Fn(ThumbnailEvent) + Send + Sync> = Arc::new(event_sink);
        let worker = thread::Builder::new()
            .name("mpv-thumbnail".to_owned())
            .spawn(move || {
                worker_main(client, shared, config, sink, started_at.elapsed());
            })
            .map_err(|error| MpvError::InvalidNode(format!("could not start worker: {error}")))?;

        Ok(Self {
            controller,
            worker: Some(worker),
        })
    }

    /// Returns a clonable handle for source and request control.
    pub fn controller(&self) -> ThumbnailController {
        self.controller.clone()
    }

    /// Requests shutdown and joins the worker thread.
    pub fn shutdown(mut self) -> Result<(), MpvError> {
        self.join_worker()
    }

    fn join_worker(&mut self) -> Result<(), MpvError> {
        self.controller.shutdown();
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker.join().map_err(|_| MpvError::ThumbnailWorkerPanicked)
    }
}

impl Drop for ThumbnailRuntime {
    fn drop(&mut self) {
        let _ = self.join_worker();
    }
}

#[derive(Default)]
struct SharedMailbox {
    mailbox: Mutex<Mailbox>,
    condvar: Condvar,
    mpv_wakeup_revision: AtomicU64,
}

#[derive(Default)]
struct Mailbox {
    controls: VecDeque<Control>,
    latest_request: Option<VersionedRequest>,
    source_revision: u64,
    request_revision: u64,
    coalesced_requests: u64,
    closed: bool,
}

enum Control {
    Load(u64, ThumbnailSource),
    Clear,
    Unload(u64),
    Shutdown,
}

#[derive(Clone, Copy)]
struct VersionedRequest {
    revision: u64,
    received_at: Instant,
    request: ThumbnailRequest,
}

unsafe extern "C" fn wakeup_thumbnail(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: The callback is unregistered before the worker releases the
    // `Arc<SharedMailbox>` whose stable allocation was passed as context.
    let shared = unsafe { &*context.cast::<SharedMailbox>() };
    shared.mpv_wakeup_revision.fetch_add(1, Ordering::Release);
    shared.condvar.notify_one();
}

fn lock_mailbox(shared: &SharedMailbox) -> MutexGuard<'_, Mailbox> {
    shared
        .mailbox
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn ensure_open(mailbox: &Mailbox) -> Result<(), MpvError> {
    if mailbox.closed {
        Err(MpvError::ThumbnailWorkerClosed)
    } else {
        Ok(())
    }
}

fn validate_config(config: &ThumbnailConfig) -> Result<(), MpvError> {
    if config.max_width == 0 || config.max_height == 0 {
        return Err(MpvError::InvalidNode(
            "thumbnail bounds must be non-zero".to_owned(),
        ));
    }
    let _ = usize::try_from(config.max_width)
        .ok()
        .and_then(|width| {
            usize::try_from(config.max_height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| MpvError::InvalidNode("thumbnail bounds overflow memory size".to_owned()))?;
    if config.max_source_attempts == 0 || config.max_request_attempts == 0 {
        return Err(MpvError::InvalidNode(
            "thumbnail attempt counts must be non-zero".to_owned(),
        ));
    }
    Ok(())
}

fn configure_client(client: &MpvClient, config: &ThumbnailConfig) -> Result<(), MpvError> {
    let options = [
        ("config", "no"),
        ("load-scripts", "no"),
        ("terminal", "no"),
        ("input-default-bindings", "no"),
        ("input-vo-keyboard", "no"),
        ("osc", "no"),
        ("idle", "yes"),
        ("pause", "yes"),
        ("keep-open", "yes"),
        ("vo", "null"),
        ("audio", "no"),
        ("ad", "no"),
        ("aid", "no"),
        ("sid", "no"),
        ("sub-auto", "no"),
        ("ytdl", "no"),
        ("hwdec", thumbnail_hwdec(config.hardware_decoding)),
        ("screenshot-sw", "yes"),
        ("vd-lavc-threads", "2"),
        ("vd-lavc-fast", "yes"),
        ("vd-lavc-skiploopfilter", "all"),
        ("vd-lavc-dr", "no"),
        ("cache", "yes"),
        ("cache-pause", "no"),
        ("cache-secs", "0"),
        ("demuxer-readahead-secs", "0"),
        ("demuxer-max-bytes", "8388608"),
        ("demuxer-max-back-bytes", "8388608"),
        ("sws-scaler", "fast-bilinear"),
    ];
    for (name, value) in options {
        client.set_option(name, value)?;
    }
    Ok(())
}

const fn thumbnail_hwdec(hardware_decoding: bool) -> &'static str {
    if hardware_decoding { "auto-copy" } else { "no" }
}

struct LoadedSource {
    generation: u64,
    duration: f64,
    rotation: u16,
}

struct WorkerState {
    source: Option<LoadedSource>,
    cache: FrameCache,
    last_seek_started: Option<Instant>,
    hdr_filter_warning_emitted: bool,
}

fn worker_main(
    client: Arc<MpvClient>,
    shared: Arc<SharedMailbox>,
    config: ThumbnailConfig,
    sink: Arc<dyn Fn(ThumbnailEvent) + Send + Sync>,
    initialization_time: Duration,
) {
    eprintln!(
        "thumbnail_worker_ready initialization_ms={}",
        initialization_time.as_millis()
    );
    sink(ThumbnailEvent::WorkerReady);
    let mut state = WorkerState {
        source: None,
        cache: FrameCache::new(config.cache_capacity_bytes),
        last_seek_started: None,
        hdr_filter_warning_emitted: false,
    };

    let mut running = true;
    while running {
        if let Some(control) = take_control(&shared) {
            running = handle_control(&client, &shared, &config, &sink, &mut state, control);
            continue;
        }

        if let Some(request) = take_latest_request(&shared) {
            process_request(&client, &shared, &config, &sink, &mut state, request);
            continue;
        }

        drain_unhandled_events(&client);
        wait_for_activity(&shared);
    }

    client.set_wakeup_callback(None, std::ptr::null_mut());
    let _ = client.command(&["stop"]);
    sink(ThumbnailEvent::Shutdown);
}

fn take_control(shared: &SharedMailbox) -> Option<Control> {
    lock_mailbox(shared).controls.pop_front()
}

fn take_latest_request(shared: &SharedMailbox) -> Option<VersionedRequest> {
    let mut mailbox = lock_mailbox(shared);
    let request = mailbox.latest_request.take();
    let coalesced = std::mem::take(&mut mailbox.coalesced_requests);
    if coalesced > 0 {
        eprintln!("thumbnail_requests_coalesced count={coalesced}");
    }
    request
}

fn handle_control(
    client: &MpvClient,
    shared: &SharedMailbox,
    config: &ThumbnailConfig,
    sink: &Arc<dyn Fn(ThumbnailEvent) + Send + Sync>,
    state: &mut WorkerState,
    control: Control,
) -> bool {
    match control {
        Control::Load(source_revision, source) => {
            state.source = None;
            state.cache.clear();
            let _ = client.command(&["stop"]);
            match load_source_with_retry(client, shared, config, state, source_revision, &source) {
                Ok(Some(loaded)) => {
                    let generation = loaded.generation;
                    let duration = loaded.duration;
                    state.source = Some(loaded);
                    if is_source_current(shared, source_revision) {
                        sink(ThumbnailEvent::SourceReady {
                            generation,
                            duration,
                        });
                    }
                }
                Ok(None) => {}
                Err(reason) => {
                    if is_source_current(shared, source_revision) {
                        sink(ThumbnailEvent::SourceUnavailable {
                            generation: source.generation,
                            reason,
                        });
                    }
                }
            }
            true
        }
        Control::Clear => true,
        Control::Unload(source_revision) => {
            state.source = None;
            state.cache.clear();
            if is_source_current(shared, source_revision) {
                let _ = client.command(&["stop"]);
            }
            true
        }
        Control::Shutdown => false,
    }
}

fn load_source_with_retry(
    client: &MpvClient,
    shared: &SharedMailbox,
    config: &ThumbnailConfig,
    state: &mut WorkerState,
    source_revision: u64,
    source: &ThumbnailSource,
) -> Result<Option<LoadedSource>, ThumbnailUnavailableReason> {
    for attempt in 1..=config.max_source_attempts {
        match load_source(client, shared, config, state, source_revision, source) {
            Ok(result) => return Ok(result),
            Err(reason) if source_retry_permitted(&reason, attempt, config.max_source_attempts) => {
                if !wait_source_interruptible(shared, config.retry_delay, source_revision) {
                    return Ok(None);
                }
                let _ = client.command(&["stop"]);
            }
            Err(reason) => return Err(reason),
        }
    }
    unreachable!("validated source attempt count is non-zero")
}

fn source_retry_permitted(
    reason: &ThumbnailUnavailableReason,
    attempt: u8,
    max_attempts: u8,
) -> bool {
    attempt < max_attempts && matches!(reason, ThumbnailUnavailableReason::LoadFailed(_))
}

fn load_source(
    client: &MpvClient,
    shared: &SharedMailbox,
    config: &ThumbnailConfig,
    state: &mut WorkerState,
    source_revision: u64,
    source: &ThumbnailSource,
) -> Result<Option<LoadedSource>, ThumbnailUnavailableReason> {
    let start = finite_non_negative(source.initial_position);
    let load_options = format!("start={start:.6}");
    // MPV 0.38 and newer place the playlist index before per-file options:
    // loadfile <url> <flags> <index> <options>.
    client
        .command(&["loadfile", &source.url, "replace", "-1", &load_options])
        .map_err(|error| ThumbnailUnavailableReason::LoadFailed(error.to_string()))?;
    match wait_for_event(
        client,
        shared,
        EVENT_FILE_LOADED,
        Instant::now() + config.load_timeout,
        Cancellation::Source(source_revision),
    ) {
        WaitOutcome::Reached => {}
        WaitOutcome::Cancelled => return Ok(None),
        WaitOutcome::TimedOut => {
            return Err(ThumbnailUnavailableReason::LoadFailed(
                "timed out waiting for the source".to_owned(),
            ));
        }
        WaitOutcome::Failed(message) => {
            return Err(ThumbnailUnavailableReason::LoadFailed(message));
        }
    }

    let width = client
        .get_i64("video-params/dw")
        .or_else(|_| client.get_i64("video-params/w"))
        .unwrap_or(0);
    let height = client
        .get_i64("video-params/dh")
        .or_else(|_| client.get_i64("video-params/h"))
        .unwrap_or(0);
    if width <= 0 || height <= 0 {
        return Err(ThumbnailUnavailableReason::NoVideo);
    }
    if !client.get_flag("seekable").unwrap_or(false) {
        return Err(ThumbnailUnavailableReason::NotSeekable);
    }
    let duration = client.get_double("duration").unwrap_or(0.0);
    if !duration.is_finite() || duration <= 0.0 {
        return Err(ThumbnailUnavailableReason::NotSeekable);
    }

    let rotation = normalize_rotation(client.get_i64("video-params/rotate").unwrap_or(0));
    let primaries = client
        .get_string("video-params/primaries")
        .unwrap_or_default();
    let transfer = client.get_string("video-params/gamma").unwrap_or_default();
    let hdr = primaries.eq_ignore_ascii_case("bt.2020")
        || transfer.eq_ignore_ascii_case("pq")
        || transfer.eq_ignore_ascii_case("hlg");
    apply_scale_filter(client, config, rotation, hdr, state)?;

    let position = start.min((duration - 0.001).max(0.0));
    seek(client, position, "absolute+exact")
        .map_err(|error| ThumbnailUnavailableReason::LoadFailed(error.to_string()))?;
    match wait_for_event(
        client,
        shared,
        EVENT_PLAYBACK_RESTART,
        Instant::now() + config.seek_timeout,
        Cancellation::Source(source_revision),
    ) {
        WaitOutcome::Reached => Ok(Some(LoadedSource {
            generation: source.generation,
            duration,
            rotation,
        })),
        WaitOutcome::Cancelled => Ok(None),
        WaitOutcome::TimedOut => Err(ThumbnailUnavailableReason::LoadFailed(
            "timed out prewarming the first frame".to_owned(),
        )),
        WaitOutcome::Failed(message) => Err(ThumbnailUnavailableReason::LoadFailed(message)),
    }
}

fn apply_scale_filter(
    client: &MpvClient,
    config: &ThumbnailConfig,
    rotation: u16,
    hdr: bool,
    state: &mut WorkerState,
) -> Result<(), ThumbnailUnavailableReason> {
    let (width, height) = if matches!(rotation, 90 | 270) {
        (config.max_height, config.max_width)
    } else {
        (config.max_width, config.max_height)
    };
    let scale = format!("scale=w={width}:h={height}:force_original_aspect_ratio=decrease");
    if hdr {
        let hdr_filter = format!(
            "zscale=transfer=linear,format=gbrpf32le,tonemap=hable,zscale=transfer=bt709,{scale}"
        );
        if client.set_string("vf", &hdr_filter).is_ok() {
            return Ok(());
        }
        if !state.hdr_filter_warning_emitted {
            eprintln!("thumbnail_hdr_filter_unavailable fallback=ordinary_conversion");
            state.hdr_filter_warning_emitted = true;
        }
    }
    client
        .set_string("vf", &scale)
        .map_err(|error| ThumbnailUnavailableReason::LoadFailed(error.to_string()))
}

fn process_request(
    client: &MpvClient,
    shared: &SharedMailbox,
    config: &ThumbnailConfig,
    sink: &Arc<dyn Fn(ThumbnailEvent) + Send + Sync>,
    state: &mut WorkerState,
    versioned: VersionedRequest,
) {
    let Some(source) = state.source.as_ref() else {
        return;
    };
    let request = versioned.request;
    if request.generation != source.generation || !request.seconds.is_finite() {
        return;
    }
    let source_duration = source.duration;
    let source_rotation = source.rotation;
    let seconds = request
        .seconds
        .clamp(0.0, (source_duration - 0.001).max(0.0));
    let key = CacheKey::new(request.generation, seconds);
    if let Some(cached) = state.cache.get(key) {
        if is_request_current(shared, versioned.revision) {
            sink(ThumbnailEvent::Frame(
                cached.into_frame(request, ThumbnailQuality::Cached),
            ));
        }
        return;
    }

    if uses_fast_stage(source_duration) {
        if !perform_seek_and_capture(
            client,
            shared,
            config,
            sink,
            state,
            source_rotation,
            versioned,
            seconds,
            "absolute+keyframes",
            ThumbnailQuality::Fast,
        ) {
            return;
        }
        if !wait_until_exact_due(shared, versioned, config.exact_seek_delay) {
            return;
        }
    }

    let _ = perform_seek_and_capture(
        client,
        shared,
        config,
        sink,
        state,
        source_rotation,
        versioned,
        seconds,
        "absolute+exact",
        ThumbnailQuality::Exact,
    );
}

fn uses_fast_stage(duration: f64) -> bool {
    duration >= FAST_SEEK_MINIMUM_DURATION
}

#[allow(clippy::too_many_arguments)]
fn perform_seek_and_capture(
    client: &MpvClient,
    shared: &SharedMailbox,
    config: &ThumbnailConfig,
    sink: &Arc<dyn Fn(ThumbnailEvent) + Send + Sync>,
    state: &mut WorkerState,
    rotation: u16,
    versioned: VersionedRequest,
    seconds: f64,
    seek_mode: &str,
    quality: ThumbnailQuality,
) -> bool {
    for attempt in 1..=config.max_request_attempts {
        match seek_and_capture_once(
            client, shared, config, state, rotation, versioned, seconds, seek_mode, quality,
        ) {
            RequestAttempt::Captured(captured) => {
                if quality == ThumbnailQuality::Exact {
                    state.cache.insert(
                        CacheKey::new(versioned.request.generation, seconds),
                        CachedFrame::from_frame(&captured),
                    );
                }
                sink(ThumbnailEvent::Frame(captured));
                return true;
            }
            RequestAttempt::Cancelled => return false,
            RequestAttempt::Failed(reason)
                if request_retry_permitted(&reason, attempt, config.max_request_attempts) =>
            {
                if !wait_interruptible(shared, config.retry_delay, versioned.revision) {
                    return false;
                }
            }
            RequestAttempt::Failed(reason) => {
                emit_request_failure(shared, sink, versioned, reason);
                return false;
            }
        }
    }
    unreachable!("validated request attempt count is non-zero")
}

enum RequestAttempt {
    Captured(ThumbnailFrame),
    Cancelled,
    Failed(ThumbnailUnavailableReason),
}

#[allow(clippy::too_many_arguments)]
fn seek_and_capture_once(
    client: &MpvClient,
    shared: &SharedMailbox,
    config: &ThumbnailConfig,
    state: &mut WorkerState,
    rotation: u16,
    versioned: VersionedRequest,
    seconds: f64,
    seek_mode: &str,
    quality: ThumbnailQuality,
) -> RequestAttempt {
    if !throttle_seek(shared, versioned.revision, state, config.fast_seek_interval) {
        return RequestAttempt::Cancelled;
    }
    if let Err(error) = seek(client, seconds, seek_mode) {
        return RequestAttempt::Failed(ThumbnailUnavailableReason::SeekFailed(error.to_string()));
    }
    state.last_seek_started = Some(Instant::now());
    match wait_for_event(
        client,
        shared,
        EVENT_PLAYBACK_RESTART,
        Instant::now() + config.seek_timeout,
        Cancellation::Request(versioned.revision),
    ) {
        WaitOutcome::Reached => {}
        WaitOutcome::Cancelled => return RequestAttempt::Cancelled,
        WaitOutcome::TimedOut => {
            return RequestAttempt::Failed(ThumbnailUnavailableReason::SeekFailed(
                "timed out waiting for the seek".to_owned(),
            ));
        }
        WaitOutcome::Failed(message) => {
            return RequestAttempt::Failed(ThumbnailUnavailableReason::SeekFailed(message));
        }
    }
    if !is_request_current(shared, versioned.revision) {
        return RequestAttempt::Cancelled;
    }
    match capture_frame(client, versioned.request, seconds, rotation, quality) {
        Ok(frame) if is_request_current(shared, versioned.revision) => {
            RequestAttempt::Captured(frame)
        }
        Ok(_) => RequestAttempt::Cancelled,
        Err(reason) => RequestAttempt::Failed(reason),
    }
}

fn request_retry_permitted(
    reason: &ThumbnailUnavailableReason,
    attempt: u8,
    max_attempts: u8,
) -> bool {
    attempt < max_attempts
        && matches!(
            reason,
            ThumbnailUnavailableReason::SeekFailed(_)
                | ThumbnailUnavailableReason::ScreenshotFailed(_)
        )
}

fn seek(client: &MpvClient, seconds: f64, mode: &str) -> Result<(), MpvError> {
    let seconds = format!("{seconds:.6}");
    client.command(&["seek", &seconds, mode])
}

fn throttle_seek(
    shared: &SharedMailbox,
    request_revision: u64,
    state: &WorkerState,
    interval: Duration,
) -> bool {
    let Some(last_started) = state.last_seek_started else {
        return is_request_current(shared, request_revision);
    };
    let Some(remaining) = interval.checked_sub(last_started.elapsed()) else {
        return is_request_current(shared, request_revision);
    };
    wait_interruptible(shared, remaining, request_revision)
}

fn wait_until_exact_due(
    shared: &SharedMailbox,
    request: VersionedRequest,
    exact_seek_delay: Duration,
) -> bool {
    let due = request.received_at + exact_seek_delay;
    let remaining = due.saturating_duration_since(Instant::now());
    wait_interruptible(shared, remaining, request.revision)
}

fn wait_interruptible(shared: &SharedMailbox, duration: Duration, request_revision: u64) -> bool {
    let deadline = Instant::now() + duration;
    let mut mailbox = lock_mailbox(shared);
    while mailbox.request_revision == request_revision && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let (next, _) = shared
            .condvar
            .wait_timeout(mailbox, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        mailbox = next;
    }
    mailbox.request_revision == request_revision
}

fn wait_source_interruptible(
    shared: &SharedMailbox,
    duration: Duration,
    source_revision: u64,
) -> bool {
    let deadline = Instant::now() + duration;
    let mut mailbox = lock_mailbox(shared);
    while mailbox.source_revision == source_revision && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let (next, _) = shared
            .condvar
            .wait_timeout(mailbox, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        mailbox = next;
    }
    mailbox.source_revision == source_revision
}

fn capture_frame(
    client: &MpvClient,
    request: ThumbnailRequest,
    requested_seconds: f64,
    rotation: u16,
    quality: ThumbnailQuality,
) -> Result<ThumbnailFrame, ThumbnailUnavailableReason> {
    let result = client
        .command_result(&["screenshot-raw", "video", "rgba"])
        .map_err(|error| ThumbnailUnavailableReason::ScreenshotFailed(error.to_string()))?;
    let raw =
        parse_screenshot(result.as_node()).map_err(ThumbnailUnavailableReason::InvalidFrame)?;
    let (width, height, rgba) = rotate_rgba(raw.width, raw.height, raw.rgba, rotation)
        .map_err(ThumbnailUnavailableReason::InvalidFrame)?;
    let decoded_seconds = client
        .get_double("time-pos")
        .ok()
        .filter(|value| value.is_finite())
        .unwrap_or(requested_seconds);
    Ok(ThumbnailFrame {
        generation: request.generation,
        request_id: request.request_id,
        requested_seconds,
        decoded_seconds,
        width,
        height,
        quality,
        rgba: Arc::from(rgba),
    })
}

fn emit_request_failure(
    shared: &SharedMailbox,
    sink: &Arc<dyn Fn(ThumbnailEvent) + Send + Sync>,
    request: VersionedRequest,
    reason: ThumbnailUnavailableReason,
) {
    if is_request_current(shared, request.revision) {
        sink(ThumbnailEvent::RequestFailed {
            generation: request.request.generation,
            request_id: request.request.request_id,
            reason,
        });
    }
}

#[derive(Clone, Copy)]
enum Cancellation {
    Source(u64),
    Request(u64),
}

enum WaitOutcome {
    Reached,
    Cancelled,
    TimedOut,
    Failed(String),
}

fn wait_for_event(
    client: &MpvClient,
    shared: &SharedMailbox,
    target: i32,
    deadline: Instant,
    cancellation: Cancellation,
) -> WaitOutcome {
    let mut wakeup_revision = shared.mpv_wakeup_revision.load(Ordering::Acquire);
    loop {
        loop {
            let event = client.wait_event(0.0);
            if event.is_null() {
                return WaitOutcome::Failed("libmpv returned a null event".to_owned());
            }
            // SAFETY: MPV keeps this event valid until the next `wait_event` call.
            let event = unsafe { &*event };
            if event.event_id == EVENT_NONE {
                break;
            }
            if event.error < 0 {
                return WaitOutcome::Failed(client.api.operation_error(event.error).to_string());
            }
            if event.event_id == target {
                return WaitOutcome::Reached;
            }
            if event.event_id == EVENT_SHUTDOWN {
                return WaitOutcome::Failed("libmpv shut down".to_owned());
            }
            if event.event_id == EVENT_END_FILE && !event.data.is_null() {
                // SAFETY: MPV_END_FILE events carry `mpv_event_end_file` data
                // for the lifetime of the current event.
                let end = unsafe { &*event.data.cast::<MpvEventEndFile>() };
                if end.error < 0 {
                    return WaitOutcome::Failed(client.api.operation_error(end.error).to_string());
                }
            }
        }

        if cancellation_invalid(shared, cancellation) {
            return WaitOutcome::Cancelled;
        }
        if Instant::now() >= deadline {
            return WaitOutcome::TimedOut;
        }
        let current_wakeup = shared.mpv_wakeup_revision.load(Ordering::Acquire);
        if current_wakeup != wakeup_revision {
            wakeup_revision = current_wakeup;
            continue;
        }

        let mailbox = lock_mailbox(shared);
        if cancellation_invalid_locked(&mailbox, cancellation) {
            return WaitOutcome::Cancelled;
        }
        if shared.mpv_wakeup_revision.load(Ordering::Acquire) != wakeup_revision {
            continue;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let (_mailbox, _) = shared
            .condvar
            .wait_timeout(mailbox, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        wakeup_revision = shared.mpv_wakeup_revision.load(Ordering::Acquire);
    }
}

fn cancellation_invalid(shared: &SharedMailbox, cancellation: Cancellation) -> bool {
    cancellation_invalid_locked(&lock_mailbox(shared), cancellation)
}

fn cancellation_invalid_locked(mailbox: &Mailbox, cancellation: Cancellation) -> bool {
    match cancellation {
        Cancellation::Source(revision) => mailbox.source_revision != revision,
        Cancellation::Request(revision) => mailbox.request_revision != revision,
    }
}

fn is_source_current(shared: &SharedMailbox, revision: u64) -> bool {
    lock_mailbox(shared).source_revision == revision
}

fn is_request_current(shared: &SharedMailbox, revision: u64) -> bool {
    lock_mailbox(shared).request_revision == revision
}

fn wait_for_activity(shared: &SharedMailbox) {
    let wakeup_revision = shared.mpv_wakeup_revision.load(Ordering::Acquire);
    let mut mailbox = lock_mailbox(shared);
    while mailbox.controls.is_empty()
        && mailbox.latest_request.is_none()
        && shared.mpv_wakeup_revision.load(Ordering::Acquire) == wakeup_revision
    {
        mailbox = shared
            .condvar
            .wait(mailbox)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

fn drain_unhandled_events(client: &MpvClient) {
    loop {
        let event = client.wait_event(0.0);
        if event.is_null() {
            return;
        }
        // SAFETY: MPV keeps the returned event valid until the next wait call.
        if unsafe { (*event).event_id } == EVENT_NONE {
            return;
        }
    }
}

fn finite_non_negative(value: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

pub(crate) fn normalize_rotation(rotation: i64) -> u16 {
    let normalized = rotation.rem_euclid(360) as u16;
    match normalized {
        45..=134 => 90,
        135..=224 => 180,
        225..=314 => 270,
        _ => 0,
    }
}

#[derive(Debug)]
pub(crate) struct RawRgba {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
}

pub(crate) fn parse_screenshot(node: &MpvNode) -> Result<RawRgba, String> {
    if node.format != FORMAT_NODE_MAP {
        return Err(format!("expected node map, got format {}", node.format));
    }
    // SAFETY: The union member is selected only after checking the node format.
    let list = unsafe { node.value.list };
    // SAFETY: A node-map result carries a valid list allocation for the
    // lifetime of its enclosing command-result node.
    let list =
        unsafe { list.as_ref() }.ok_or_else(|| "screenshot map has no entries".to_owned())?;
    let entries = map_entries(list)?;
    let width = map_i64(&entries, "w")?;
    let height = map_i64(&entries, "h")?;
    let stride = map_i64(&entries, "stride")?;
    let format = map_string(&entries, "format")?;
    if format != "rgba" {
        return Err(format!("unsupported screenshot format {format:?}"));
    }
    let width = u32::try_from(width).map_err(|_| "invalid screenshot width".to_owned())?;
    let height = u32::try_from(height).map_err(|_| "invalid screenshot height".to_owned())?;
    if width == 0 || height == 0 {
        return Err("screenshot dimensions must be non-zero".to_owned());
    }
    let row_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or_else(|| "screenshot row size overflow".to_owned())?;
    let absolute_stride = stride
        .checked_abs()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| "invalid screenshot stride".to_owned())?;
    if absolute_stride < row_bytes {
        return Err("screenshot stride is smaller than one RGBA row".to_owned());
    }
    let height_usize = usize::try_from(height).map_err(|_| "invalid height".to_owned())?;
    let required = absolute_stride
        .checked_mul(height_usize)
        .ok_or_else(|| "screenshot buffer size overflow".to_owned())?;
    let data_node = map_node(&entries, "data")?;
    if data_node.format != FORMAT_BYTE_ARRAY {
        return Err("screenshot data is not a byte array".to_owned());
    }
    // SAFETY: The union member is selected only after checking its format.
    let byte_array = unsafe { data_node.value.byte_array };
    // SAFETY: A byte-array node carries a valid descriptor allocation for the
    // lifetime of its enclosing command-result node.
    let byte_array =
        unsafe { byte_array.as_ref() }.ok_or_else(|| "screenshot byte array is null".to_owned())?;
    if byte_array.size < required {
        return Err(format!(
            "screenshot buffer is undersized: {} < {required}",
            byte_array.size
        ));
    }
    if byte_array.data.is_null() {
        return Err("screenshot pixel pointer is null".to_owned());
    }
    // SAFETY: MPV reports a byte-array allocation at least `required` bytes
    // long, and it remains owned by the surrounding `MpvOwnedNode` while this
    // function copies it.
    let source = unsafe { slice::from_raw_parts(byte_array.data.cast::<u8>(), required) };
    let output_len = row_bytes
        .checked_mul(height_usize)
        .ok_or_else(|| "packed screenshot size overflow".to_owned())?;
    let mut rgba = vec![0_u8; output_len];
    for row in 0..height_usize {
        let source_row = if stride < 0 {
            height_usize - 1 - row
        } else {
            row
        };
        let source_offset = source_row * absolute_stride;
        let destination_offset = row * row_bytes;
        rgba[destination_offset..destination_offset + row_bytes]
            .copy_from_slice(&source[source_offset..source_offset + row_bytes]);
    }
    Ok(RawRgba {
        width,
        height,
        rgba,
    })
}

fn map_entries(list: &MpvNodeList) -> Result<Vec<(&CStr, &MpvNode)>, String> {
    let count = usize::try_from(list.num).map_err(|_| "negative map entry count".to_owned())?;
    if count > 64 {
        return Err("screenshot map contains too many entries".to_owned());
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    if list.values.is_null() || list.keys.is_null() {
        return Err("screenshot map has missing key/value storage".to_owned());
    }
    // SAFETY: MPV_NODE_MAP guarantees `count` value and key entries when the
    // count is positive. The enclosing owned node keeps them alive.
    let values = unsafe { slice::from_raw_parts(list.values, count) };
    // SAFETY: Same allocation/lifetime guarantee as `values` above.
    let keys = unsafe { slice::from_raw_parts(list.keys, count) };
    let mut entries = Vec::with_capacity(count);
    for (key, value) in keys.iter().zip(values) {
        if key.is_null() {
            return Err("screenshot map contains a null key".to_owned());
        }
        // SAFETY: MPV map keys are non-null, null-terminated strings owned by
        // the enclosing node.
        entries.push((unsafe { CStr::from_ptr(*key) }, value));
    }
    Ok(entries)
}

fn map_node<'a>(entries: &[(&CStr, &'a MpvNode)], name: &str) -> Result<&'a MpvNode, String> {
    entries
        .iter()
        .find_map(|(key, value)| (key.to_bytes() == name.as_bytes()).then_some(*value))
        .ok_or_else(|| format!("screenshot map is missing {name:?}"))
}

fn map_i64(entries: &[(&CStr, &MpvNode)], name: &str) -> Result<i64, String> {
    let node = map_node(entries, name)?;
    if node.format != FORMAT_INT64 {
        return Err(format!("screenshot field {name:?} is not an integer"));
    }
    // SAFETY: The union member is selected only after checking its format.
    Ok(unsafe { node.value.int64 })
}

fn map_string(entries: &[(&CStr, &MpvNode)], name: &str) -> Result<String, String> {
    let node = map_node(entries, name)?;
    if node.format != FORMAT_STRING {
        return Err(format!("screenshot field {name:?} is not a string"));
    }
    // SAFETY: The union member is selected only after checking its format.
    let value = unsafe { node.value.string };
    if value.is_null() {
        return Err(format!("screenshot field {name:?} is null"));
    }
    // SAFETY: MPV string nodes are null-terminated and remain owned by the
    // surrounding command-result node.
    Ok(unsafe { CStr::from_ptr(value) }
        .to_string_lossy()
        .into_owned())
}

pub(crate) fn rotate_rgba(
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    rotation: u16,
) -> Result<(u32, u32, Vec<u8>), String> {
    let expected = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "rotated frame size overflow".to_owned())?;
    if rgba.len() != expected {
        return Err("RGBA buffer does not match its dimensions".to_owned());
    }
    if rotation == 0 {
        return Ok((width, height, rgba));
    }
    let (output_width, output_height) = if matches!(rotation, 90 | 270) {
        (height, width)
    } else {
        (width, height)
    };
    let mut output = vec![0_u8; expected];
    let width_usize = width as usize;
    let height_usize = height as usize;
    let output_width_usize = output_width as usize;
    for y in 0..height_usize {
        for x in 0..width_usize {
            let (destination_x, destination_y) = match rotation {
                90 => (height_usize - 1 - y, x),
                180 => (width_usize - 1 - x, height_usize - 1 - y),
                270 => (y, width_usize - 1 - x),
                _ => (x, y),
            };
            let source_offset = (y * width_usize + x) * 4;
            let destination_offset = (destination_y * output_width_usize + destination_x) * 4;
            output[destination_offset..destination_offset + 4]
                .copy_from_slice(&rgba[source_offset..source_offset + 4]);
        }
    }
    Ok((output_width, output_height, output))
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct CacheKey {
    generation: u64,
    bucket: i64,
}

impl CacheKey {
    fn new(generation: u64, seconds: f64) -> Self {
        Self {
            generation,
            bucket: (seconds / CACHE_BUCKET_SECONDS).round() as i64,
        }
    }
}

#[derive(Clone)]
struct CachedFrame {
    decoded_seconds: f64,
    width: u32,
    height: u32,
    rgba: Arc<[u8]>,
}

impl CachedFrame {
    fn from_frame(frame: &ThumbnailFrame) -> Self {
        Self {
            decoded_seconds: frame.decoded_seconds,
            width: frame.width,
            height: frame.height,
            rgba: frame.rgba.clone(),
        }
    }

    fn into_frame(self, request: ThumbnailRequest, quality: ThumbnailQuality) -> ThumbnailFrame {
        ThumbnailFrame {
            generation: request.generation,
            request_id: request.request_id,
            requested_seconds: request.seconds,
            decoded_seconds: self.decoded_seconds,
            width: self.width,
            height: self.height,
            quality,
            rgba: self.rgba,
        }
    }
}

struct CacheEntry {
    key: CacheKey,
    frame: CachedFrame,
}

struct FrameCache {
    capacity: usize,
    used: usize,
    entries: VecDeque<CacheEntry>,
}

impl FrameCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            used: 0,
            entries: VecDeque::new(),
        }
    }

    fn get(&mut self, key: CacheKey) -> Option<CachedFrame> {
        let index = self.entries.iter().position(|entry| entry.key == key)?;
        let entry = self.entries.remove(index)?;
        let result = entry.frame.clone();
        self.entries.push_back(entry);
        Some(result)
    }

    fn insert(&mut self, key: CacheKey, frame: CachedFrame) {
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key)
            && let Some(previous) = self.entries.remove(index)
        {
            self.used = self.used.saturating_sub(previous.frame.rgba.len());
        }
        let size = frame.rgba.len();
        if size > self.capacity {
            return;
        }
        while self.used.saturating_add(size) > self.capacity {
            let Some(evicted) = self.entries.pop_front() else {
                break;
            };
            self.used = self.used.saturating_sub(evicted.frame.rgba.len());
        }
        self.used = self.used.saturating_add(size);
        self.entries.push_back(CacheEntry { key, frame });
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.used = 0;
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;

    use super::*;
    use crate::ffi::{MpvByteArray, MpvNodeValue};

    struct ScreenshotFixture {
        _keys: Vec<CString>,
        _format: CString,
        pixels: Vec<u8>,
        byte_array: Box<MpvByteArray>,
        values: Vec<MpvNode>,
        key_pointers: Vec<*mut i8>,
        list: Box<MpvNodeList>,
        root: MpvNode,
    }

    impl ScreenshotFixture {
        fn new(width: i64, height: i64, stride: i64, pixels: Vec<u8>) -> Self {
            let keys = ["w", "h", "stride", "format", "data"]
                .into_iter()
                .map(|key| CString::new(key).expect("static key"))
                .collect::<Vec<_>>();
            let format = CString::new("rgba").expect("static format");
            let mut pixels = pixels;
            let mut byte_array = Box::new(MpvByteArray {
                data: pixels.as_mut_ptr().cast(),
                size: pixels.len(),
            });
            let mut values = vec![
                MpvNode {
                    value: MpvNodeValue { int64: width },
                    format: FORMAT_INT64,
                },
                MpvNode {
                    value: MpvNodeValue { int64: height },
                    format: FORMAT_INT64,
                },
                MpvNode {
                    value: MpvNodeValue { int64: stride },
                    format: FORMAT_INT64,
                },
                MpvNode {
                    value: MpvNodeValue {
                        string: format.as_ptr().cast_mut(),
                    },
                    format: FORMAT_STRING,
                },
                MpvNode {
                    value: MpvNodeValue {
                        byte_array: byte_array.as_mut(),
                    },
                    format: FORMAT_BYTE_ARRAY,
                },
            ];
            let mut key_pointers = keys
                .iter()
                .map(|key| key.as_ptr().cast_mut())
                .collect::<Vec<_>>();
            let mut list = Box::new(MpvNodeList {
                num: values.len() as i32,
                values: values.as_mut_ptr(),
                keys: key_pointers.as_mut_ptr(),
            });
            let root = MpvNode {
                value: MpvNodeValue {
                    list: list.as_mut(),
                },
                format: FORMAT_NODE_MAP,
            };
            Self {
                _keys: keys,
                _format: format,
                pixels,
                byte_array,
                values,
                key_pointers,
                list,
                root,
            }
        }

        fn keep_alive(&self) {
            std::hint::black_box((
                &self.pixels,
                &self.byte_array,
                &self.values,
                &self.key_pointers,
                &self.list,
            ));
        }
    }

    fn pixel(value: u8) -> [u8; 4] {
        [value, 0, 0, 255]
    }

    #[test]
    fn screenshot_parser_accepts_packed_rows() {
        let fixture =
            ScreenshotFixture::new(2, 2, 8, [pixel(1), pixel(2), pixel(3), pixel(4)].concat());
        let parsed = parse_screenshot(&fixture.root).expect("valid screenshot");
        fixture.keep_alive();
        assert_eq!(
            parsed.rgba,
            [pixel(1), pixel(2), pixel(3), pixel(4)].concat()
        );
    }

    #[test]
    fn screenshot_parser_packs_padded_rows() {
        let fixture = ScreenshotFixture::new(
            2,
            2,
            12,
            [pixel(1), pixel(2), [9; 4], pixel(3), pixel(4), [9; 4]].concat(),
        );
        let parsed = parse_screenshot(&fixture.root).expect("valid screenshot");
        fixture.keep_alive();
        assert_eq!(
            parsed.rgba,
            [pixel(1), pixel(2), pixel(3), pixel(4)].concat()
        );
    }

    #[test]
    fn screenshot_parser_normalizes_negative_stride() {
        let fixture =
            ScreenshotFixture::new(2, 2, -8, [pixel(3), pixel(4), pixel(1), pixel(2)].concat());
        let parsed = parse_screenshot(&fixture.root).expect("valid screenshot");
        fixture.keep_alive();
        assert_eq!(
            parsed.rgba,
            [pixel(1), pixel(2), pixel(3), pixel(4)].concat()
        );
    }

    #[test]
    fn screenshot_parser_rejects_undersized_buffers() {
        let fixture = ScreenshotFixture::new(2, 2, 8, vec![0; 15]);
        let error = parse_screenshot(&fixture.root).expect_err("buffer must be rejected");
        fixture.keep_alive();
        assert!(error.contains("undersized"), "unexpected error: {error}");
    }

    #[test]
    fn screenshot_parser_rejects_missing_fields() {
        let mut fixture = ScreenshotFixture::new(1, 1, 4, pixel(1).to_vec());
        fixture.list.num = 4;
        let error = parse_screenshot(&fixture.root).expect_err("missing data must be rejected");
        fixture.keep_alive();
        assert!(
            error.contains("missing \"data\""),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn screenshot_parser_rejects_wrong_field_types() {
        let mut fixture = ScreenshotFixture::new(1, 1, 4, pixel(1).to_vec());
        fixture.values[0].format = FORMAT_STRING;
        let error = parse_screenshot(&fixture.root).expect_err("wrong type must be rejected");
        fixture.keep_alive();
        assert!(
            error.contains("not an integer"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn screenshot_parser_rejects_invalid_dimensions() {
        let fixture = ScreenshotFixture::new(-1, 1, 4, pixel(1).to_vec());
        let error = parse_screenshot(&fixture.root).expect_err("negative width must be rejected");
        fixture.keep_alive();
        assert!(error.contains("width"), "unexpected error: {error}");
    }

    #[test]
    fn screenshot_parser_rejects_dimension_overflow() {
        let fixture = ScreenshotFixture::new(i64::MAX, 1, 4, pixel(1).to_vec());
        let error = parse_screenshot(&fixture.root).expect_err("huge width must be rejected");
        fixture.keep_alive();
        assert!(error.contains("width"), "unexpected error: {error}");
    }

    #[test]
    fn rotate_rgba_rotates_clockwise_by_ninety_degrees() {
        let rgba = [pixel(1), pixel(2), pixel(3), pixel(4), pixel(5), pixel(6)].concat();
        let (width, height, rotated) = rotate_rgba(3, 2, rgba, 90).expect("valid frame");
        assert_eq!((width, height), (2, 3));
        assert_eq!(
            rotated,
            [pixel(4), pixel(1), pixel(5), pixel(2), pixel(6), pixel(3)].concat()
        );
    }

    #[test]
    fn rotate_rgba_rotates_by_one_hundred_eighty_degrees() {
        let rgba = [pixel(1), pixel(2), pixel(3), pixel(4)].concat();
        let (_, _, rotated) = rotate_rgba(2, 2, rgba, 180).expect("valid frame");
        assert_eq!(rotated, [pixel(4), pixel(3), pixel(2), pixel(1)].concat());
    }

    #[test]
    fn rotate_rgba_rotates_clockwise_by_two_hundred_seventy_degrees() {
        let rgba = [pixel(1), pixel(2), pixel(3), pixel(4), pixel(5), pixel(6)].concat();
        let (width, height, rotated) = rotate_rgba(3, 2, rgba, 270).expect("valid frame");
        assert_eq!((width, height), (2, 3));
        assert_eq!(
            rotated,
            [pixel(3), pixel(6), pixel(2), pixel(5), pixel(1), pixel(4)].concat()
        );
    }

    #[test]
    fn newest_request_replaces_pending_request() {
        let shared = Arc::new(SharedMailbox::default());
        let controller = ThumbnailController {
            shared: shared.clone(),
        };
        controller
            .request(ThumbnailRequest {
                generation: 1,
                request_id: 1,
                seconds: 1.0,
            })
            .expect("first request");
        controller
            .request(ThumbnailRequest {
                generation: 1,
                request_id: 2,
                seconds: 2.0,
            })
            .expect("second request");
        let request = take_latest_request(&shared).expect("latest request");
        assert_eq!(request.request.request_id, 2);
    }

    #[test]
    fn clear_invalidates_an_in_flight_request_revision() {
        let shared = Arc::new(SharedMailbox::default());
        let controller = ThumbnailController {
            shared: shared.clone(),
        };
        controller
            .request(ThumbnailRequest {
                generation: 1,
                request_id: 1,
                seconds: 1.0,
            })
            .expect("request");
        let revision = take_latest_request(&shared).expect("request").revision;
        controller.clear().expect("clear");
        assert!(!is_request_current(&shared, revision));
    }

    #[test]
    fn hardware_mode_uses_copy_back_and_disabled_mode_stays_software_only() {
        assert_eq!(thumbnail_hwdec(true), "auto-copy");
        assert_eq!(thumbnail_hwdec(false), "no");
    }

    #[test]
    fn zero_retry_attempts_are_rejected() {
        let config = ThumbnailConfig {
            max_request_attempts: 0,
            ..ThumbnailConfig::default()
        };

        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn transient_source_failure_is_retried_once_with_default_policy() {
        let config = ThumbnailConfig::default();
        let reason = ThumbnailUnavailableReason::LoadFailed("temporary".to_owned());

        assert!(source_retry_permitted(
            &reason,
            1,
            config.max_source_attempts
        ));
        assert!(!source_retry_permitted(
            &reason,
            2,
            config.max_source_attempts
        ));
    }

    #[test]
    fn permanent_source_failures_are_not_retried() {
        let config = ThumbnailConfig::default();

        assert!(!source_retry_permitted(
            &ThumbnailUnavailableReason::NoVideo,
            1,
            config.max_source_attempts
        ));
        assert!(!source_retry_permitted(
            &ThumbnailUnavailableReason::NotSeekable,
            1,
            config.max_source_attempts
        ));
    }

    #[test]
    fn transient_request_failures_are_retried_once_with_default_policy() {
        let config = ThumbnailConfig::default();
        let seek = ThumbnailUnavailableReason::SeekFailed("temporary".to_owned());
        let screenshot = ThumbnailUnavailableReason::ScreenshotFailed("temporary".to_owned());

        assert!(request_retry_permitted(
            &seek,
            1,
            config.max_request_attempts
        ));
        assert!(request_retry_permitted(
            &screenshot,
            1,
            config.max_request_attempts
        ));
        assert!(!request_retry_permitted(
            &seek,
            2,
            config.max_request_attempts
        ));
    }

    #[test]
    fn invalid_frames_are_not_retried() {
        let config = ThumbnailConfig::default();

        assert!(!request_retry_permitted(
            &ThumbnailUnavailableReason::InvalidFrame("bad frame".to_owned()),
            1,
            config.max_request_attempts
        ));
    }

    #[test]
    fn a_new_source_generation_cancels_source_retry_delay() {
        let shared = Arc::new(SharedMailbox::default());
        let controller = ThumbnailController {
            shared: shared.clone(),
        };
        controller
            .load_source(ThumbnailSource {
                generation: 1,
                url: "file:///first.mp4".to_owned(),
                initial_position: 0.0,
            })
            .expect("first source");
        let revision = lock_mailbox(&shared).source_revision;
        controller
            .load_source(ThumbnailSource {
                generation: 2,
                url: "file:///second.mp4".to_owned(),
                initial_position: 0.0,
            })
            .expect("replacement source");

        assert!(!wait_source_interruptible(
            &shared,
            Duration::from_secs(1),
            revision
        ));
    }

    #[test]
    fn long_video_uses_fast_then_exact_scheduling() {
        assert!(uses_fast_stage(30.0));
    }

    #[test]
    fn short_video_uses_exact_only_scheduling() {
        assert!(!uses_fast_stage(29.999));
    }

    #[test]
    fn frame_cache_stays_within_capacity() {
        let mut cache = FrameCache::new(8);
        let frame = |value| CachedFrame {
            decoded_seconds: 0.0,
            width: 1,
            height: 1,
            rgba: Arc::from([value; 4]),
        };
        cache.insert(CacheKey::new(1, 0.0), frame(1));
        cache.insert(CacheKey::new(1, 1.0), frame(2));
        cache.insert(CacheKey::new(1, 2.0), frame(3));
        assert_eq!(cache.used, 8);
        assert!(cache.get(CacheKey::new(1, 0.0)).is_none());
    }

    #[test]
    fn clearing_cache_invalidates_previous_generation_frames() {
        let mut cache = FrameCache::new(8);
        cache.insert(
            CacheKey::new(1, 1.0),
            CachedFrame {
                decoded_seconds: 1.0,
                width: 1,
                height: 1,
                rgba: Arc::from([1; 4]),
            },
        );
        cache.clear();
        assert!(cache.get(CacheKey::new(1, 1.0)).is_none());
    }
}
