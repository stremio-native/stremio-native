//! Secondary libmpv worker that decodes muted hover trailer previews.
//!
//! The timeline thumbnail worker seeks a paused source one frame at a time;
//! this one keeps a source *playing* and pulls the currently decoded frame on a
//! fixed cadence, so a catalog card can show a live trailer without the render
//! context, window surface, or session bookkeeping the main player owns.

use std::{
    ffi::c_void,
    fmt,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    ffi::{
        EVENT_END_FILE, EVENT_FILE_LOADED, EVENT_NONE, EVENT_PLAYBACK_RESTART, EVENT_SHUTDOWN,
        MpvApi, MpvClient, MpvError, MpvEventEndFile,
    },
    thumbnail::{normalize_rotation, parse_screenshot, rotate_rgba},
};

/// Resource and scheduling limits for the hover preview decoder.
#[derive(Clone, Debug)]
pub struct PreviewConfig {
    pub max_width: u32,
    pub max_height: u32,
    /// Wall-clock spacing between delivered frames.
    pub frame_interval: Duration,
    pub load_timeout: Duration,
    pub hardware_decoding: bool,
    /// Restarts the trailer at EOF so the popup keeps moving while hovered.
    pub loop_playback: bool,
    /// Consecutive capture failures tolerated before the source is abandoned.
    pub max_capture_failures: u8,
    /// Explicit `yt-dlp` location for MPV's `ytdl_hook`. [`None`] leaves the
    /// hook to find the binary on `PATH` itself.
    pub ytdl_path: Option<std::path::PathBuf>,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            // The popup's video area is roughly 420x235 logical pixels, and
            // every frame is copied twice on its way to Slint, so decoding
            // larger than the card can show is pure bandwidth.
            max_width: 480,
            max_height: 270,
            frame_interval: Duration::from_millis(66),
            load_timeout: Duration::from_secs(20),
            hardware_decoding: false,
            loop_playback: true,
            max_capture_failures: 5,
            ytdl_path: None,
        }
    }
}

/// Trailers arrive as YouTube links, so the popup needs `ytdl_hook` too. Frames
/// are delivered no larger than [`PreviewConfig::max_width`] by
/// [`PreviewConfig::max_height`], so anything above 480p would be decoded only
/// to be scaled away — and a smaller rendition starts noticeably sooner, which
/// matters far more for a popup that opens on hover.
const PREVIEW_YTDL_FORMAT: &str =
    "bestvideo[height<=480]+bestaudio/best[height<=480]/bestvideo[height<=720]+bestaudio/best";

/// A trailer assigned to the preview worker.
#[derive(Clone, Debug)]
pub struct PreviewSource {
    pub generation: u64,
    pub url: String,
    pub start_seconds: f64,
    pub muted: bool,
}

/// A tightly packed, top-down RGBA preview frame.
#[derive(Clone)]
pub struct PreviewFrame {
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub position: f64,
    pub duration: f64,
    pub rgba: Arc<[u8]>,
}

impl fmt::Debug for PreviewFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreviewFrame")
            .field("generation", &self.generation)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("position", &self.position)
            .field("duration", &self.duration)
            .field("rgba_bytes", &self.rgba.len())
            .finish()
    }
}

/// Why a preview could not be produced for a source.
#[derive(Clone, Debug)]
pub enum PreviewUnavailableReason {
    NoVideo,
    LoadFailed(String),
    ScreenshotFailed(String),
    InvalidFrame(String),
}

impl PreviewUnavailableReason {
    /// A short, user-presentable summary.
    pub fn summary(&self) -> &str {
        match self {
            Self::NoVideo => "the trailer has no video track",
            Self::LoadFailed(message)
            | Self::ScreenshotFailed(message)
            | Self::InvalidFrame(message) => message,
        }
    }
}

/// Events emitted from the preview worker thread.
#[derive(Clone, Debug)]
pub enum PreviewEvent {
    WorkerReady,
    Buffering {
        generation: u64,
    },
    Playing {
        generation: u64,
        duration: f64,
    },
    Frame(PreviewFrame),
    Unavailable {
        generation: u64,
        reason: PreviewUnavailableReason,
    },
    Finished {
        generation: u64,
    },
    Shutdown,
}

