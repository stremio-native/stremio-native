//! OS-level playback presence: system media controls (SMTC on Windows, MPRIS on
//! Linux, MediaRemote on macOS via `souvlaki`) plus a sleep inhibitor
//! (`keepawake`) that holds the machine and display awake while a video plays.
//!
//! Both concerns are driven by the same playback transitions and share one
//! dedicated worker thread. That single thread is deliberate:
//!
//! * `souvlaki::MediaControls` is created and owned in one place for its whole
//!   life, so it never has to cross a thread boundary. SMTC button events are
//!   delivered by WinRT on its own thread pool, so no message pump is required
//!   here.
//! * `keepawake` on Windows is `SetThreadExecutionState`, which is *thread
//!   affine*: the wake lock lives on the thread that set it and must be released
//!   on that same thread. A work-stealing async task could drop it on the wrong
//!   thread and leak the lock; a dedicated OS thread makes acquire/release
//!   always land on the same thread.

use std::{
    sync::mpsc::{self, Sender},
    time::Duration,
};

use slint::ComponentHandle;
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};

use crate::MainWindow;

const DISPLAY_NAME: &str = "Stremio Native";
const DBUS_NAME: &str = "stremio_native";
const WAKE_REVERSE_DOMAIN: &str = "com.stremio.native";

/// Handler invoked for every media-key / OS-control event. Runs on the media
/// backend's own thread, so it must be `Send`.
type EventHandler = Box<dyn Fn(MediaControlEvent) + Send + 'static>;

enum Command {
    /// Create the OS controls. `hwnd` is required on Windows and ignored
    /// elsewhere; the window is not known until it has been realized, so this
    /// arrives after construction.
    Attach {
        hwnd: Option<usize>,
    },
    Metadata {
        title: String,
        cover_url: Option<String>,
        /// Next-best cover tried when `cover_url` cannot be fetched or decoded.
        fallback_cover_url: Option<String>,
        duration: Duration,
    },
    CoverDownloaded {
        url: String,
        file_url: String,
    },
    Playback {
        playing: bool,
        position: Duration,
    },
    Clear,
}

/// Cheap, clonable handle to the media-session worker. Every method is a
/// non-blocking channel send; if the worker or the OS backend is unavailable the
/// calls are silently dropped.
pub struct MediaSession {
    commands: Sender<Command>,
}

impl MediaSession {
    /// Spawn a session whose OS control events drive `ui`'s player callbacks.
    pub fn for_window(ui: &MainWindow) -> Self {
        let ui_weak = ui.as_weak();
        Self::new(Box::new(move |event| {
            let ui_weak = ui_weak.clone();
            // Control events arrive on the OS backend's thread; hop to the UI
            // thread before touching the window.
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    apply_event(&ui, event);
                }
            });
        }))
    }

    /// Spawn the worker. `on_event` is called for each OS control event; it is
    /// consumed when the controls are first attached.
    pub fn new(on_event: EventHandler) -> Self {
        let (commands, receiver) = mpsc::channel::<Command>();
        let commands_clone = commands.clone();
        // Capture the Tokio runtime here, on the caller's runtime thread; the
        // worker below runs on a plain OS thread where `tokio::spawn` would panic,
        // so cover preparation is dispatched through this handle instead.
        let runtime = tokio::runtime::Handle::try_current().ok();

        std::thread::Builder::new()
            .name("media-session".to_owned())
            .spawn(move || {
                let mut worker = Worker::new(on_event, commands_clone, runtime);
                while let Ok(command) = receiver.recv() {
                    worker.handle(command);
                }
                // Channel closed: drop controls and wake lock on this thread,
                // which detaches SMTC and restores the execution state where it
                // was set.
            })
            .expect("spawn media-session thread");

        Self { commands }
    }

    /// Create the OS controls once the native window exists. Safe to call more
    /// than once; only the first successful attach takes effect.
    pub fn attach(&self, hwnd: Option<usize>) {
        self.send(Command::Attach { hwnd });
    }

    pub fn set_metadata(
        &self,
        title: &str,
        cover_url: Option<&str>,
        fallback_cover_url: Option<&str>,
        duration_secs: i64,
    ) {
        self.send(Command::Metadata {
            title: title.to_owned(),
            cover_url: cover_url.map(str::to_owned),
            fallback_cover_url: fallback_cover_url.map(str::to_owned),
            duration: secs_to_duration(duration_secs),
        });
    }

    pub fn set_playback(&self, playing: bool, position_secs: i64) {
        self.send(Command::Playback {
            playing,
            position: secs_to_duration(position_secs),
        });
    }

    /// Report that nothing is playing: clears the OS controls and releases the
    /// wake lock.
    pub fn clear(&self) {
        self.send(Command::Clear);
    }

    fn send(&self, command: Command) {
        // A disconnected worker only happens during shutdown; nothing to do.
        let _ = self.commands.send(command);
    }
}

