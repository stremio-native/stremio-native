//! Drives the muted hover trailer preview behind the floating catalog popup.
//!
//! Ownership mirrors [`crate::thumbnail_preview`]: this type holds the UI
//! projection and the session bookkeeping, while `playback_mpv::PreviewRuntime`
//! owns the decoder thread. Every session carries a generation so frames from a
//! trailer the pointer already left are dropped instead of flashing into the
//! popup of the next card.

use std::sync::{Arc, Mutex, MutexGuard};

use playback_mpv::{
    PreviewController, PreviewEvent, PreviewFrame, PreviewSource, PreviewUnavailableReason,
};
use slint::ComponentHandle;

use crate::{MainWindow, playback::TrailerPreview};

/// Trailers open a few seconds in: the first frames are usually a distributor
/// bumper or black, which reads as a broken preview in a card this small.
const TRAILER_START_SECONDS: f64 = 3.0;

#[derive(Clone)]
pub struct PreviewPlayer {
    state: Arc<Mutex<PreviewState>>,
    controller: Arc<Mutex<Option<PreviewController>>>,
    ui: slint::Weak<MainWindow>,
}

impl PreviewPlayer {
    pub fn new(enabled: bool, ui: slint::Weak<MainWindow>) -> Self {
        Self {
            state: Arc::new(Mutex::new(PreviewState::new(enabled))),
            controller: Arc::new(Mutex::new(None)),
            ui,
        }
    }

    pub fn attach_controller(&self, controller: PreviewController) {
        *lock_controller(&self.controller) = Some(controller);
        lock_state(&self.state).worker_ready = true;
    }

    pub fn worker_failed(&self, message: String) {
        let projection = {
            let mut state = lock_state(&self.state);
            state.worker_ready = false;
            state.loading = false;
            state.has_frame = false;
            state.status = format!("Preview decoder unavailable: {message}");
            state.projection()
        };
        self.schedule_projection(projection);
    }

    pub fn is_enabled(&self) -> bool {
        lock_state(&self.state).enabled
    }

    pub fn active_id(&self) -> Option<String> {
        lock_state(&self.state).active_id.clone()
    }

    /// Opens a session for the hovered item and projects its metadata.
    ///
    /// Runs on the UI thread, so the popup's text is populated in the same
    /// frame the popup becomes visible even when no trailer can be decoded.
    pub fn begin(&self, ui: &MainWindow, trailer: &TrailerPreview) {
        let (source, projection) = {
            let mut state = lock_state(&self.state);
            state.generation = state.generation.wrapping_add(1);
            state.active_id = Some(trailer.meta_id.clone());
            state.has_frame = false;
            let playable = state.enabled && state.worker_ready && trailer.stream_url.is_some();
            state.loading = playable;
            state.status = if !state.enabled {
                "Hover previews are off.".to_owned()
            } else if trailer.stream_url.is_none() {
                "No trailer available for this title.".to_owned()
            } else if !state.worker_ready {
                "The preview decoder is unavailable.".to_owned()
            } else {
                String::new()
            };
            let source = playable.then(|| PreviewSource {
                generation: state.generation,
                url: trailer.stream_url.clone().unwrap_or_default(),
                start_seconds: TRAILER_START_SECONDS,
                muted: state.muted,
            });
            (source, state.projection())
        };

        apply_metadata(ui, trailer);
        apply_projection(ui, &projection);
        ui.set_hover_preview_frame(slint::Image::default());

        if let Some(controller) = self.controller() {
            let result = match source {
                Some(source) => controller.play(source),
                None => controller.stop(),
            };
            log_worker_command(result);
        }
    }

    /// Ends the session and releases the decoder.
    pub fn dismiss(&self) {
        let projection = {
            let mut state = lock_state(&self.state);
            state.generation = state.generation.wrapping_add(1);
            state.active_id = None;
            state.loading = false;
            state.has_frame = false;
            state.status = String::new();
            state.projection()
        };
        if let Some(controller) = self.controller() {
            log_worker_command(controller.stop());
        }
        self.schedule_projection_with_clear(projection);
    }

    /// Flips the mute state and returns the new value.
    pub fn toggle_muted(&self) -> bool {
        let (muted, projection) = {
            let mut state = lock_state(&self.state);
            state.muted = !state.muted;
            (state.muted, state.projection())
        };
        if let Some(controller) = self.controller() {
            log_worker_command(controller.set_muted(muted));
        }
        self.schedule_projection(projection);
        muted
    }

    pub fn handle_event(&self, event: PreviewEvent) {
        match event {
            PreviewEvent::WorkerReady => {
                tracing::info!(worker = "mpv-preview", "hover preview worker ready");
            }
            PreviewEvent::Buffering { generation } => {
                let Some(projection) = self.update_if_current(generation, |state| {
                    state.loading = true;
                    state.status = String::new();
                }) else {
                    return;
                };
                self.schedule_projection(projection);
            }
            PreviewEvent::Playing {
                generation,
                duration,
            } => {
                tracing::debug!(generation, duration, "hover trailer preview playing");
            }
            PreviewEvent::Frame(frame) => self.handle_frame(frame),
            PreviewEvent::Unavailable { generation, reason } => {
                let Some(projection) = self.update_if_current(generation, |state| {
                    state.loading = false;
                    state.has_frame = false;
                    state.status = unavailable_status(&reason);
                }) else {
                    return;
                };
                self.schedule_projection_with_clear(projection);
            }
            PreviewEvent::Finished { generation } => {
                let Some(projection) = self.update_if_current(generation, |state| {
                    state.loading = false;
                }) else {
                    return;
                };
                self.schedule_projection(projection);
            }
            PreviewEvent::Shutdown => {
                tracing::info!(worker = "mpv-preview", "hover preview worker stopped");
            }
        }
    }

