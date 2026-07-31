use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use slint::{
    ComponentHandle, Model,
    winit_030::{EventResult, WinitWindowAccessor, winit},
};

use crate::{MainWindow, media_session::MediaSession};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HotkeyAction {
    ToggleFullscreen,
    TogglePause,
    TemporaryDoubleSpeed,
    SeekBackward,
    SeekBackwardShort,
    SeekForward,
    SeekForwardShort,
    VolumeUp,
    VolumeDown,
    ToggleMute,
    SubtitleSizeDown,
    SubtitleSizeUp,
    SubtitleDelayDown,
    SubtitleDelayUp,
    SpeedDown,
    SpeedUp,
    ToggleSubtitles,
    OpenSubtitles,
    OpenAudio,
    OpenEpisodes,
    OpenSpeed,
    OpenStats,
    NextEpisode,
    TogglePictureInPicture,
    CaptureFrame,
    CloseOverlayOrPlayer,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HotkeyContext {
    Global,
    Player,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HotkeyBehavior {
    Press,
    Hold,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct KeyChord {
    pub key: String,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(default, rename = "meta")]
    pub meta: bool,
}

impl KeyChord {
    fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
        }
    }

    fn shift(key: impl Into<String>) -> Self {
        Self {
            shift: true,
            ..Self::new(key)
        }
    }

    fn ctrl_shift(key: impl Into<String>) -> Self {
        Self {
            ctrl: true,
            shift: true,
            ..Self::new(key)
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HotkeyBinding {
    pub action: HotkeyAction,
    pub context: HotkeyContext,
    pub chord: KeyChord,
    pub behavior: HotkeyBehavior,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HotkeyConfig {
    pub version: u32,
    pub bindings: Vec<HotkeyBinding>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HotkeyValidationError {
    Reserved(KeyChord),
    Conflict {
        context: HotkeyContext,
        chord: KeyChord,
        first: HotkeyAction,
        second: HotkeyAction,
    },
    EmptyKey(HotkeyAction),
}

impl HotkeyConfig {
    const VERSION: u32 = 1;

    pub fn defaults() -> Self {
        let player = |action, chord| HotkeyBinding {
            action,
            context: HotkeyContext::Player,
            chord,
            behavior: HotkeyBehavior::Press,
        };
        let mut bindings = vec![
            HotkeyBinding {
                action: HotkeyAction::ToggleFullscreen,
                context: HotkeyContext::Global,
                chord: KeyChord::new("f"),
                behavior: HotkeyBehavior::Press,
            },
            player(HotkeyAction::TogglePause, KeyChord::new("k")),
            HotkeyBinding {
                action: HotkeyAction::TemporaryDoubleSpeed,
                context: HotkeyContext::Player,
                chord: KeyChord::new("space"),
                behavior: HotkeyBehavior::Hold,
            },
            player(HotkeyAction::SeekBackward, KeyChord::new("arrow-left")),
            player(
                HotkeyAction::SeekBackwardShort,
                KeyChord::shift("arrow-left"),
            ),
            player(HotkeyAction::SeekForward, KeyChord::new("arrow-right")),
            player(
                HotkeyAction::SeekForwardShort,
                KeyChord::shift("arrow-right"),
            ),
            player(HotkeyAction::VolumeUp, KeyChord::new("arrow-up")),
            player(HotkeyAction::VolumeDown, KeyChord::new("arrow-down")),
            player(HotkeyAction::ToggleMute, KeyChord::new("m")),
            player(HotkeyAction::SubtitleSizeDown, KeyChord::new("-")),
            player(HotkeyAction::SubtitleSizeUp, KeyChord::new("=")),
            player(HotkeyAction::SubtitleDelayDown, KeyChord::new("g")),
            player(HotkeyAction::SubtitleDelayUp, KeyChord::new("h")),
            player(HotkeyAction::SpeedDown, KeyChord::new("[")),
            player(HotkeyAction::SpeedUp, KeyChord::new("]")),
            player(HotkeyAction::ToggleSubtitles, KeyChord::new("c")),
            player(HotkeyAction::OpenSubtitles, KeyChord::new("s")),
            player(HotkeyAction::OpenAudio, KeyChord::new("a")),
            player(HotkeyAction::OpenEpisodes, KeyChord::new("i")),
            player(HotkeyAction::OpenSpeed, KeyChord::new("r")),
            player(HotkeyAction::OpenStats, KeyChord::new("d")),
            player(HotkeyAction::NextEpisode, KeyChord::shift("n")),
            player(HotkeyAction::TogglePictureInPicture, KeyChord::new("p")),
            player(HotkeyAction::CaptureFrame, KeyChord::ctrl_shift("s")),
            player(HotkeyAction::CloseOverlayOrPlayer, KeyChord::new("escape")),
        ];
        bindings.shrink_to_fit();
        Self {
            version: Self::VERSION,
            bindings,
        }
    }

    pub fn validate(&self) -> Result<(), HotkeyValidationError> {
        let mut seen = std::collections::HashMap::new();
        for binding in &self.bindings {
            if binding.chord.key.trim().is_empty() {
                return Err(HotkeyValidationError::EmptyKey(binding.action));
            }
            if is_reserved(&binding.chord) {
                return Err(HotkeyValidationError::Reserved(binding.chord.clone()));
            }
            let key = (binding.context, binding.chord.clone());
            if let Some(first) = seen.insert(key, binding.action) {
                return Err(HotkeyValidationError::Conflict {
                    context: binding.context,
                    chord: binding.chord.clone(),
                    first,
                    second: binding.action,
                });
            }
        }
        Ok(())
    }

    pub fn reset_to_defaults(path: &std::path::Path) -> std::io::Result<Self> {
        let defaults = Self::defaults();
        let encoded = serde_json::to_vec_pretty(&defaults)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        fs::write(path, encoded)?;
        Ok(defaults)
    }

    fn rebind(
        &mut self,
        action: HotkeyAction,
        chord: KeyChord,
    ) -> Result<(), HotkeyValidationError> {
        let binding = self
            .bindings
            .iter_mut()
            .find(|binding| binding.action == action)
            .ok_or(HotkeyValidationError::EmptyKey(action))?;
        binding.chord = chord;
        self.validate()
    }
}

impl std::fmt::Display for HotkeyValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reserved(chord) => write!(
                formatter,
                "{} is reserved by the operating system",
                format_chord(chord)
            ),
            Self::Conflict {
                chord,
                first,
                second,
                ..
            } => write!(
                formatter,
                "{} conflicts between {} and {}",
                format_chord(chord),
                action_label(*first),
                action_label(*second)
            ),
            Self::EmptyKey(action) => {
                write!(formatter, "{} requires a key", action_label(*action))
            }
        }
    }
}

fn is_reserved(chord: &KeyChord) -> bool {
    (chord.alt && chord.key.eq_ignore_ascii_case("f4"))
        || (chord.meta && chord.key.eq_ignore_ascii_case("q"))
        || (chord.ctrl && chord.alt && chord.key.eq_ignore_ascii_case("delete"))
}

fn config_path() -> PathBuf {
    crate::paths::get().root().join("hotkeys.json")
}

fn load_config() -> HotkeyConfig {
    let path = config_path();
    let config = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<HotkeyConfig>(&bytes).ok());
    match config {
        Some(config) if config.version == HotkeyConfig::VERSION && config.validate().is_ok() => {
            config
        }
        Some(_) => {
            tracing::warn!(path = %path.display(), "invalid hotkey configuration; using defaults");
            HotkeyConfig::defaults()
        }
        None => HotkeyConfig::defaults(),
    }
}

fn save_config(config: &HotkeyConfig) -> std::io::Result<()> {
    let encoded = serde_json::to_vec_pretty(config)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    fs::write(config_path(), encoded)
}

#[derive(Default)]
struct HotkeyDispatcher {
    modifiers: winit::keyboard::ModifiersState,
    ime_editing: bool,
    pressed: HashSet<HotkeyAction>,
    active_keys: HashMap<String, HotkeyAction>,
    double_speed_restore: Option<f32>,
    double_speed_started_at: Option<Instant>,
    last_subtitle_index: i32,
}

impl HotkeyDispatcher {
    fn chord(&self, key: &winit::keyboard::Key) -> Option<KeyChord> {
        let key = normalized_key(key)?;
        Some(KeyChord {
            key,
            ctrl: self.modifiers.control_key(),
            alt: self.modifiers.alt_key(),
            shift: self.modifiers.shift_key(),
            meta: self.modifiers.super_key(),
        })
    }

    fn press(
        &mut self,
        ui: &MainWindow,
        binding: &HotkeyBinding,
        key: &winit::keyboard::Key,
        repeat: bool,
    ) {
        if repeat {
            return;
        }
        let Some(key) = normalized_key(key) else {
            return;
        };
        if !self.pressed.insert(binding.action) {
            return;
        }
        self.active_keys.insert(key, binding.action);

        if binding.behavior == HotkeyBehavior::Hold {
            self.double_speed_restore = Some(ui.get_player_playback_speed());
            self.double_speed_started_at = Some(Instant::now());
            ui.set_player_playback_speed(2.0);
            ui.invoke_player_change_speed(2.0);
            return;
        }
        invoke_action(ui, binding.action, &mut self.last_subtitle_index);
    }

    fn take_active_action(&mut self, key: &str) -> Option<HotkeyAction> {
        self.active_keys.remove(key)
    }

    fn release_key(&mut self, ui: &MainWindow, key: &winit::keyboard::Key) -> bool {
        let Some(key) = normalized_key(key) else {
            return false;
        };
        let Some(action) = self.take_active_action(&key) else {
            return false;
        };
        self.release(ui, action);
        true
    }

    fn release(&mut self, ui: &MainWindow, action: HotkeyAction) {
        if !self.pressed.remove(&action) {
            return;
        }
        if action == HotkeyAction::TemporaryDoubleSpeed {
            let restore = self.double_speed_restore.take().unwrap_or(1.0);
            let quick_tap = self
                .double_speed_started_at
                .take()
                .is_some_and(|started| started.elapsed() < Duration::from_millis(400));
            ui.set_player_playback_speed(restore);
            ui.invoke_player_change_speed(restore);
            if quick_tap {
                ui.invoke_player_toggle_pause();
            }
        }
    }

    fn release_all(&mut self, ui: &MainWindow) {
        for action in std::mem::take(&mut self.active_keys).into_values() {
            self.release(ui, action);
        }
        self.pressed.clear();
    }
}

fn should_dispatch(binding: &HotkeyBinding, player_menu_open: bool) -> bool {
    !player_menu_open
        || binding.context != HotkeyContext::Player
        || binding.action == HotkeyAction::CloseOverlayOrPlayer
}

fn normalized_key(key: &winit::keyboard::Key) -> Option<String> {
    use winit::keyboard::{Key, NamedKey};
    match key {
        Key::Character(character) => Some(character.to_lowercase()),
        Key::Named(named) => Some(
            match named {
                NamedKey::Space => "space",
                NamedKey::Escape => "escape",
                NamedKey::ArrowLeft => "arrow-left",
                NamedKey::ArrowRight => "arrow-right",
                NamedKey::ArrowUp => "arrow-up",
                NamedKey::ArrowDown => "arrow-down",
                NamedKey::F4 => "f4",
                NamedKey::Delete => "delete",
                NamedKey::MediaPlayPause => "media-play-pause",
                NamedKey::MediaPlay => "media-play",
                NamedKey::MediaPause => "media-pause",
                NamedKey::MediaTrackNext => "media-track-next",
                _ => return None,
            }
            .to_owned(),
        ),
        _ => None,
    }
}

fn invoke_action(ui: &MainWindow, action: HotkeyAction, last_subtitle_index: &mut i32) {
    match action {
        HotkeyAction::ToggleFullscreen => {
            if ui.get_show_player() {
                ui.invoke_player_toggle_fullscreen();
            } else {
                ui.invoke_toggle_fullscreen();
            }
        }
        HotkeyAction::TogglePause => ui.invoke_player_toggle_pause(),
        HotkeyAction::TemporaryDoubleSpeed => {}
        HotkeyAction::SeekBackward => {
            ui.invoke_player_seek_relative(-ui.get_player_seek_step_seconds())
        }
        HotkeyAction::SeekBackwardShort => {
            ui.invoke_player_seek_relative(-ui.get_player_short_seek_step_seconds())
        }
        HotkeyAction::SeekForward => {
            ui.invoke_player_seek_relative(ui.get_player_seek_step_seconds())
        }
        HotkeyAction::SeekForwardShort => {
            ui.invoke_player_seek_relative(ui.get_player_short_seek_step_seconds())
        }
        HotkeyAction::VolumeUp => {
            let volume = (ui.get_player_volume() + 0.05).min(2.0);
            ui.set_player_volume(volume);
            ui.invoke_player_change_volume(volume);
        }
        HotkeyAction::VolumeDown => {
            let volume = (ui.get_player_volume() - 0.05).max(0.0);
            ui.set_player_volume(volume);
            ui.invoke_player_change_volume(volume);
        }
        HotkeyAction::ToggleMute => ui.invoke_player_toggle_mute(),
        HotkeyAction::SubtitleSizeDown => adjust_subtitle_size(ui, -1),
        HotkeyAction::SubtitleSizeUp => adjust_subtitle_size(ui, 1),
        HotkeyAction::SubtitleDelayDown => {
            let delay = ui.get_player_subtitle_delay_seconds() - 0.25;
            ui.set_player_subtitle_delay_seconds(delay);
            ui.invoke_player_change_subtitle_delay(delay);
        }
        HotkeyAction::SubtitleDelayUp => {
            let delay = ui.get_player_subtitle_delay_seconds() + 0.25;
            ui.set_player_subtitle_delay_seconds(delay);
            ui.invoke_player_change_subtitle_delay(delay);
        }
        HotkeyAction::SpeedDown => {
            let speed = (ui.get_player_playback_speed() - 0.25).max(0.25);
            ui.set_player_playback_speed(speed);
            ui.invoke_player_change_speed(speed);
        }
        HotkeyAction::SpeedUp => {
            let speed = (ui.get_player_playback_speed() + 0.25).min(2.0);
            ui.set_player_playback_speed(speed);
            ui.invoke_player_change_speed(speed);
        }
        HotkeyAction::ToggleSubtitles => {
            let active = ui.get_player_active_subtitle_idx();
            if active >= 0 {
                *last_subtitle_index = active;
                ui.invoke_player_change_subtitle(-1);
            } else if ui.get_player_subtitles_tracks().row_count() > 0 {
                ui.invoke_player_change_subtitle(*last_subtitle_index);
            }
        }
        HotkeyAction::OpenSubtitles => toggle_menu(ui, MenuKind::Subtitles),
        HotkeyAction::OpenAudio => toggle_menu(ui, MenuKind::Audio),
        HotkeyAction::OpenEpisodes => toggle_menu(ui, MenuKind::Episodes),
        HotkeyAction::OpenSpeed => toggle_menu(ui, MenuKind::Speed),
        HotkeyAction::OpenStats => toggle_menu(ui, MenuKind::Stats),
        HotkeyAction::NextEpisode => {
            if ui.get_player_is_series() && ui.get_player_has_next_episode() {
                ui.invoke_close_player_menus();
                ui.invoke_player_next_episode();
            }
        }
        HotkeyAction::TogglePictureInPicture => ui.invoke_player_toggle_pip(),
        HotkeyAction::CaptureFrame => ui.invoke_player_capture_frame(),
        HotkeyAction::CloseOverlayOrPlayer => {
            if ui.get_player_menu_open() {
                ui.invoke_close_player_menus();
            } else {
                ui.invoke_player_close();
            }
        }
    }
    ui.invoke_player_activity();
}

fn adjust_subtitle_size(ui: &MainWindow, direction: i8) {
    let current = ui.get_player_subtitle_size_percent();
    let values = [50.0, 75.0, 100.0, 125.0, 150.0, 175.0, 200.0, 250.0];
    let next = if direction > 0 {
        values
            .into_iter()
            .find(|value| *value > current)
            .unwrap_or(250.0)
    } else {
        values
            .into_iter()
            .rev()
            .find(|value| *value < current)
            .unwrap_or(50.0)
    };
    ui.set_player_subtitle_size_percent(next);
    ui.invoke_player_change_subtitle_size(next);
}

#[derive(Clone, Copy)]
enum MenuKind {
    Subtitles,
    Audio,
    Episodes,
    Speed,
    Stats,
}

fn toggle_menu(ui: &MainWindow, menu: MenuKind) {
    let was_open = match menu {
        MenuKind::Subtitles => ui.get_player_show_subtitles_menu(),
        MenuKind::Audio => ui.get_player_show_audio_menu(),
        MenuKind::Episodes => ui.get_player_show_playlist_drawer(),
        MenuKind::Speed => ui.get_player_show_speed_menu(),
        MenuKind::Stats => ui.get_player_show_stats_menu(),
    };
    ui.invoke_close_player_menus();
    if was_open {
        return;
    }
    match menu {
        MenuKind::Subtitles if ui.get_player_subtitles_tracks().row_count() > 0 => {
            ui.set_player_show_subtitles_menu(true)
        }
        MenuKind::Audio if ui.get_player_audio_tracks().row_count() > 0 => {
            ui.set_player_show_audio_menu(true)
        }
        MenuKind::Episodes if ui.get_player_is_series() => ui.set_player_show_playlist_drawer(true),
        MenuKind::Speed => ui.set_player_show_speed_menu(true),
        MenuKind::Stats => ui.set_player_show_stats_menu(true),
        _ => {}
    }
}

/// Installs typed native shortcuts, media keys, and minimize behavior.
pub fn install_platform_shortcuts(ui: &MainWindow, media_session: Arc<MediaSession>) {
    let weak_ui = ui.as_weak();
    let config = Arc::new(RwLock::new(load_config()));
    project_hotkeys(ui, &config);
    ui.on_hotkey_rebind({
        let config = config.clone();
        let weak = ui.as_weak();
        move |action, chord| {
            let Some(action) = action_from_id(action.as_str()) else {
                return;
            };
            let result = parse_chord(chord.as_str(), action).and_then(|chord| {
                let mut next = config
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                next.rebind(action, chord)?;
                save_config(&next).map_err(|_| HotkeyValidationError::EmptyKey(action))?;
                *config
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = next;
                Ok(())
            });
            if let Some(ui) = weak.upgrade() {
                match result {
                    Ok(()) => {
                        ui.set_hotkey_conflict("".into());
                        project_hotkeys(&ui, &config);
                    }
                    Err(error) => ui.set_hotkey_conflict(error.to_string().into()),
                }
            }
        }
    });
    ui.on_hotkey_reset({
        let config = config.clone();
        let weak = ui.as_weak();
        move || match HotkeyConfig::reset_to_defaults(&config_path()) {
            Ok(defaults) => {
                *config
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = defaults;
                if let Some(ui) = weak.upgrade() {
                    ui.set_hotkey_conflict("".into());
                    project_hotkeys(&ui, &config);
                }
            }
            Err(error) => {
                if let Some(ui) = weak.upgrade() {
                    ui.set_hotkey_conflict(error.to_string().into());
                }
            }
        }
    });
    let dispatch_config = config.clone();
    let mut dispatcher = HotkeyDispatcher::default();
    let mut native_window_style_applied = false;
    let mut media_controls_attached = false;

    crate::window_hooks::register(ui, move |window, event| {
        if !native_window_style_applied {
            native_window_style_applied = window
                .with_winit_window(crate::window_style::apply)
                .unwrap_or(false);
        }
        if !media_controls_attached
            && let Some(hwnd) = window.with_winit_window(crate::window_style::window_hwnd)
        {
            media_session.attach(hwnd);
            if let Some(raw_hwnd) = hwnd {
                crate::taskbar_media::init(raw_hwnd, weak_ui.clone());
            }
            media_controls_attached = true;
        }

        let Some(ui) = weak_ui.upgrade() else {
            return EventResult::Propagate;
        };

        match event {
            winit::event::WindowEvent::ModifiersChanged(modifiers) => {
                dispatcher.modifiers = modifiers.state();
                EventResult::Propagate
            }
            winit::event::WindowEvent::Ime(winit::event::Ime::Enabled) => {
                dispatcher.ime_editing = true;
                EventResult::Propagate
            }
            winit::event::WindowEvent::Ime(winit::event::Ime::Disabled) => {
                dispatcher.ime_editing = false;
                EventResult::Propagate
            }
            winit::event::WindowEvent::Focused(false) => {
                dispatcher.release_all(&ui);
                dispatcher.modifiers = winit::keyboard::ModifiersState::empty();
                dispatcher.ime_editing = false;
                EventResult::Propagate
            }
            winit::event::WindowEvent::KeyboardInput { event, .. } => {
                if handle_media_key(&ui, event) {
                    return EventResult::PreventDefault;
                }
                if event.state == winit::event::ElementState::Released
                    && dispatcher.release_key(&ui, &event.logical_key)
                {
                    return EventResult::PreventDefault;
                }
                // Text fields elsewhere in the shell need their keystrokes, so
                // an active IME session suppresses dispatch. The player hosts
                // no text input, so once it is showing an IME session can only
                // be one a field on another page left behind — honouring it
                // there would silence every player hotkey.
                if dispatcher.ime_editing && !ui.get_show_player() {
                    return EventResult::Propagate;
                }
                let Some(chord) = dispatcher.chord(&event.logical_key) else {
                    return EventResult::Propagate;
                };
                let binding = dispatch_config
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .bindings
                    .iter()
                    .find(|binding| {
                        binding.chord == chord
                            && match binding.context {
                                HotkeyContext::Global => true,
                                HotkeyContext::Player => ui.get_show_player(),
                            }
                    })
                    .cloned();
                let Some(binding) = binding else {
                    return EventResult::Propagate;
                };
                if !should_dispatch(&binding, ui.get_player_menu_open()) {
                    return EventResult::Propagate;
                }
                match event.state {
                    winit::event::ElementState::Pressed => {
                        dispatcher.press(&ui, &binding, &event.logical_key, event.repeat)
                    }
                    winit::event::ElementState::Released => dispatcher.release(&ui, binding.action),
                }
                EventResult::PreventDefault
            }
            winit::event::WindowEvent::Occluded(true)
                if ui.get_show_player()
                    && ui.get_settings_pause_on_minimize()
                    && !ui.get_player_paused() =>
            {
                ui.invoke_player_toggle_pause();
                EventResult::Propagate
            }
            _ => EventResult::Propagate,
        }
    });
}

fn project_hotkeys(ui: &MainWindow, config: &Arc<RwLock<HotkeyConfig>>) {
    let items = config
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .bindings
        .iter()
        .map(|binding| crate::HotkeyItem {
            id: action_id(binding.action).into(),
            label: action_label(binding.action).into(),
            context: match binding.context {
                HotkeyContext::Global => "Global",
                HotkeyContext::Player => "Player",
            }
            .into(),
            binding: format_chord(&binding.chord).into(),
        })
        .collect::<Vec<_>>();
    ui.set_hotkey_items(slint::ModelRc::new(slint::VecModel::from(items)));
}

fn parse_chord(value: &str, action: HotkeyAction) -> Result<KeyChord, HotkeyValidationError> {
    let mut chord = KeyChord::new("");
    for part in value
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => chord.ctrl = true,
            "alt" | "option" => chord.alt = true,
            "shift" => chord.shift = true,
            "meta" | "cmd" | "command" | "super" | "win" => chord.meta = true,
            key if chord.key.is_empty() => chord.key = normalize_config_key(key),
            _ => return Err(HotkeyValidationError::EmptyKey(action)),
        }
    }
    if chord.key.is_empty() {
        return Err(HotkeyValidationError::EmptyKey(action));
    }
    if is_reserved(&chord) {
        return Err(HotkeyValidationError::Reserved(chord));
    }
    Ok(chord)
}