struct Worker {
    controls: Option<MediaControls>,
    commands: Sender<Command>,
    /// Taken and installed the first time the controls are created.
    pending_handler: Option<EventHandler>,
    /// Runtime used to prepare cover art off this OS thread; `None` if the session
    /// was created outside a Tokio runtime, in which case remote covers are skipped.
    runtime: Option<tokio::runtime::Handle>,
    wake: Option<keepawake::KeepAwake>,
    last_metadata: Option<(String, Option<String>, Duration)>,
    last_playback: Option<(bool, Duration)>,
}

impl Worker {
    fn new(
        on_event: EventHandler,
        commands: Sender<Command>,
        runtime: Option<tokio::runtime::Handle>,
    ) -> Self {
        Self {
            controls: None,
            commands,
            pending_handler: Some(on_event),
            runtime,
            wake: None,
            last_metadata: None,
            last_playback: None,
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Attach { hwnd } => self.attach(hwnd),
            Command::Metadata {
                title,
                cover_url,
                fallback_cover_url,
                duration,
            } => self.set_metadata(title, cover_url, fallback_cover_url, duration),
            Command::CoverDownloaded { url, file_url } => {
                self.handle_cover_downloaded(url, file_url);
            }
            Command::Playback { playing, position } => self.set_playback(playing, position),
            Command::Clear => self.clear(),
        }
    }

    fn attach(&mut self, hwnd: Option<usize>) {
        if self.controls.is_some() {
            return;
        }
        // Windows SMTC needs the window handle; without it there is nothing to
        // create yet, so wait for a later attach that carries one.
        if cfg!(target_os = "windows") && hwnd.is_none() {
            return;
        }
        let Some(handler) = self.pending_handler.take() else {
            return;
        };

        let config = PlatformConfig {
            display_name: DISPLAY_NAME,
            dbus_name: DBUS_NAME,
            hwnd: hwnd.map(|hwnd| hwnd as *mut std::ffi::c_void),
        };

        match MediaControls::new(config) {
            Ok(mut controls) => {
                if let Err(error) = controls.attach(handler) {
                    tracing::warn!(%error, "OS media controls could not attach an event handler");
                    // Attach failed: keep the controls anyway (metadata/playback
                    // still display) but the handler is gone; a retry is pointless.
                }
                self.controls = Some(controls);
                tracing::info!("OS media controls attached");
            }
            Err(error) => {
                tracing::warn!(%error, "OS media controls are unavailable");
                // Restore the handler so a later attach can retry.
                self.pending_handler = Some(handler);
            }
        }
    }

    fn set_metadata(
        &mut self,
        title: String,
        cover_url: Option<String>,
        fallback_cover_url: Option<String>,
        duration: Duration,
    ) {
        let key = (title.clone(), cover_url.clone(), duration);
        if self.last_metadata.as_ref() == Some(&key) {
            return;
        }

        let resolved_cover_url = self.resolve_cover(&cover_url, fallback_cover_url);
        self.apply_metadata(&title, resolved_cover_url.as_deref(), duration);
        self.last_metadata = Some(key);
    }

