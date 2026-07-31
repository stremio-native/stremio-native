use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use slint::winit_030::winit::{
    dpi::{PhysicalPosition, PhysicalSize},
    window::{Fullscreen, Window, WindowLevel},
};
use tokio_util::sync::CancellationToken;

const PIP_WIDTH: u32 = 480;
const PIP_HEIGHT: u32 = 320;
const PIP_INSET: i32 = 16;
const RECOVERY_STABLE_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SleepMode {
    After(Duration),
    EndOfCurrent,
    EndOfNext,
}

impl SleepMode {
    pub fn from_ui_value(value: i32) -> Option<Self> {
        match value {
            -2 => Some(Self::EndOfNext),
            -1 => Some(Self::EndOfCurrent),
            15 | 30 | 45 | 60 | 120 | 180 | 240 | 360 => {
                Some(Self::After(Duration::from_secs(value as u64 * 60)))
            }
            _ => None,
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::After(duration) => format!("{} min", duration.as_secs() / 60),
            Self::EndOfCurrent => "End of episode".to_owned(),
            Self::EndOfNext => "End of next episode".to_owned(),
        }
    }

    fn remaining_ends(self) -> u8 {
        match self {
            Self::After(_) => 0,
            Self::EndOfCurrent => 1,
            Self::EndOfNext => 2,
        }
    }
}

#[derive(Debug)]
pub struct SleepTimerState {
    pub mode: SleepMode,
    remaining_ends: u8,
    pub cancellation: CancellationToken,
}

impl SleepTimerState {
    pub fn new(mode: SleepMode) -> Self {
        Self {
            mode,
            remaining_ends: mode.remaining_ends(),
            cancellation: CancellationToken::new(),
        }
    }

    /// Returns true when an episode boundary should stop playback instead of
    /// advancing to the next episode.
    pub fn consume_episode_end(&mut self) -> bool {
        if self.remaining_ends == 0 {
            return false;
        }

        self.remaining_ends -= 1;
        self.remaining_ends == 0
    }
}

#[derive(Clone, Debug, Default)]
pub struct RecoveryState {
    automatic_retry_used: bool,
    stable_since: Option<Instant>,
}

impl RecoveryState {
    pub fn reset_for_source(&mut self) {
        self.automatic_retry_used = false;
        self.stable_since = None;
    }

    pub fn claim_automatic_retry(&mut self) -> bool {
        if self.automatic_retry_used {
            return false;
        }

        self.automatic_retry_used = true;
        self.stable_since = None;
        true
    }

    pub fn observe_playback(&mut self, loaded: bool, paused: bool, buffering: bool, now: Instant) {
        if !loaded || paused || buffering {
            self.stable_since = None;
            return;
        }

        let stable_since = self.stable_since.get_or_insert(now);
        if now.duration_since(*stable_since) >= RECOVERY_STABLE_WINDOW {
            self.automatic_retry_used = false;
            self.stable_since = Some(now);
        }
    }
}

#[derive(Clone, Debug)]
pub struct PipSnapshot {
    pub position: Option<PhysicalPosition<i32>>,
    pub size: PhysicalSize<u32>,
    pub maximized: bool,
    pub fullscreen: Option<Fullscreen>,
    pub decorated: bool,
    pub resizable: bool,
}

#[derive(Debug, Default)]
pub struct PipController {
    snapshot: Option<PipSnapshot>,
}

impl PipController {
    pub fn is_active(&self) -> bool {
        self.snapshot.is_some()
    }

    pub fn toggle(&mut self, window: &Window) -> bool {
        if self.is_active() {
            self.exit(window);
        } else {
            self.enter(window);
        }

        self.is_active()
    }

    pub fn exit(&mut self, window: &Window) {
        let Some(snapshot) = self.snapshot.take() else {
            return;
        };

        window.set_window_level(WindowLevel::Normal);
        window.set_decorations(snapshot.decorated);
        window.set_resizable(snapshot.resizable);
        window.set_fullscreen(snapshot.fullscreen);
        window.set_maximized(snapshot.maximized);
        let _ = window.request_inner_size(snapshot.size);
        if let Some(position) = snapshot.position {
            window.set_outer_position(position);
        }
    }