#[derive(Clone)]
pub struct PreviewController {
    shared: Arc<SharedMailbox>,
}

impl PreviewController {
    /// Replaces whatever the worker is playing and invalidates older work.
    pub fn play(&self, source: PreviewSource) -> Result<(), MpvError> {
        let mut mailbox = lock_mailbox(&self.shared);
        ensure_open(&mailbox)?;
        mailbox.source_revision = mailbox.source_revision.wrapping_add(1);
        mailbox.muted = source.muted;
        let revision = mailbox.source_revision;
        mailbox.controls.push_back(Control::Play(revision, source));
        drop(mailbox);
        self.shared.condvar.notify_one();
        Ok(())
    }

    /// Applies a mute change to the session that is playing now.
    pub fn set_muted(&self, muted: bool) -> Result<(), MpvError> {
        let mut mailbox = lock_mailbox(&self.shared);
        ensure_open(&mailbox)?;
        mailbox.muted = muted;
        mailbox.controls.push_back(Control::Mute(muted));
        drop(mailbox);
        self.shared.condvar.notify_one();
        Ok(())
    }

    /// Tears the current source down without stopping the worker.
    pub fn stop(&self) -> Result<(), MpvError> {
        let mut mailbox = lock_mailbox(&self.shared);
        ensure_open(&mailbox)?;
        mailbox.source_revision = mailbox.source_revision.wrapping_add(1);
        let revision = mailbox.source_revision;
        mailbox.controls.push_back(Control::Stop(revision));
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
        mailbox.controls.push_back(Control::Shutdown);
        drop(mailbox);
        self.shared.condvar.notify_all();
    }
}

/// Owns the persistent preview thread and joins it on shutdown or drop.
pub struct PreviewRuntime {
    controller: PreviewController,
    worker: Option<JoinHandle<()>>,
}

impl PreviewRuntime {
    /// Initializes a separate MPV client and starts its worker.
    ///
    /// Like the thumbnail worker this always uses the pinned runtime: a
    /// swapped-in libmpv with a screenshot defect would otherwise crash the
    /// whole application while a viewer merely hovers a catalog card.
    pub fn start(
        config: PreviewConfig,
        event_sink: impl Fn(PreviewEvent) + Send + Sync + 'static,
    ) -> Result<Self, MpvError> {
        let started_at = Instant::now();
        validate_config(&config)?;
        let api = MpvApi::pinned_runtime()?;
        let client = MpvClient::create(api)?;
        configure_client(&client, &config)?;
        client.initialize()?;

        let shared = Arc::new(SharedMailbox::default());
        client.set_wakeup_callback(
            Some(wakeup_preview),
            Arc::as_ptr(&shared).cast_mut().cast::<c_void>(),
        );
        let controller = PreviewController {
            shared: shared.clone(),
        };
        let sink: Arc<dyn Fn(PreviewEvent) + Send + Sync> = Arc::new(event_sink);
        let worker = thread::Builder::new()
            .name("mpv-preview".to_owned())
            .spawn(move || {
                worker_main(client, shared, config, sink, started_at.elapsed());
            })
            .map_err(|error| MpvError::InvalidNode(format!("could not start worker: {error}")))?;

        Ok(Self {
            controller,
            worker: Some(worker),
        })
    }