    /// Turns the requested cover into a local `file://` path the OS controls can
    /// display, or `None` if it is not ready yet.
    ///
    /// The OS controls load a local file far more reliably than a remote URL, and
    /// Windows SMTC additionally cannot decode the WebP/AVIF posters often arrive
    /// as — so `cover_art_*` re-encodes to a PNG the shell can always show. When
    /// nothing is prepared yet, each candidate (primary then fallback) is tried in
    /// order in the background, and the first that succeeds is applied via
    /// `CoverDownloaded`, keyed on the primary URL so it matches the current item.
    fn resolve_cover(
        &self,
        cover_url: &Option<String>,
        fallback_cover_url: Option<String>,
    ) -> Option<String> {
        let Some(url) = cover_url else {
            return None;
        };
        if let Some(local) = url.strip_prefix("file://") {
            let clean = local.strip_prefix('/').unwrap_or(local);
            return Some(path_to_file_url(std::path::Path::new(clean)));
        }

        let candidates: Vec<String> = std::iter::once(url.clone())
            .chain(fallback_cover_url)
            .collect();
        if let Some(path) = candidates
            .iter()
            .find_map(|candidate| crate::image_cache::cover_art_cached(candidate))
        {
            return Some(path_to_file_url(&path));
        }

        // Not prepared yet: build it on the Tokio runtime (this worker thread has
        // none of its own) and apply it once ready via `CoverDownloaded`.
        let runtime = self.runtime.as_ref()?;
        let sender = self.commands.clone();
        let primary = url.clone();
        runtime.spawn(async move {
            for candidate in candidates {
                if let Some(path) = crate::image_cache::cover_art_file(candidate).await {
                    let _ = sender.send(Command::CoverDownloaded {
                        file_url: path_to_file_url(&path),
                        url: primary,
                    });
                    return;
                }
            }
            tracing::warn!(%primary, "media cover art could not be prepared from any source");
        });
        None
    }

    fn handle_cover_downloaded(&mut self, url: String, file_url: String) {
        // Apply the freshly cached cover only if the current item still wants it.
        let matched = match &self.last_metadata {
            Some((title, Some(last_url), duration)) if *last_url == url => {
                Some((title.clone(), *duration))
            }
            _ => None,
        };
        if let Some((title, duration)) = matched {
            self.apply_metadata(&title, Some(&file_url), duration);
        }
    }

    fn apply_metadata(&mut self, title: &str, cover_url: Option<&str>, duration: Duration) {
        if let Some(controls) = self.controls.as_mut() {
            let duration_opt = if duration.is_zero() {
                None
            } else {
                Some(duration)
            };
            let result = controls.set_metadata(MediaMetadata {
                title: Some(title),
                artist: Some("Stremio"),
                cover_url,
                duration: duration_opt,
                ..MediaMetadata::default()
            });
            if let Err(error) = result {
                tracing::warn!(%error, "OS media metadata update failed with cover art; retrying without cover art");
                if cover_url.is_some() {
                    let fallback_result = controls.set_metadata(MediaMetadata {
                        title: Some(title),
                        artist: Some("Stremio"),
                        cover_url: None,
                        duration: duration_opt,
                        ..MediaMetadata::default()
                    });
                    if let Err(err2) = fallback_result {
                        tracing::warn!(error = %err2, "OS media metadata update failed even without cover art");
                    }
                }
            }
        }
    }

    fn set_playback(&mut self, playing: bool, position: Duration) {
        self.set_wake(playing);

        if self.last_playback == Some((playing, position)) {
            return;
        }
        if let Some(controls) = self.controls.as_mut() {
            let progress = Some(MediaPosition(position));
            let playback = if playing {
                MediaPlayback::Playing { progress }
            } else {
                MediaPlayback::Paused { progress }
            };
            if let Err(error) = controls.set_playback(playback) {
                tracing::warn!(%error, "OS media playback update failed");
            }
        }
        self.last_playback = Some((playing, position));
    }

    fn clear(&mut self) {
        self.set_wake(false);
        self.last_metadata = None;
        self.last_playback = None;
        if let Some(controls) = self.controls.as_mut()
            && let Err(error) = controls.set_playback(MediaPlayback::Stopped)
        {
            tracing::warn!(%error, "OS media playback stop failed");
        }
    }