    fn enter(&mut self, window: &Window) {
        self.snapshot = Some(PipSnapshot {
            position: window.outer_position().ok(),
            size: window.inner_size(),
            maximized: window.is_maximized(),
            fullscreen: window.fullscreen(),
            decorated: window.is_decorated(),
            resizable: window.is_resizable(),
        });

        window.set_fullscreen(None);
        window.set_maximized(false);
        window.set_decorations(false);
        window.set_resizable(false);
        window.set_window_level(WindowLevel::AlwaysOnTop);

        let size = PhysicalSize::new(PIP_WIDTH, PIP_HEIGHT);
        let _ = window.request_inner_size(size);
        if let Some(monitor) = window.current_monitor() {
            window.set_outer_position(pip_position(
                monitor.position(),
                monitor.size(),
                size,
                PIP_INSET,
            ));
        }
    }
}

fn pip_position(
    monitor_position: PhysicalPosition<i32>,
    monitor_size: PhysicalSize<u32>,
    window_size: PhysicalSize<u32>,
    inset: i32,
) -> PhysicalPosition<i32> {
    let x = i64::from(monitor_position.x) + i64::from(monitor_size.width)
        - i64::from(window_size.width)
        - i64::from(inset);
    let y = i64::from(monitor_position.y) + i64::from(monitor_size.height)
        - i64::from(window_size.height)
        - i64::from(inset);
    PhysicalPosition::new(clamp_i64_to_i32(x), clamp_i64_to_i32(y))
}

fn clamp_i64_to_i32(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

pub fn prepare_capture_directory() -> std::io::Result<PathBuf> {
    let preferred =
        dirs::picture_dir().map(|pictures| pictures.join("Stremio Native").join("Captures"));
    let fallback = crate::paths::get().root().join("captures");

    if let Some(preferred) = preferred
        && fs::create_dir_all(&preferred).is_ok()
    {
        return Ok(preferred);
    }

    fs::create_dir_all(&fallback)?;
    Ok(fallback)
}

pub fn capture_path(
    directory: &Path,
    title: &str,
    episode: Option<&str>,
    timestamp: DateTime<Utc>,
    request_id: u64,
) -> PathBuf {
    let mut stem = sanitize_filename_component(title);
    if let Some(episode) = episode.filter(|episode| !episode.trim().is_empty()) {
        stem.push('-');
        stem.push_str(&sanitize_filename_component(episode));
    }
    stem.push('-');
    stem.push_str(&timestamp.format("%Y%m%d-%H%M%S").to_string());
    stem.push('-');
    stem.push_str(&request_id.to_string());
    directory.join(format!("{stem}.png"))
}

fn sanitize_filename_component(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len().min(80));
    let mut previous_was_separator = false;

    for character in value.chars().take(80) {
        if character.is_alphanumeric() || matches!(character, '-' | '_') {
            sanitized.push(character);
            previous_was_separator = false;
        } else if !previous_was_separator && !sanitized.is_empty() {
            sanitized.push('_');
            previous_was_separator = true;
        }
    }

    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        "capture".to_owned()
    } else {
        sanitized.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_of_next_timer_stops_on_second_boundary() {
        let mut timer = SleepTimerState::new(SleepMode::EndOfNext);

        assert!(!timer.consume_episode_end());
        assert!(timer.consume_episode_end());
    }

    #[test]
    fn recovery_claims_only_one_retry() {
        let mut state = RecoveryState::default();

        assert!(state.claim_automatic_retry());
        assert!(!state.claim_automatic_retry());
        state.reset_for_source();
        assert!(state.claim_automatic_retry());
    }

    #[test]
    fn stable_playback_restores_retry_budget() {
        let mut state = RecoveryState::default();
        assert!(state.claim_automatic_retry());
        let start = Instant::now();

        state.observe_playback(true, false, false, start);
        state.observe_playback(true, false, false, start + RECOVERY_STABLE_WINDOW);

        assert!(state.claim_automatic_retry());
    }

    #[test]
    fn pip_position_uses_monitor_origin_and_inset() {
        assert_eq!(
            pip_position(
                PhysicalPosition::new(1920, 0),
                PhysicalSize::new(1920, 1080),
                PhysicalSize::new(480, 320),
                16,
            ),
            PhysicalPosition::new(3344, 744)
        );
    }

    #[test]
    fn capture_filename_is_sanitized_and_unique() {
        let timestamp = DateTime::parse_from_rfc3339("2026-07-28T12:30:45Z")
            .expect("valid timestamp")
            .with_timezone(&Utc);

        assert_eq!(
            capture_path(Path::new("captures"), "Show: S01/E02?", None, timestamp, 7,),
            Path::new("captures").join("Show_S01_E02-20260728-123045-7.png")
        );
    }
}
