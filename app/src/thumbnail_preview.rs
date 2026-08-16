use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

use crate::MainWindow;
use playback_mpv::{
    ThumbnailController, ThumbnailEvent, ThumbnailFrame, ThumbnailQuality, ThumbnailRequest,
    ThumbnailSource, ThumbnailUnavailableReason,
};

const LEGACY_SCRIPT_HEADER: &str =
    "-- thumbfast.lua: Thumbnail preview generator for libmpv embeddings";

#[derive(Clone)]
pub struct ThumbnailPreview {
    coordinator: Arc<Mutex<ThumbnailCoordinator>>,
    controller: Arc<Mutex<Option<ThumbnailController>>>,
    ui: slint::Weak<MainWindow>,
}

impl ThumbnailPreview {
    pub fn new(enabled: bool, ui: slint::Weak<MainWindow>) -> Self {
        Self {
            coordinator: Arc::new(Mutex::new(ThumbnailCoordinator::new(enabled))),
            controller: Arc::new(Mutex::new(None)),
            ui,
        }
    }

    pub fn attach_controller(&self, controller: ThumbnailController) {
        *lock_controller(&self.controller) = Some(controller);
        let projection = {
            let mut coordinator = lock_coordinator(&self.coordinator);
            coordinator.worker_ready = true;
            coordinator.status = if coordinator.enabled {
                "Waiting for a video…".to_owned()
            } else {
                "Timeline previews are off.".to_owned()
            };
            coordinator.next_projection(false)
        };
        self.schedule_projection(projection);
    }

    pub fn worker_failed(&self, message: String) {
        let projection = {
            let mut coordinator = lock_coordinator(&self.coordinator);
            coordinator.worker_ready = false;
            coordinator.source_state = SourceState::Unavailable;
            coordinator.hover = None;
            coordinator.has_frame = false;
            coordinator.status = format!("Thumbnail decoder unavailable: {message}");
            coordinator.next_projection(true)
        };
        self.schedule_projection(projection);
    }

    pub fn handle_event(&self, event: ThumbnailEvent) {
        match event {
            ThumbnailEvent::WorkerReady => {
                tracing::info!(worker = "mpv-thumbnail", "thumbnail worker ready");
            }
            ThumbnailEvent::SourceReady {
                generation,
                duration,
            } => {
                let projection = {
                    let mut coordinator = lock_coordinator(&self.coordinator);
                    if generation != coordinator.generation {
                        return;
                    }
                    let readiness_ms = coordinator
                        .warming_started
                        .map(|started| started.elapsed().as_millis());
                    tracing::info!(
                        generation,
                        duration_seconds = duration,
                        ?readiness_ms,
                        "thumbnail source ready"
                    );
                    coordinator.source_state = SourceState::Ready;
                    coordinator.warming_started = None;
                    coordinator.status = "Timeline previews ready.".to_owned();
                    coordinator.next_projection(false)
                };
                self.schedule_projection(projection);
            }
            ThumbnailEvent::SourceUnavailable { generation, reason } => {
                let projection = {
                    let mut coordinator = lock_coordinator(&self.coordinator);
                    if generation != coordinator.generation {
                        return;
                    }
                    tracing::warn!(generation, reason = ?reason, "thumbnail source unavailable");
                    coordinator.source_state = SourceState::Unavailable;
                    coordinator.warming_started = None;
                    coordinator.hover = None;
                    coordinator.has_frame = false;
                    coordinator.status = reason_status(&reason);
                    coordinator.next_projection(true)
                };
                self.schedule_projection(projection);
            }
            ThumbnailEvent::Frame(frame) => self.handle_frame(frame),
            ThumbnailEvent::RequestFailed {
                generation,
                request_id,
                reason,
            } => {
                let projection = {
                    let mut coordinator = lock_coordinator(&self.coordinator);
                    let Some(hover) = coordinator.hover.as_ref() else {
                        return;
                    };
                    if generation != coordinator.generation || request_id != hover.request_id {
                        return;
                    }
                    tracing::debug!(
                        generation,
                        request_id,
                        reason = ?reason,
                        "thumbnail request failed"
                    );
                    coordinator.loading = false;
                    coordinator.status =
                        format!("Preview unavailable: {}", reason_summary(&reason));
                    coordinator.next_projection(false)
                };
                self.schedule_projection(projection);
            }
            ThumbnailEvent::Shutdown => {
                tracing::info!(worker = "mpv-thumbnail", "thumbnail worker stopped");
            }
        }
    }