fn normalize_config_key(value: &str) -> String {
    match value {
        "left" => "arrow-left",
        "right" => "arrow-right",
        "up" => "arrow-up",
        "down" => "arrow-down",
        "esc" => "escape",
        other => other,
    }
    .to_owned()
}

fn format_chord(chord: &KeyChord) -> String {
    let mut parts = Vec::new();
    if chord.ctrl {
        parts.push("Ctrl".to_owned());
    }
    if chord.alt {
        parts.push("Alt".to_owned());
    }
    if chord.shift {
        parts.push("Shift".to_owned());
    }
    if chord.meta {
        parts.push("Meta".to_owned());
    }
    parts.push(match chord.key.as_str() {
        "arrow-left" => "Left".to_owned(),
        "arrow-right" => "Right".to_owned(),
        "arrow-up" => "Up".to_owned(),
        "arrow-down" => "Down".to_owned(),
        key => key.to_ascii_uppercase(),
    });
    parts.join("+")
}

fn action_id(action: HotkeyAction) -> &'static str {
    match action {
        HotkeyAction::ToggleFullscreen => "toggle-fullscreen",
        HotkeyAction::TogglePause => "toggle-pause",
        HotkeyAction::TemporaryDoubleSpeed => "temporary-double-speed",
        HotkeyAction::SeekBackward => "seek-backward",
        HotkeyAction::SeekBackwardShort => "seek-backward-short",
        HotkeyAction::SeekForward => "seek-forward",
        HotkeyAction::SeekForwardShort => "seek-forward-short",
        HotkeyAction::VolumeUp => "volume-up",
        HotkeyAction::VolumeDown => "volume-down",
        HotkeyAction::ToggleMute => "toggle-mute",
        HotkeyAction::SubtitleSizeDown => "subtitle-size-down",
        HotkeyAction::SubtitleSizeUp => "subtitle-size-up",
        HotkeyAction::SubtitleDelayDown => "subtitle-delay-down",
        HotkeyAction::SubtitleDelayUp => "subtitle-delay-up",
        HotkeyAction::SpeedDown => "speed-down",
        HotkeyAction::SpeedUp => "speed-up",
        HotkeyAction::ToggleSubtitles => "toggle-subtitles",
        HotkeyAction::OpenSubtitles => "open-subtitles",
        HotkeyAction::OpenAudio => "open-audio",
        HotkeyAction::OpenEpisodes => "open-episodes",
        HotkeyAction::OpenSpeed => "open-speed",
        HotkeyAction::OpenStats => "open-stats",
        HotkeyAction::NextEpisode => "next-episode",
        HotkeyAction::TogglePictureInPicture => "toggle-picture-in-picture",
        HotkeyAction::CaptureFrame => "capture-frame",
        HotkeyAction::CloseOverlayOrPlayer => "close-overlay-or-player",
    }
}