    /// Hold or release the sleep inhibitor. Acquiring and releasing both happen
    /// on this worker thread, satisfying the platform's thread affinity.
    fn set_wake(&mut self, awake: bool) {
        if awake == self.wake.is_some() {
            return;
        }
        if awake {
            match keepawake::Builder::default()
                .display(true)
                .idle(true)
                .reason("Playing video")
                .app_name(DISPLAY_NAME)
                .app_reverse_domain(WAKE_REVERSE_DOMAIN)
                .create()
            {
                Ok(handle) => self.wake = Some(handle),
                Err(error) => tracing::warn!(%error, "could not inhibit system sleep"),
            }
        } else {
            self.wake = None;
        }
    }
}

fn secs_to_duration(secs: i64) -> Duration {
    Duration::from_secs(secs.max(0) as u64)
}

fn path_to_file_url(path: &std::path::Path) -> String {
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(current_dir) = std::env::current_dir() {
        current_dir.join(path)
    } else {
        path.to_path_buf()
    };

    let path_str = abs_path.to_string_lossy();
    if cfg!(target_os = "windows") {
        let clean_path = path_str.strip_prefix(r"\\?\").unwrap_or(&path_str);
        let win_path = clean_path.replace('/', r"\");
        format!("file://{}", win_path)
    } else {
        format!("file://{}", path_str)
    }
}

/// Translate an OS control event into the player's existing callbacks, which
/// already carry the playback controller and its state.
fn apply_event(ui: &MainWindow, event: MediaControlEvent) {
    if !ui.get_show_player() {
        return;
    }
    match event {
        MediaControlEvent::Play => {
            if ui.get_player_paused() {
                ui.invoke_player_toggle_pause();
            }
        }
        MediaControlEvent::Pause => {
            if !ui.get_player_paused() {
                ui.invoke_player_toggle_pause();
            }
        }
        MediaControlEvent::Toggle => ui.invoke_player_toggle_pause(),
        MediaControlEvent::Stop | MediaControlEvent::Quit => ui.invoke_player_close(),
        MediaControlEvent::Next => {
            if ui.get_player_is_series() && ui.get_player_has_next_episode() {
                ui.invoke_player_next_episode();
            }
        }
        MediaControlEvent::Seek(direction) => {
            ui.invoke_player_seek_relative(signed_seek(
                ui.get_player_seek_step_seconds(),
                direction,
            ));
        }
        MediaControlEvent::SeekBy(direction, amount) => {
            ui.invoke_player_seek_relative(signed_seek(amount.as_secs_f32(), direction));
        }
        MediaControlEvent::SetPosition(MediaPosition(position)) => {
            let duration = ui.get_player_duration_seconds();
            if duration > 0.0 {
                let fraction = (position.as_secs_f32() / duration).clamp(0.0, 1.0);
                ui.invoke_player_seek(fraction);
            }
        }
        // Previous-episode, volume, open-uri and raise have no player
        // equivalent; ignore them rather than guess at a mapping.
        _ => return,
    }
    ui.invoke_player_activity();
}

fn signed_seek(magnitude_secs: f32, direction: SeekDirection) -> f32 {
    match direction {
        SeekDirection::Forward => magnitude_secs,
        SeekDirection::Backward => -magnitude_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_direction_sets_the_sign() {
        assert_eq!(signed_seek(10.0, SeekDirection::Forward), 10.0);
        assert_eq!(signed_seek(10.0, SeekDirection::Backward), -10.0);
    }

    #[test]
    fn negative_durations_clamp_to_zero() {
        assert_eq!(secs_to_duration(-5), Duration::ZERO);
        assert_eq!(secs_to_duration(42), Duration::from_secs(42));
    }

    #[test]
    fn path_to_file_url_produces_absolute_uri() {
        let relative = std::path::Path::new("storage/image-cache-v1/ab/test.jpg");
        let url = path_to_file_url(relative);
        assert!(url.starts_with("file://"));
        assert!(!url.contains("file://storage"));
    }
}