    fn handle_frame(&self, frame: ThumbnailFrame) {
        let update = {
            let mut coordinator = lock_coordinator(&self.coordinator);
            let Some(hover) = coordinator.hover.as_ref() else {
                return;
            };
            if !coordinator.accepts_frame(frame.generation, frame.request_id) {
                return;
            }
            let latency_ms = hover.requested_at.elapsed().as_millis();
            tracing::debug!(
                generation = frame.generation,
                request_id = frame.request_id,
                requested_seconds = frame.requested_seconds,
                decoded_seconds = frame.decoded_seconds,
                quality = ?frame.quality,
                width = frame.width,
                height = frame.height,
                bytes = frame.rgba.len(),
                latency_ms,
                "thumbnail frame decoded"
            );
            coordinator.has_frame = true;
            coordinator.loading = false;
            coordinator.aspect_ratio = frame.width as f32 / frame.height.max(1) as f32;
            coordinator.last_quality = Some(frame.quality);
            coordinator.status = "Timeline previews ready.".to_owned();
            let projection = coordinator.next_projection(false);
            FrameUpdate {
                token: projection.token,
                generation: frame.generation,
                request_id: frame.request_id,
                width: frame.width,
                height: frame.height,
                rgba: frame.rgba,
                projection,
            }
        };
        self.schedule_frame(update);
    }

    #[cfg_attr(feature = "profiling", hotpath::measure)]
    pub fn begin_load(&self, generation: u64) {
        let projection = {
            let mut coordinator = lock_coordinator(&self.coordinator);
            coordinator.generation = generation;
            coordinator.source_state = SourceState::Idle;
            coordinator.warming_started = None;
            coordinator.hover = None;
            coordinator.has_frame = false;
            coordinator.loading = false;
            coordinator.aspect_ratio = 16.0 / 9.0;
            coordinator.last_quality = None;
            coordinator.status = if coordinator.enabled {
                "Waiting for the video to load…".to_owned()
            } else {
                "Timeline previews are off.".to_owned()
            };
            coordinator.next_projection(true)
        };
        if let Some(controller) = self.controller() {
            log_worker_command(controller.unload());
        }
        self.schedule_projection(projection);
    }

    #[cfg_attr(feature = "profiling", hotpath::measure)]
    pub fn prewarm(&self, source: ThumbnailSource) {
        let should_load = {
            let mut coordinator = lock_coordinator(&self.coordinator);
            if !coordinator.enabled
                || !coordinator.worker_ready
                || source.generation != coordinator.generation
            {
                false
            } else {
                coordinator.source_state = SourceState::Warming;
                coordinator.warming_started = Some(Instant::now());
                coordinator.status = "Preparing timeline previews…".to_owned();
                let projection = coordinator.next_projection(false);
                drop(coordinator);
                self.schedule_projection(projection);
                true
            }
        };
        if should_load && let Some(controller) = self.controller() {
            log_worker_command(controller.load_source(source));
        }
    }

