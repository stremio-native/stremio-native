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
        duration: Duration,
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

        std::thread::Builder::new()
            .name("media-session".to_owned())
            .spawn(move || {
                let mut worker = Worker::new(on_event);
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

    pub fn set_metadata(&self, title: &str, cover_url: Option<&str>, duration_secs: i64) {
        self.send(Command::Metadata {
            title: title.to_owned(),
            cover_url: cover_url.map(str::to_owned),
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
    /// Taken and installed the first time the controls are created.
    pending_handler: Option<EventHandler>,
    wake: Option<keepawake::KeepAwake>,
    last_metadata: Option<(String, Option<String>, Duration)>,
    last_playback: Option<(bool, Duration)>,
}

impl Worker {
    fn new(on_event: EventHandler) -> Self {
        Self {
            controls: None,
            pending_handler: Some(on_event),
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
                duration,
            } => self.set_metadata(title, cover_url, duration),
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

    fn set_metadata(&mut self, title: String, cover_url: Option<String>, duration: Duration) {
        let key = (title, cover_url, duration);
        if self.last_metadata.as_ref() == Some(&key) {
            return;
        }
        if let Some(controls) = self.controls.as_mut() {
            let (title, cover_url, duration) = &key;
            let result = controls.set_metadata(MediaMetadata {
                title: Some(title),
                cover_url: cover_url.as_deref(),
                duration: Some(*duration),
                ..MediaMetadata::default()
            });
            if let Err(error) = result {
                tracing::warn!(%error, "OS media metadata update failed");
            }
        }
        self.last_metadata = Some(key);
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
}