    /// Returns a clonable handle for source and mute control.
    pub fn controller(&self) -> PreviewController {
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

impl Drop for PreviewRuntime {
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
    controls: std::collections::VecDeque<Control>,
    source_revision: u64,
    muted: bool,
    closed: bool,
}

enum Control {
    Play(u64, PreviewSource),
    Mute(bool),
    Stop(u64),
    Shutdown,
}

unsafe extern "C" fn wakeup_preview(context: *mut c_void) {
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

fn validate_config(config: &PreviewConfig) -> Result<(), MpvError> {
    if config.max_width == 0 || config.max_height == 0 {
        return Err(MpvError::InvalidNode(
            "preview bounds must be non-zero".to_owned(),
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
        .ok_or_else(|| MpvError::InvalidNode("preview bounds overflow memory size".to_owned()))?;
    if config.frame_interval.is_zero() {
        return Err(MpvError::InvalidNode(
            "preview frame interval must be non-zero".to_owned(),
        ));
    }
    if config.max_capture_failures == 0 {
        return Err(MpvError::InvalidNode(
            "preview capture failure budget must be non-zero".to_owned(),
        ));
    }
    Ok(())
}

fn configure_client(client: &MpvClient, config: &PreviewConfig) -> Result<(), MpvError> {
    let options = [
        ("config", "no"),
        ("load-scripts", "no"),
        ("terminal", "no"),
        ("input-default-bindings", "no"),
        ("input-vo-keyboard", "no"),
        ("osc", "no"),
        ("idle", "yes"),
        ("keep-open", "no"),
        ("vo", "null"),
        // Audio stays initialized but silent: unmuting is a popup control, and
        // re-initializing the audio chain mid-hover would stall playback.
        ("mute", "yes"),
        ("volume", "60"),
        ("sid", "no"),
        ("sub-auto", "no"),
        ("ytdl", "yes"),
        ("ytdl-format", PREVIEW_YTDL_FORMAT),
        ("hwdec", preview_hwdec(config.hardware_decoding)),
        ("screenshot-sw", "yes"),
        ("vd-lavc-threads", "2"),
        ("vd-lavc-fast", "yes"),
        ("cache", "yes"),
        ("cache-secs", "10"),
        ("demuxer-max-bytes", "16777216"),
        ("demuxer-max-back-bytes", "8388608"),
        ("sws-scaler", "fast-bilinear"),
        ("loop-file", if config.loop_playback { "inf" } else { "no" }),
    ];
    for (name, value) in options {
        client.set_option(name, value)?;
    }
    // Not fatal: without it the hook falls back to searching `PATH`.
    if let Some(ytdl_path) = config.ytdl_path.as_deref()
        && let Err(error) =
            client.set_option("script-opts", &crate::actor::ytdl_script_opt(ytdl_path))
    {
        tracing::warn!(%error, path = %ytdl_path.display(), "could not point ytdl_hook at yt-dlp");
    }
    Ok(())
}

const fn preview_hwdec(hardware_decoding: bool) -> &'static str {
    if hardware_decoding { "auto-copy" } else { "no" }
}

struct PlayingSource {
    generation: u64,
    duration: f64,
    rotation: u16,
    next_frame_due: Instant,
    capture_failures: u8,
}

fn worker_main(
    client: Arc<MpvClient>,
    shared: Arc<SharedMailbox>,
    config: PreviewConfig,
    sink: Arc<dyn Fn(PreviewEvent) + Send + Sync>,
    initialization_time: Duration,
) {
    tracing::info!(
        worker = "mpv-preview",
        initialization_ms = initialization_time.as_millis(),
        "hover preview worker ready"
    );
    sink(PreviewEvent::WorkerReady);
    let mut source: Option<PlayingSource> = None;

    let mut running = true;
    while running {
        if let Some(control) = take_control(&shared) {
            running = handle_control(&client, &shared, &config, &sink, &mut source, control);
            continue;
        }

        let Some(playing) = source.as_mut() else {
            drain_unhandled_events(&client);
            wait_for_activity(&shared);
            continue;
        };

        let remaining = playing
            .next_frame_due
            .saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            drain_unhandled_events(&client);
            wait_for_deadline(&shared, playing.next_frame_due);
            continue;
        }
        playing.next_frame_due = Instant::now() + config.frame_interval;

        let revision = lock_mailbox(&shared).source_revision;
        // With `keep-open=no` a finished, non-looping trailer leaves the client
        // idle; screenshots would then repeat the last decoded frame forever.
        if client.get_flag("idle-active").unwrap_or(false) {
            let generation = playing.generation;
            source = None;
            if is_source_current(&shared, revision) {
                sink(PreviewEvent::Finished { generation });
            }
            continue;
        }
        match capture_frame(&client, playing) {
            Ok(frame) => {
                playing.capture_failures = 0;
                if is_source_current(&shared, revision) {
                    sink(PreviewEvent::Frame(frame));
                }
            }
            Err(reason) => {
                playing.capture_failures = playing.capture_failures.saturating_add(1);
                if playing.capture_failures < config.max_capture_failures {
                    continue;
                }
                let generation = playing.generation;
                source = None;
                let _ = client.command(&["stop"]);
                if is_source_current(&shared, revision) {
                    tracing::debug!(generation, reason = ?reason, "hover preview capture gave up");
                    sink(PreviewEvent::Unavailable { generation, reason });
                }
            }
        }
    }

    client.set_wakeup_callback(None, std::ptr::null_mut());
    let _ = client.command(&["stop"]);
    sink(PreviewEvent::Shutdown);
}

fn take_control(shared: &SharedMailbox) -> Option<Control> {
    lock_mailbox(shared).controls.pop_front()
}

fn handle_control(
    client: &MpvClient,
    shared: &SharedMailbox,
    config: &PreviewConfig,
    sink: &Arc<dyn Fn(PreviewEvent) + Send + Sync>,
    source: &mut Option<PlayingSource>,
    control: Control,
) -> bool {
    match control {
        Control::Play(revision, requested) => {
            *source = None;
            let _ = client.command(&["stop"]);
            if !is_source_current(shared, revision) {
                return true;
            }
            sink(PreviewEvent::Buffering {
                generation: requested.generation,
            });
            match load_source(client, shared, config, revision, &requested) {
                Ok(Some(loaded)) => {
                    if is_source_current(shared, revision) {
                        sink(PreviewEvent::Playing {
                            generation: loaded.generation,
                            duration: loaded.duration,
                        });
                        *source = Some(loaded);
                    } else {
                        let _ = client.command(&["stop"]);
                    }
                }
                Ok(None) => {}
                Err(reason) => {
                    let _ = client.command(&["stop"]);
                    if is_source_current(shared, revision) {
                        tracing::debug!(reason = ?reason, "hover preview source unavailable");
                        sink(PreviewEvent::Unavailable {
                            generation: requested.generation,
                            reason,
                        });
                    }
                }
            }
            true
        }
        Control::Mute(muted) => {
            if source.is_some()
                && let Err(error) = client.set_flag("mute", muted)
            {
                tracing::debug!(%error, "hover preview mute change was rejected");
            }
            true
        }
        Control::Stop(revision) => {
            *source = None;
            if is_source_current(shared, revision) {
                let _ = client.command(&["stop"]);
            }
            true
        }
        Control::Shutdown => false,
    }
}

fn load_source(
    client: &MpvClient,
    shared: &SharedMailbox,
    config: &PreviewConfig,
    revision: u64,
    source: &PreviewSource,
) -> Result<Option<PlayingSource>, PreviewUnavailableReason> {
    let start = if source.start_seconds.is_finite() {
        source.start_seconds.max(0.0)
    } else {
        0.0
    };
    let load_options = format!("start={start:.6},pause=no");
    // MPV 0.38 and newer place the playlist index before per-file options:
    // loadfile <url> <flags> <index> <options>.
    client
        .command(&["loadfile", &source.url, "replace", "-1", &load_options])
        .map_err(|error| PreviewUnavailableReason::LoadFailed(error.to_string()))?;
    match wait_for_event(
        client,
        shared,
        EVENT_FILE_LOADED,
        Instant::now() + config.load_timeout,
        revision,
    ) {
        WaitOutcome::Reached => {}
        WaitOutcome::Cancelled => return Ok(None),
        WaitOutcome::TimedOut => {
            return Err(PreviewUnavailableReason::LoadFailed(
                "timed out waiting for the trailer".to_owned(),
            ));
        }
        WaitOutcome::Failed(message) => return Err(PreviewUnavailableReason::LoadFailed(message)),
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
        return Err(PreviewUnavailableReason::NoVideo);
    }

    let rotation = normalize_rotation(client.get_i64("video-params/rotate").unwrap_or(0));
    apply_scale_filter(client, config, rotation)?;
    if let Err(error) = client.set_flag("mute", source.muted) {
        tracing::debug!(%error, "hover preview could not apply the initial mute state");
    }
    let _ = client.set_flag("pause", false);

    match wait_for_event(
        client,
        shared,
        EVENT_PLAYBACK_RESTART,
        Instant::now() + config.load_timeout,
        revision,
    ) {
        WaitOutcome::Reached => {}
        WaitOutcome::Cancelled => return Ok(None),
        WaitOutcome::TimedOut => {
            return Err(PreviewUnavailableReason::LoadFailed(
                "timed out decoding the first trailer frame".to_owned(),
            ));
        }
        WaitOutcome::Failed(message) => return Err(PreviewUnavailableReason::LoadFailed(message)),
    }

    let duration = client
        .get_double("duration")
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or_default();
    Ok(Some(PlayingSource {
        generation: source.generation,
        duration,
        rotation,
        next_frame_due: Instant::now(),
        capture_failures: 0,
    }))
}

fn apply_scale_filter(
    client: &MpvClient,
    config: &PreviewConfig,
    rotation: u16,
) -> Result<(), PreviewUnavailableReason> {
    let (width, height) = if matches!(rotation, 90 | 270) {
        (config.max_height, config.max_width)
    } else {
        (config.max_width, config.max_height)
    };
    client
        .set_string(
            "vf",
            &format!("scale=w={width}:h={height}:force_original_aspect_ratio=decrease"),
        )
        .map_err(|error| PreviewUnavailableReason::LoadFailed(error.to_string()))
}

fn capture_frame(
    client: &MpvClient,
    playing: &PlayingSource,
) -> Result<PreviewFrame, PreviewUnavailableReason> {
    let result = client
        .command_result(&["screenshot-raw", "video", "rgba"])
        .map_err(|error| PreviewUnavailableReason::ScreenshotFailed(error.to_string()))?;
    let raw = parse_screenshot(result.as_node()).map_err(PreviewUnavailableReason::InvalidFrame)?;
    let (width, height, rgba) = rotate_rgba(raw.width, raw.height, raw.rgba, playing.rotation)
        .map_err(PreviewUnavailableReason::InvalidFrame)?;
    let position = client
        .get_double("time-pos")
        .ok()
        .filter(|value| value.is_finite())
        .unwrap_or_default();
    Ok(PreviewFrame {
        generation: playing.generation,
        width,
        height,
        position,
        duration: playing.duration,
        rgba: Arc::from(rgba),
    })
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
    revision: u64,
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

        if !is_source_current(shared, revision) {
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
        if mailbox.source_revision != revision {
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

fn is_source_current(shared: &SharedMailbox, revision: u64) -> bool {
    lock_mailbox(shared).source_revision == revision
}

fn wait_for_activity(shared: &SharedMailbox) {
    let wakeup_revision = shared.mpv_wakeup_revision.load(Ordering::Acquire);
    let mut mailbox = lock_mailbox(shared);
    while mailbox.controls.is_empty()
        && shared.mpv_wakeup_revision.load(Ordering::Acquire) == wakeup_revision
    {
        mailbox = shared
            .condvar
            .wait(mailbox)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

/// Sleeps until the next frame is due, waking early for any control message.
fn wait_for_deadline(shared: &SharedMailbox, deadline: Instant) {
    let mut mailbox = lock_mailbox(shared);
    while mailbox.controls.is_empty() && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let (next, _) = shared
            .condvar
            .wait_timeout(mailbox, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        mailbox = next;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_frame_interval_is_rejected() {
        let config = PreviewConfig {
            frame_interval: Duration::ZERO,
            ..PreviewConfig::default()
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn zero_bounds_are_rejected() {
        let config = PreviewConfig {
            max_width: 0,
            ..PreviewConfig::default()
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn default_config_is_valid() {
        assert!(validate_config(&PreviewConfig::default()).is_ok());
    }

    #[test]
    fn a_new_source_invalidates_the_previous_revision() {
        let shared = SharedMailbox::default();
        let controller = PreviewController {
            shared: Arc::new(shared),
        };
        let source = PreviewSource {
            generation: 1,
            url: "https://example.invalid/trailer.mp4".to_owned(),
            start_seconds: 0.0,
            muted: true,
        };
        controller.play(source.clone()).expect("first play queued");
        let first = lock_mailbox(&controller.shared).source_revision;
        controller.play(source).expect("second play queued");
        assert!(!is_source_current(&controller.shared, first));
    }

    #[test]
    fn unavailable_reasons_summarize_their_message() {
        assert_eq!(
            PreviewUnavailableReason::LoadFailed("no route to host".to_owned()).summary(),
            "no route to host"
        );
        assert_eq!(
            PreviewUnavailableReason::NoVideo.summary(),
            "the trailer has no video track"
        );
    }
}