    pub fn hover(&self, progress: f32, duration: f64) {
        let command = {
            let mut coordinator = lock_coordinator(&self.coordinator);
            if !coordinator.enabled
                || !coordinator.worker_ready
                || !duration.is_finite()
                || duration <= 0.0
                || matches!(
                    coordinator.source_state,
                    SourceState::Idle | SourceState::Unavailable
                )
            {
                return;
            }
            let progress = progress.clamp(0.0, 1.0);
            let seconds = (duration * f64::from(progress)).clamp(0.0, (duration - 0.001).max(0.0));
            coordinator.next_request_id = coordinator.next_request_id.wrapping_add(1).max(1);
            let request_id = coordinator.next_request_id;
            coordinator.hover = Some(HoverState {
                request_id,
                requested_at: Instant::now(),
            });
            coordinator.loading = true;
            coordinator.hover_x = progress;
            coordinator.time_label = format_timestamp(seconds);
            let request = ThumbnailRequest {
                generation: coordinator.generation,
                request_id,
                seconds,
            };
            tracing::debug!(
                generation = request.generation,
                request_id,
                requested_seconds = seconds,
                "thumbnail requested"
            );
            let projection = coordinator.next_projection(false);
            (request, projection)
        };
        self.apply_projection_now(command.1);
        if let Some(controller) = self.controller() {
            log_worker_command(controller.request(command.0));
        }
    }

    pub fn leave(&self) {
        let projection = {
            let mut coordinator = lock_coordinator(&self.coordinator);
            coordinator.hover = None;
            coordinator.loading = false;
            coordinator.next_projection(false)
        };
        self.apply_projection_now(projection);
        if let Some(controller) = self.controller() {
            log_worker_command(controller.clear());
        }
    }

    pub fn unload(&self, generation: u64) {
        let projection = {
            let mut coordinator = lock_coordinator(&self.coordinator);
            coordinator.generation = generation;
            coordinator.source_state = SourceState::Idle;
            coordinator.warming_started = None;
            coordinator.hover = None;
            coordinator.loading = false;
            coordinator.has_frame = false;
            coordinator.last_quality = None;
            coordinator.status = if coordinator.enabled {
                "Waiting for a video…".to_owned()
            } else {
                "Timeline previews are off.".to_owned()
            };
            coordinator.next_projection(true)
        };
        if let Some(controller) = self.controller() {
            log_worker_command(controller.unload());
        }
        self.schedule_projection(projection);
    }

    pub fn set_enabled(&self, enabled: bool, current_source: Option<ThumbnailSource>) {
        let (load, projection) = {
            let mut coordinator = lock_coordinator(&self.coordinator);
            coordinator.enabled = enabled;
            coordinator.hover = None;
            coordinator.loading = false;
            coordinator.has_frame = false;
            coordinator.last_quality = None;
            let load = if enabled && coordinator.worker_ready {
                if let Some(source) = current_source
                    && source.generation == coordinator.generation
                {
                    coordinator.source_state = SourceState::Warming;
                    coordinator.warming_started = Some(Instant::now());
                    coordinator.status = "Preparing timeline previews…".to_owned();
                    Some(source)
                } else {
                    coordinator.source_state = SourceState::Idle;
                    coordinator.status = "Waiting for a video…".to_owned();
                    None
                }
            } else {
                coordinator.source_state = SourceState::Idle;
                coordinator.warming_started = None;
                coordinator.status = if enabled {
                    "Thumbnail decoder unavailable.".to_owned()
                } else {
                    "Timeline previews are off.".to_owned()
                };
                None
            };
            (load, coordinator.next_projection(true))
        };
        self.apply_projection_now(projection);
        if let Some(controller) = self.controller() {
            if let Some(source) = load {
                log_worker_command(controller.load_source(source));
            } else {
                log_worker_command(controller.unload());
            }
        }
    }

    fn controller(&self) -> Option<ThumbnailController> {
        lock_controller(&self.controller).clone()
    }