fn action_from_id(value: &str) -> Option<HotkeyAction> {
    HotkeyConfig::defaults()
        .bindings
        .into_iter()
        .find(|binding| action_id(binding.action) == value)
        .map(|binding| binding.action)
}

fn action_label(action: HotkeyAction) -> &'static str {
    match action {
        HotkeyAction::ToggleFullscreen => "Toggle fullscreen",
        HotkeyAction::TogglePause => "Play / pause",
        HotkeyAction::TemporaryDoubleSpeed => "Temporary 2× speed",
        HotkeyAction::SeekBackward => "Seek backward",
        HotkeyAction::SeekBackwardShort => "Short seek backward",
        HotkeyAction::SeekForward => "Seek forward",
        HotkeyAction::SeekForwardShort => "Short seek forward",
        HotkeyAction::VolumeUp => "Volume up",
        HotkeyAction::VolumeDown => "Volume down",
        HotkeyAction::ToggleMute => "Mute",
        HotkeyAction::SubtitleSizeDown => "Subtitle size down",
        HotkeyAction::SubtitleSizeUp => "Subtitle size up",
        HotkeyAction::SubtitleDelayDown => "Subtitle delay down",
        HotkeyAction::SubtitleDelayUp => "Subtitle delay up",
        HotkeyAction::SpeedDown => "Playback speed down",
        HotkeyAction::SpeedUp => "Playback speed up",
        HotkeyAction::ToggleSubtitles => "Toggle subtitles",
        HotkeyAction::OpenSubtitles => "Subtitle menu",
        HotkeyAction::OpenAudio => "Audio menu",
        HotkeyAction::OpenEpisodes => "Episode menu",
        HotkeyAction::OpenSpeed => "Speed menu",
        HotkeyAction::OpenStats => "Playback statistics",
        HotkeyAction::NextEpisode => "Next episode",
        HotkeyAction::TogglePictureInPicture => "Picture in picture",
        HotkeyAction::CaptureFrame => "Capture frame",
        HotkeyAction::CloseOverlayOrPlayer => "Close overlay or player",
    }
}