    fn handle_frame(&self, frame: PreviewFrame) {
        let Some(projection) = self.update_if_current(frame.generation, |state| {
            state.loading = false;
            state.has_frame = true;
        }) else {
            return;
        };
        let ui = self.ui.clone();
        let state = self.state.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if lock_state(&state).generation != frame.generation {
                return;
            }
            let Some(ui) = ui.upgrade() else {
                return;
            };
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                frame.rgba.as_ref(),
                frame.width,
                frame.height,
            );
            ui.set_hover_preview_frame(slint::Image::from_rgba8(buffer));
            apply_projection(&ui, &projection);
        });
    }

    /// Mutates the session state only while `generation` is still the live one.
    fn update_if_current(
        &self,
        generation: u64,
        mutate: impl FnOnce(&mut PreviewState),
    ) -> Option<UiProjection> {
        let mut state = lock_state(&self.state);
        if state.generation != generation || state.active_id.is_none() {
            return None;
        }
        mutate(&mut state);
        Some(state.projection())
    }

    fn controller(&self) -> Option<PreviewController> {
        lock_controller(&self.controller).clone()
    }

    fn schedule_projection(&self, projection: UiProjection) {
        self.dispatch_projection(projection, false);
    }

    fn schedule_projection_with_clear(&self, projection: UiProjection) {
        self.dispatch_projection(projection, true);
    }

    fn dispatch_projection(&self, projection: UiProjection, clear_image: bool) {
        let ui = self.ui.clone();
        let state = self.state.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if lock_state(&state).generation != projection.generation {
                return;
            }
            let Some(ui) = ui.upgrade() else {
                return;
            };
            if clear_image {
                ui.set_hover_preview_frame(slint::Image::default());
            }
            apply_projection(&ui, &projection);
        });
    }
}

struct PreviewState {
    enabled: bool,
    worker_ready: bool,
    generation: u64,
    active_id: Option<String>,
    muted: bool,
    loading: bool,
    has_frame: bool,
    status: String,
}

impl PreviewState {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            worker_ready: false,
            generation: 0,
            active_id: None,
            muted: true,
            loading: false,
            has_frame: false,
            status: String::new(),
        }
    }

    fn projection(&self) -> UiProjection {
        UiProjection {
            generation: self.generation,
            loading: self.loading,
            has_frame: self.has_frame,
            muted: self.muted,
            status: self.status.clone(),
        }
    }
}

struct UiProjection {
    generation: u64,
    loading: bool,
    has_frame: bool,
    muted: bool,
    status: String,
}

fn apply_projection(ui: &MainWindow, projection: &UiProjection) {
    ui.set_hover_preview_loading(projection.loading);
    ui.set_hover_preview_has_frame(projection.has_frame);
    ui.set_hover_preview_muted(projection.muted);
    ui.set_hover_preview_status(projection.status.as_str().into());
}

fn apply_metadata(ui: &MainWindow, trailer: &TrailerPreview) {
    let ui_weak = ui.as_weak();
    ui.set_hover_preview_title(trailer.title.as_str().into());
    ui.set_hover_preview_year(trailer.year.as_str().into());
    ui.set_hover_preview_runtime(trailer.runtime.as_str().into());
    ui.set_hover_preview_rating(trailer.rating.as_str().into());
    ui.set_hover_preview_description(trailer.description.as_str().into());
    ui.set_hover_preview_in_library(trailer.in_library);
    ui.set_hover_preview_genres(slint::ModelRc::new(slint::VecModel::from(
        trailer
            .genres
            .iter()
            .map(|genre| slint::SharedString::from(genre.as_str()))
            .collect::<Vec<_>>(),
    )));
    ui.set_hover_preview_poster(crate::image_cache::get_poster_image(
        &trailer.poster,
        &ui_weak,
    ));
}

fn unavailable_status(reason: &PreviewUnavailableReason) -> String {
    match reason {
        PreviewUnavailableReason::NoVideo => "This trailer has no video track.".to_owned(),
        other => format!("Preview unavailable: {}", other.summary()),
    }
}

fn lock_state(state: &Mutex<PreviewState>) -> MutexGuard<'_, PreviewState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_controller(
    controller: &Mutex<Option<PreviewController>>,
) -> MutexGuard<'_, Option<PreviewController>> {
    controller
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn log_worker_command(result: Result<(), playback_mpv::MpvError>) {
    if let Err(error) = result {
        tracing::debug!(%error, "hover preview worker command was not accepted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_session() -> PreviewState {
        let mut state = PreviewState::new(true);
        state.worker_ready = true;
        state.generation = 4;
        state.active_id = Some("tt0000001".to_owned());
        state
    }

    #[test]
    fn projection_carries_the_current_generation() {
        let state = state_with_session();
        assert_eq!(state.projection().generation, 4);
    }

    #[test]
    fn a_dismissed_session_has_no_active_item() {
        let mut state = state_with_session();
        state.active_id = None;
        assert!(state.active_id.is_none());
    }

    #[test]
    fn previews_start_muted() {
        assert!(PreviewState::new(true).muted);
    }

    #[test]
    fn missing_video_tracks_get_a_dedicated_message() {
        assert_eq!(
            unavailable_status(&PreviewUnavailableReason::NoVideo),
            "This trailer has no video track."
        );
        assert_eq!(
            unavailable_status(&PreviewUnavailableReason::LoadFailed(
                "timed out".to_owned()
            )),
            "Preview unavailable: timed out"
        );
    }
}