    fn apply_projection_now(&self, projection: UiProjection) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        if self.token_is_current(projection.token) {
            apply_projection(&ui, &projection);
        }
    }

    fn schedule_projection(&self, projection: UiProjection) {
        let ui = self.ui.clone();
        let coordinator = self.coordinator.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if lock_coordinator(&coordinator).ui_token != projection.token {
                return;
            }
            if let Some(ui) = ui.upgrade() {
                apply_projection(&ui, &projection);
            }
        });
    }

    fn schedule_frame(&self, update: FrameUpdate) {
        let ui = self.ui.clone();
        let coordinator = self.coordinator.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let current = {
                let coordinator = lock_coordinator(&coordinator);
                coordinator.ui_token == update.token
                    && coordinator.generation == update.generation
                    && coordinator
                        .hover
                        .as_ref()
                        .is_some_and(|hover| hover.request_id == update.request_id)
            };
            if !current {
                return;
            }
            let Some(ui) = ui.upgrade() else {
                return;
            };
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                update.rgba.as_ref(),
                update.width,
                update.height,
            );
            ui.set_player_thumbnail_preview(slint::Image::from_rgba8(buffer));
            apply_projection(&ui, &update.projection);
        });
    }

    fn token_is_current(&self, token: u64) -> bool {
        lock_coordinator(&self.coordinator).ui_token == token
    }
}

struct ThumbnailCoordinator {
    enabled: bool,
    worker_ready: bool,
    generation: u64,
    source_state: SourceState,
    warming_started: Option<Instant>,
    hover: Option<HoverState>,
    next_request_id: u64,
    ui_token: u64,
    loading: bool,
    has_frame: bool,
    aspect_ratio: f32,
    hover_x: f32,
    time_label: String,
    status: String,
    last_quality: Option<ThumbnailQuality>,
}

impl ThumbnailCoordinator {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            worker_ready: false,
            generation: 0,
            source_state: SourceState::Idle,
            warming_started: None,
            hover: None,
            next_request_id: 0,
            ui_token: 0,
            loading: false,
            has_frame: false,
            aspect_ratio: 16.0 / 9.0,
            hover_x: 0.0,
            time_label: "00:00".to_owned(),
            status: if enabled {
                "Starting thumbnail decoder…".to_owned()
            } else {
                "Timeline previews are off.".to_owned()
            },
            last_quality: None,
        }
    }

    fn next_projection(&mut self, clear_image: bool) -> UiProjection {
        self.ui_token = self.ui_token.wrapping_add(1);
        UiProjection {
            token: self.ui_token,
            visible: self.enabled && self.hover.is_some(),
            loading: self.loading,
            has_frame: self.has_frame,
            aspect_ratio: self.aspect_ratio,
            hover_x: self.hover_x,
            time_label: self.time_label.clone(),
            status: self.status.clone(),
            clear_image,
        }
    }

    fn accepts_frame(&self, generation: u64, request_id: u64) -> bool {
        self.enabled
            && generation == self.generation
            && self
                .hover
                .as_ref()
                .is_some_and(|hover| hover.request_id == request_id)
    }
}

enum SourceState {
    Idle,
    Warming,
    Ready,
    Unavailable,
}

struct HoverState {
    request_id: u64,
    requested_at: Instant,
}

struct UiProjection {
    token: u64,
    visible: bool,
    loading: bool,
    has_frame: bool,
    aspect_ratio: f32,
    hover_x: f32,
    time_label: String,
    status: String,
    clear_image: bool,
}

struct FrameUpdate {
    token: u64,
    generation: u64,
    request_id: u64,
    width: u32,
    height: u32,
    rgba: Arc<[u8]>,
    projection: UiProjection,
}

fn apply_projection(ui: &MainWindow, projection: &UiProjection) {
    ui.set_player_thumbnail_visible(projection.visible);
    ui.set_player_thumbnail_loading(projection.loading);
    ui.set_player_thumbnail_has_frame(projection.has_frame);
    ui.set_player_thumbnail_aspect_ratio(projection.aspect_ratio);
    ui.set_player_thumbnail_hover_x(projection.hover_x);
    ui.set_player_thumbnail_time_label(projection.time_label.clone().into());
    ui.set_settings_thumbnail_preview_status(projection.status.clone().into());
    if projection.clear_image {
        ui.set_player_thumbnail_preview(slint::Image::default());
    }
}