fn handle_media_key(ui: &MainWindow, event: &winit::event::KeyEvent) -> bool {
    if event.state != winit::event::ElementState::Pressed || event.repeat || !ui.get_show_player() {
        return false;
    }
    use winit::keyboard::{Key, NamedKey};
    match &event.logical_key {
        Key::Named(NamedKey::MediaPlayPause) => ui.invoke_player_toggle_pause(),
        Key::Named(NamedKey::MediaPlay) if ui.get_player_paused() => {
            ui.invoke_player_toggle_pause()
        }
        Key::Named(NamedKey::MediaPause) if !ui.get_player_paused() => {
            ui.invoke_player_toggle_pause()
        }
        Key::Named(NamedKey::MediaTrackNext)
            if ui.get_player_is_series() && ui.get_player_has_next_episode() =>
        {
            ui.invoke_player_next_episode()
        }
        _ => return false,
    }
    ui.invoke_player_activity();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_no_conflicts_or_reserved_chords() {
        assert_eq!(HotkeyConfig::defaults().validate(), Ok(()));
    }

    #[test]
    fn conflict_is_scoped_to_context() {
        let chord = KeyChord::new("k");
        let config = HotkeyConfig {
            version: 1,
            bindings: vec![
                HotkeyBinding {
                    action: HotkeyAction::TogglePause,
                    context: HotkeyContext::Player,
                    chord: chord.clone(),
                    behavior: HotkeyBehavior::Press,
                },
                HotkeyBinding {
                    action: HotkeyAction::ToggleMute,
                    context: HotkeyContext::Player,
                    chord: chord.clone(),
                    behavior: HotkeyBehavior::Press,
                },
            ],
        };

        assert!(matches!(
            config.validate(),
            Err(HotkeyValidationError::Conflict { .. })
        ));
    }

    #[test]
    fn alt_f4_is_reserved() {
        assert!(is_reserved(&KeyChord {
            key: "f4".to_owned(),
            ctrl: false,
            alt: true,
            shift: false,
            meta: false,
        }));
    }

    #[test]
    fn active_action_is_recovered_by_base_key() {
        let mut dispatcher = HotkeyDispatcher::default();
        dispatcher
            .active_keys
            .insert("arrow-right".to_owned(), HotkeyAction::SeekForwardShort);

        assert_eq!(
            dispatcher.take_active_action("arrow-right"),
            Some(HotkeyAction::SeekForwardShort)
        );
    }

    #[test]
    fn player_navigation_hotkey_propagates_while_menu_is_open() {
        let binding = HotkeyBinding {
            action: HotkeyAction::SeekForward,
            context: HotkeyContext::Player,
            chord: KeyChord::new("arrow-right"),
            behavior: HotkeyBehavior::Press,
        };

        assert!(!should_dispatch(&binding, true));
    }

    #[test]
    fn escape_hotkey_still_closes_an_open_player_menu() {
        let binding = HotkeyBinding {
            action: HotkeyAction::CloseOverlayOrPlayer,
            context: HotkeyContext::Player,
            chord: KeyChord::new("escape"),
            behavior: HotkeyBehavior::Press,
        };

        assert!(should_dispatch(&binding, true));
    }
}