fn lock_coordinator(
    coordinator: &Mutex<ThumbnailCoordinator>,
) -> MutexGuard<'_, ThumbnailCoordinator> {
    coordinator
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_controller(
    controller: &Mutex<Option<ThumbnailController>>,
) -> MutexGuard<'_, Option<ThumbnailController>> {
    controller
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn log_worker_command(result: Result<(), playback_mpv::MpvError>) {
    if let Err(error) = result {
        tracing::debug!(%error, "thumbnail worker command was not accepted");
    }
}

fn format_timestamp(seconds: f64) -> String {
    let seconds = seconds.max(0.0).floor() as u64;
    let hours = seconds / 3_600;
    let minutes = (seconds / 60) % 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn reason_status(reason: &ThumbnailUnavailableReason) -> String {
    match reason {
        ThumbnailUnavailableReason::NoVideo => {
            "Timeline previews are unavailable for audio-only media.".to_owned()
        }
        ThumbnailUnavailableReason::NotSeekable => {
            "Timeline previews require a seekable video.".to_owned()
        }
        _ => format!("Timeline previews unavailable: {}", reason_summary(reason)),
    }
}

fn reason_summary(reason: &ThumbnailUnavailableReason) -> &str {
    match reason {
        ThumbnailUnavailableReason::NoVideo => "no video track",
        ThumbnailUnavailableReason::NotSeekable => "the stream is not seekable",
        ThumbnailUnavailableReason::LoadFailed(message)
        | ThumbnailUnavailableReason::SeekFailed(message)
        | ThumbnailUnavailableReason::ScreenshotFailed(message)
        | ThumbnailUnavailableReason::InvalidFrame(message) => message,
    }
}

/// Renames only the legacy script generated by this application.
pub fn disable_legacy_script(config_dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    let script = config_dir.join("scripts").join("thumbfast.lua");
    if !script.is_file() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&script)?;
    if !contents.starts_with(LEGACY_SCRIPT_HEADER) {
        tracing::info!(path = %script.display(), "leaving user-managed ThumbFast script enabled");
        return Ok(None);
    }
    let preferred = script.with_extension("lua.legacy-disabled");
    let destination = if preferred.exists() {
        script.with_extension(format!("lua.legacy-disabled-{}", std::process::id()))
    } else {
        preferred
    };
    fs::rename(&script, &destination)?;
    tracing::info!(
        from = %script.display(),
        to = %destination.display(),
        "disabled the obsolete app-generated ThumbFast script"
    );
    Ok(Some(destination))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_rejects_frames_from_stale_requests() {
        let mut coordinator = ThumbnailCoordinator::new(true);
        coordinator.worker_ready = true;
        coordinator.generation = 7;
        coordinator.source_state = SourceState::Ready;
        coordinator.hover = Some(HoverState {
            request_id: 3,
            requested_at: Instant::now(),
        });
        assert!(!coordinator.accepts_frame(7, 2));
    }

    #[test]
    fn coordinator_rejects_frames_from_stale_generations() {
        let mut coordinator = ThumbnailCoordinator::new(true);
        coordinator.worker_ready = true;
        coordinator.generation = 7;
        coordinator.source_state = SourceState::Ready;
        coordinator.hover = Some(HoverState {
            request_id: 3,
            requested_at: Instant::now(),
        });
        assert!(!coordinator.accepts_frame(6, 3));
    }

    #[test]
    fn newer_ui_projection_invalidates_an_already_queued_projection() {
        let mut coordinator = ThumbnailCoordinator::new(true);
        let stale = coordinator.next_projection(false);
        let current = coordinator.next_projection(false);
        assert_ne!(stale.token, current.token);
    }

    #[test]
    fn timestamp_includes_hours_when_needed() {
        assert_eq!(format_timestamp(3_661.9), "01:01:01");
    }
}
