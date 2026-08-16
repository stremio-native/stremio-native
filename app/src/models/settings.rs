use crate::config::AppConfig;
use crate::mpv_integration::NativePlaybackBridge;
use crate::{
    AppModel, AppModelField, MainWindow, OmniphonyAudioSettings as SlintOmniphonyAudioSettings,
};
use core_env::DesktopEnv;
use playback_mpv::{OmniphonyAudioSettings as PlaybackOmniphonyAudioSettings, SpatialAudioMode};
use serde::{Deserialize, Serialize};
use server_connector::{AppServerConnector, ServerConnector};
use slint::ComponentHandle;
use std::sync::{Arc, Mutex};
use stremio_core::{
    models::{common::Loadable, data_export::DataExport},
    runtime::{
        Runtime, RuntimeAction,
        msg::{Action, ActionCtx, ActionLoad, ActionStreamingServer},
    },
    types::{
        profile::Settings as ProfileSettings, server_urls::ServerUrlsBucket,
        streaming_server::Settings as StreamingServerSettings,
    },
};

const SPATIAL_AUDIO_MODE_SETTING: &str = "spatial_audio_mode";
const OMNIPHONY_AUDIO_SETTING: &str = "omniphony_audio_settings";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
struct StoredOmniphonyAudioSettings {
    headphones: bool,
    hrir_source: String,
    sofa_path: String,
    pinna_preset: String,
    pinna_d_scale_pct: u16,
    pinna_depth_pct: u16,
    prtf_frequency_scale_pct: u16,
    prtf_depth_pct: u16,
    unit_scale_m: f32,
    head_radius_m: f32,
    reflections_enabled: bool,
    reflection_level: f32,
    room_width_m: f32,
    room_depth_m: f32,
    room_height_m: f32,
    reverb_enabled: bool,
    reverb_level: f32,
    reverb_rt60_s: f32,
    reverb_predelay_ms: f32,
    air_absorption: bool,
    tracking_address: String,
    tracking_format: String,
    tracking_smoothing: f32,
    tracking_invert: bool,
    osc_rx_port: u16,
}

impl Default for StoredOmniphonyAudioSettings {
    fn default() -> Self {
        let defaults = PlaybackOmniphonyAudioSettings::default();
        Self {
            headphones: defaults.headphones,
            hrir_source: defaults.hrir_source,
            sofa_path: defaults.sofa_path,
            pinna_preset: defaults.pinna_preset,
            pinna_d_scale_pct: defaults.pinna_d_scale_pct,
            pinna_depth_pct: defaults.pinna_depth_pct,
            prtf_frequency_scale_pct: defaults.prtf_frequency_scale_pct,
            prtf_depth_pct: defaults.prtf_depth_pct,
            unit_scale_m: defaults.unit_scale_m,
            head_radius_m: defaults.head_radius_m,
            reflections_enabled: defaults.reflections_enabled,
            reflection_level: defaults.reflection_level,
            room_width_m: defaults.room_width_m,
            room_depth_m: defaults.room_depth_m,
            room_height_m: defaults.room_height_m,
            reverb_enabled: defaults.reverb_enabled,
            reverb_level: defaults.reverb_level,
            reverb_rt60_s: defaults.reverb_rt60_s,
            reverb_predelay_ms: defaults.reverb_predelay_ms,
            air_absorption: defaults.air_absorption,
            tracking_address: defaults.tracking_address,
            tracking_format: defaults.tracking_format,
            tracking_smoothing: defaults.tracking_smoothing,
            tracking_invert: defaults.tracking_invert,
            osc_rx_port: defaults.osc_rx_port,
        }
    }
}

impl StoredOmniphonyAudioSettings {
    fn sanitize(mut self) -> Self {
        let playback: PlaybackOmniphonyAudioSettings = self.clone().into();
        let playback = playback.sanitized();
        self = playback.into();
        self
    }
}

impl From<StoredOmniphonyAudioSettings> for PlaybackOmniphonyAudioSettings {
    fn from(value: StoredOmniphonyAudioSettings) -> Self {
        Self {
            headphones: value.headphones,
            hrir_source: value.hrir_source,
            sofa_path: value.sofa_path,
            pinna_preset: value.pinna_preset,
            pinna_d_scale_pct: value.pinna_d_scale_pct,
            pinna_depth_pct: value.pinna_depth_pct,
            prtf_frequency_scale_pct: value.prtf_frequency_scale_pct,
            prtf_depth_pct: value.prtf_depth_pct,
            unit_scale_m: value.unit_scale_m,
            head_radius_m: value.head_radius_m,
            reflections_enabled: value.reflections_enabled,
            reflection_level: value.reflection_level,
            room_width_m: value.room_width_m,
            room_depth_m: value.room_depth_m,
            room_height_m: value.room_height_m,
            reverb_enabled: value.reverb_enabled,
            reverb_level: value.reverb_level,
            reverb_rt60_s: value.reverb_rt60_s,
            reverb_predelay_ms: value.reverb_predelay_ms,
            air_absorption: value.air_absorption,
            tracking_address: value.tracking_address,
            tracking_format: value.tracking_format,
            tracking_smoothing: value.tracking_smoothing,
            tracking_invert: value.tracking_invert,
            osc_rx_port: value.osc_rx_port,
        }
    }
}

impl From<PlaybackOmniphonyAudioSettings> for StoredOmniphonyAudioSettings {
    fn from(value: PlaybackOmniphonyAudioSettings) -> Self {
        Self {
            headphones: value.headphones,
            hrir_source: value.hrir_source,
            sofa_path: value.sofa_path,
            pinna_preset: value.pinna_preset,
            pinna_d_scale_pct: value.pinna_d_scale_pct,
            pinna_depth_pct: value.pinna_depth_pct,
            prtf_frequency_scale_pct: value.prtf_frequency_scale_pct,
            prtf_depth_pct: value.prtf_depth_pct,
            unit_scale_m: value.unit_scale_m,
            head_radius_m: value.head_radius_m,
            reflections_enabled: value.reflections_enabled,
            reflection_level: value.reflection_level,
            room_width_m: value.room_width_m,
            room_depth_m: value.room_depth_m,
            room_height_m: value.room_height_m,
            reverb_enabled: value.reverb_enabled,
            reverb_level: value.reverb_level,
            reverb_rt60_s: value.reverb_rt60_s,
            reverb_predelay_ms: value.reverb_predelay_ms,
            air_absorption: value.air_absorption,
            tracking_address: value.tracking_address,
            tracking_format: value.tracking_format,
            tracking_smoothing: value.tracking_smoothing,
            tracking_invert: value.tracking_invert,
            osc_rx_port: value.osc_rx_port,
        }
    }
}

impl From<&StoredOmniphonyAudioSettings> for SlintOmniphonyAudioSettings {
    fn from(value: &StoredOmniphonyAudioSettings) -> Self {
        Self {
            headphones: value.headphones,
            hrir_source: value.hrir_source.clone().into(),
            sofa_path: value.sofa_path.clone().into(),
            pinna_preset: value.pinna_preset.clone().into(),
            pinna_d_scale_pct: f32::from(value.pinna_d_scale_pct),
            pinna_depth_pct: f32::from(value.pinna_depth_pct),
            prtf_frequency_scale_pct: f32::from(value.prtf_frequency_scale_pct),
            prtf_depth_pct: f32::from(value.prtf_depth_pct),
            unit_scale_m: value.unit_scale_m,
            head_radius_cm: value.head_radius_m * 100.0,
            reflections_enabled: value.reflections_enabled,
            reflection_level: value.reflection_level,
            room_width_m: value.room_width_m,
            room_depth_m: value.room_depth_m,
            room_height_m: value.room_height_m,
            reverb_enabled: value.reverb_enabled,
            reverb_level: value.reverb_level,
            reverb_rt60_s: value.reverb_rt60_s,
            reverb_predelay_ms: value.reverb_predelay_ms,
            air_absorption: value.air_absorption,
            tracking_address: value.tracking_address.clone().into(),
            tracking_format: value.tracking_format.clone().into(),
            tracking_smoothing: value.tracking_smoothing,
            tracking_invert: value.tracking_invert,
            osc_rx_port: i32::from(value.osc_rx_port),
        }
    }
}

fn spatial_audio_mode_from_value(value: &str) -> Option<SpatialAudioMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "disabled" => Some(SpatialAudioMode::Disabled),
        "omniphony" | "omniphony 3d" | "omniphony-spatial" => {
            Some(SpatialAudioMode::OmniphonySpatial)
        }
        "binaural-hrtf" | "binaural hrtf" => Some(SpatialAudioMode::BinauralHrtf),
        "surround-matrix" | "7.1 virtual surround" => Some(SpatialAudioMode::SurroundMatrix),
        _ => None,
    }
}

fn spatial_audio_mode_id(mode: SpatialAudioMode) -> &'static str {
    match mode {
        SpatialAudioMode::Disabled => "disabled",
        SpatialAudioMode::OmniphonySpatial => "omniphony",
        SpatialAudioMode::BinauralHrtf => "binaural-hrtf",
        SpatialAudioMode::SurroundMatrix => "surround-matrix",
    }
}

fn spatial_audio_mode_label(mode: SpatialAudioMode) -> &'static str {
    match mode {
        SpatialAudioMode::Disabled => "Disabled",
        SpatialAudioMode::OmniphonySpatial => "Omniphony 3D",
        SpatialAudioMode::BinauralHrtf => "Binaural HRTF",
        SpatialAudioMode::SurroundMatrix => "7.1 Virtual Surround",
    }
}

fn mutate_omniphony_settings(
    state: &Arc<Mutex<StoredOmniphonyAudioSettings>>,
    update: impl FnOnce(&mut StoredOmniphonyAudioSettings),
) -> StoredOmniphonyAudioSettings {
    let mut settings = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    update(&mut settings);
    *settings = settings.clone().sanitize();
    settings.clone()
}

fn apply_omniphony_settings(
    settings: StoredOmniphonyAudioSettings,
    ui: Option<&MainWindow>,
    playback: Option<&NativePlaybackBridge>,
) {
    if let Some(ui) = ui {
        ui.set_settings_omniphony_audio((&settings).into());
        ui.set_settings_spatial_audio_status("Applying Omniphony settings…".into());
    }
    if let Some(playback) = playback {
        playback.configure_omniphony_audio(settings.clone().into());
    }
    tokio::spawn(async move {
        let serialized = match serde_json::to_string(&settings) {
            Ok(serialized) => serialized,
            Err(error) => {
                tracing::warn!(%error, "could not serialize Omniphony audio settings");
                return;
            }
        };
        if let Err(error) = crate::db::set_setting(OMNIPHONY_AUDIO_SETTING, &serialized).await {
            tracing::warn!(%error, "could not persist Omniphony audio settings");
        }
    });
}

fn change_omniphony_string(settings: &mut StoredOmniphonyAudioSettings, key: &str, value: &str) {
    match key {
        "hrir_source" => settings.hrir_source = value.to_owned(),
        "sofa_path" => settings.sofa_path = value.trim().to_owned(),
        "pinna_preset" => settings.pinna_preset = value.to_owned(),
        "tracking_address" => settings.tracking_address = value.trim().to_owned(),
        "tracking_format" => settings.tracking_format = value.to_owned(),
        _ => tracing::warn!(key, "ignored unknown Omniphony string setting"),
    }
}

fn change_omniphony_number(settings: &mut StoredOmniphonyAudioSettings, key: &str, value: f32) {
    match key {
        "pinna_d_scale_pct" => settings.pinna_d_scale_pct = value.round() as u16,
        "pinna_depth_pct" => settings.pinna_depth_pct = value.round() as u16,
        "prtf_frequency_scale_pct" => {
            settings.prtf_frequency_scale_pct = value.round() as u16;
        }
        "prtf_depth_pct" => settings.prtf_depth_pct = value.round() as u16,
        "unit_scale_m" => settings.unit_scale_m = value,
        "head_radius_cm" => settings.head_radius_m = value / 100.0,
        "reflection_level" => settings.reflection_level = value,
        "room_width_m" => settings.room_width_m = value,
        "room_depth_m" => settings.room_depth_m = value,
        "room_height_m" => settings.room_height_m = value,
        "reverb_level" => settings.reverb_level = value,
        "reverb_rt60_s" => settings.reverb_rt60_s = value,
        "reverb_predelay_ms" => settings.reverb_predelay_ms = value,
        "tracking_smoothing" => settings.tracking_smoothing = value,
        "osc_rx_port" => settings.osc_rx_port = value.round().clamp(1.0, 65_535.0) as u16,
        _ => tracing::warn!(key, "ignored unknown Omniphony numeric setting"),
    }
}

fn change_omniphony_bool(settings: &mut StoredOmniphonyAudioSettings, key: &str, value: bool) {
    match key {
        "headphones" => settings.headphones = value,
        "reflections_enabled" => settings.reflections_enabled = value,
        "reverb_enabled" => settings.reverb_enabled = value,
        "air_absorption" => settings.air_absorption = value,
        "tracking_invert" => settings.tracking_invert = value,
        _ => tracing::warn!(key, "ignored unknown Omniphony boolean setting"),
    }
}

pub(crate) fn update_profile_settings(
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
    update: impl FnOnce(&mut ProfileSettings),
) {
    let Ok(model) = runtime.model() else {
        return;
    };
    let mut settings = model.ctx.profile.settings.clone();
    drop(model);
    update(&mut settings);
    runtime.dispatch(RuntimeAction {
        field: Some(AppModelField::Ctx),
        action: Action::Ctx(ActionCtx::UpdateSettings(settings)),
    });
}

struct InterfaceLanguage {
    name: &'static str,
    locale: &'static str,
    code: &'static str,
}

const INTERFACE_LANGUAGES: &[InterfaceLanguage] = &[
    InterfaceLanguage {
        name: "العربية",
        locale: "ar-AR",
        code: "ara",
    },
    InterfaceLanguage {
        name: "Беларуская",
        locale: "be-BY",
        code: "bel",
    },
    InterfaceLanguage {
        name: "български език",
        locale: "bg-BG",
        code: "bul",
    },
    InterfaceLanguage {
        name: "বাংলা",
        locale: "bn-BD",
        code: "ben",
    },
    InterfaceLanguage {
        name: "català",
        locale: "ca-ES",
        code: "cat",
    },
    InterfaceLanguage {
        name: "čeština",
        locale: "cs-CZ",
        code: "ces",
    },
    InterfaceLanguage {
        name: "dansk",
        locale: "da-DK",
        code: "dan",
    },
    InterfaceLanguage {
        name: "Deutsch",
        locale: "de-DE",
        code: "deu",
    },
    InterfaceLanguage {
        name: "ελληνικά",
        locale: "el-GR",
        code: "ell",
    },
    InterfaceLanguage {
        name: "English",
        locale: "en-US",
        code: "eng",
    },
    InterfaceLanguage {
        name: "Esperanto",
        locale: "eo-EO",
        code: "epo",
    },
    InterfaceLanguage {
        name: "español",
        locale: "es-ES",
        code: "spa",
    },
    InterfaceLanguage {
        name: "Eesti",
        locale: "et-EE",
        code: "est",
    },
    InterfaceLanguage {
        name: "euskara",
        locale: "eu-ES",
        code: "eus",
    },
    InterfaceLanguage {
        name: "فارسی",
        locale: "fa-IR",
        code: "fas",
    },
    InterfaceLanguage {
        name: "Suomi",
        locale: "fi-FI",
        code: "fin",
    },
    InterfaceLanguage {
        name: "Français",
        locale: "fr-FR",
        code: "fra",
    },
    InterfaceLanguage {
        name: "עברית",
        locale: "he-IL",
        code: "heb",
    },
    InterfaceLanguage {
        name: "हिन्दी",
        locale: "hi-IN",
        code: "hin",
    },
    InterfaceLanguage {
        name: "hrvatski jezik",
        locale: "hr-HR",
        code: "hrv",
    },
    InterfaceLanguage {
        name: "magyar",
        locale: "hu-HU",
        code: "hun",
    },
    InterfaceLanguage {
        name: "Bahasa Indonesia",
        locale: "id-ID",
        code: "ind",
    },
    InterfaceLanguage {
        name: "italiano",
        locale: "it-IT",
        code: "ita",
    },
    InterfaceLanguage {
        name: "日本語 (にほんご)",
        locale: "ja-JP",
        code: "jpn",
    },
    InterfaceLanguage {
        name: "한국어",
        locale: "ko-KR",
        code: "kor",
    },
    InterfaceLanguage {
        name: "Lietuvių",
        locale: "lt-LT",
        code: "lit",
    },
    InterfaceLanguage {
        name: "македонски јазик",
        locale: "mk-MK",
        code: "mkd",
    },
    InterfaceLanguage {
        name: "ဗမာစာ",
        locale: "my-BM",
        code: "mya",
    },
    InterfaceLanguage {
        name: "नेपाली",
        locale: "ne-NP",
        code: "nep",
    },
    InterfaceLanguage {
        name: "Norsk bokmål",
        locale: "nb-NO",
        code: "nob",
    },
    InterfaceLanguage {
        name: "Nederlands",
        locale: "nl-NL",
        code: "nld",
    },
    InterfaceLanguage {
        name: "Norsk nynorsk",
        locale: "nn-NO",
        code: "nno",
    },
    InterfaceLanguage {
        name: "ਪੰਜਾਬੀ",
        locale: "pa-IN",
        code: "pan",
    },
    InterfaceLanguage {
        name: "język polski",
        locale: "pl-PL",
        code: "pol",
    },
    InterfaceLanguage {
        name: "português Brazil",
        locale: "pt-BR",
        code: "por",
    },
    InterfaceLanguage {
        name: "português",
        locale: "pt-PT",
        code: "por",
    },
    InterfaceLanguage {
        name: "Română",
        locale: "ro-RO",
        code: "ron",
    },
    InterfaceLanguage {
        name: "русский язык",
        locale: "ru-RU",
        code: "rus",
    },
    InterfaceLanguage {
        name: "Slovenčina",
        locale: "sk-SK",
        code: "slk",
    },
    InterfaceLanguage {
        name: "slovenski jezik",
        locale: "sl-SL",
        code: "slv",
    },
    InterfaceLanguage {
        name: "српски језик",
        locale: "sr-RS",
        code: "srp",
    },
    InterfaceLanguage {
        name: "Svenska",
        locale: "sv-SE",
        code: "swe",
    },
    InterfaceLanguage {
        name: "தமிழ்",
        locale: "ta-IN",
        code: "tam",
    },
    InterfaceLanguage {
        name: "తెలుగు",
        locale: "te-IN",
        code: "tel",
    },
    InterfaceLanguage {
        name: "Türkçe",
        locale: "tr-TR",
        code: "tur",
    },
    InterfaceLanguage {
        name: "українська мова",
        locale: "uk-UA",
        code: "ukr",
    },
    InterfaceLanguage {
        name: "اُرْدُو",
        locale: "ur-PK",
        code: "urd",
    },
    InterfaceLanguage {
        name: "Tiếng Việt",
        locale: "vi-VN",
        code: "vie",
    },
    InterfaceLanguage {
        name: "中文(中华人民共和国)",
        locale: "zh-CN",
        code: "zho",
    },
    InterfaceLanguage {
        name: "中文(香港特别行政區)",
        locale: "zh-HK",
        code: "zho",
    },
    InterfaceLanguage {
        name: "中文(台灣)",
        locale: "zh-TW",
        code: "zho",
    },
];

fn legacy_language_code(value: &str) -> &str {
    match value {
        "ar" => "ara",
        "be" => "bel",
        "bg" => "bul",
        "bn" => "ben",
        "ca" => "cat",
        "cs" | "cze" => "ces",
        "da" => "dan",
        "de" | "ger" => "deu",
        "el" | "gre" => "ell",
        "en" => "eng",
        "eo" => "epo",
        "es" => "spa",
        "et" => "est",
        "eu" | "baq" => "eus",
        "fa" | "per" => "fas",
        "fi" => "fin",
        "fr" | "fre" => "fra",
        "he" => "heb",
        "hi" => "hin",
        "hr" | "scr" => "hrv",
        "hu" => "hun",
        "id" => "ind",
        "it" => "ita",
        "ja" => "jpn",
        "ko" => "kor",
        "lt" => "lit",
        "mk" | "mac" => "mkd",
        "my" | "bur" => "mya",
        "nb" => "nob",
        "ne" => "nep",
        "nl" | "dut" => "nld",
        "nn" => "nno",
        "pa" => "pan",
        "pl" => "pol",
        "pt" => "por",
        "ro" | "rum" => "ron",
        "ru" => "rus",
        "sk" | "slo" => "slk",
        "sl" => "slv",
        "sr" | "scc" => "srp",
        "sv" => "swe",
        "ta" => "tam",
        "te" => "tel",
        "tr" => "tur",
        "uk" => "ukr",
        "ur" => "urd",
        "vi" => "vie",
        "zh" | "chi" => "zho",
        _ => value,
    }
}

fn find_interface_language(value: &str) -> Option<&'static InterfaceLanguage> {
    let value = value.trim();
    INTERFACE_LANGUAGES
        .iter()
        .find(|language| {
            language.name == value
                || language.locale.eq_ignore_ascii_case(value)
                || language.code.eq_ignore_ascii_case(value)
        })
        .or_else(|| {
            let normalized = value.to_ascii_lowercase();
            let code = legacy_language_code(&normalized);
            INTERFACE_LANGUAGES
                .iter()
                .find(|language| language.code == code)
        })
}

fn interface_language_code(value: &str) -> String {
    find_interface_language(value)
        .map(|language| language.locale)
        .unwrap_or_else(|| value.trim())
        .to_owned()
}

fn language_code(value: &str) -> String {
    find_interface_language(value)
        .map(|language| language.code)
        .unwrap_or_else(|| value.trim())
        .to_owned()
}

fn language_display(value: &str) -> String {
    find_interface_language(value)
        .map(|language| language.name)
        .unwrap_or(value)
        .to_owned()
}

fn map_interface_language_to_locale(value: &str) -> &'static str {
    find_interface_language(value)
        .map(|language| language.locale)
        .unwrap_or("en-US")
}

fn interface_language_options() -> slint::ModelRc<slint::SharedString> {
    let preferred_locale = sys_locale::get_locale()
        .map(|locale| locale.replace('_', "-"))
        .and_then(|locale| {
            INTERFACE_LANGUAGES
                .iter()
                .find(|language| language.locale.eq_ignore_ascii_case(&locale))
                .or_else(|| {
                    let language_code = locale.get(..2)?;
                    INTERFACE_LANGUAGES.iter().find(|language| {
                        language
                            .locale
                            .get(..2)
                            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(language_code))
                    })
                })
        })
        .map(|language| language.locale)
        .unwrap_or("en-US");

    let mut languages = INTERFACE_LANGUAGES.iter().collect::<Vec<_>>();
    languages.sort_by_key(|language| language.name.to_lowercase());
    if let Some(index) = languages
        .iter()
        .position(|language| language.locale == preferred_locale)
    {
        let preferred = languages.remove(index);
        languages.insert(0, preferred);
    }

    let names = languages
        .into_iter()
        .map(|language| slint::SharedString::from(language.name))
        .collect::<Vec<_>>();
    slint::ModelRc::new(slint::VecModel::from(names))
}

fn format_cache_size(bytes: f64) -> String {
    if bytes <= 0.0 {
        "No Caching".to_string()
    } else if bytes >= 1024.0 * 1024.0 * 1024.0 * 1024.0 {
        "Infinite".to_string()
    } else {
        let gb = bytes / 1024.0 / 1024.0 / 1024.0;
        format!("{:.1} GB", gb)
    }
}

fn color_to_hex(color: slint::Color) -> String {
    format!(
        "#{:02X}{:02X}{:02X}{:02X}",
        color.red(),
        color.green(),
        color.blue(),
        color.alpha()
    )
}

fn update_streaming_server_settings(
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
    update: impl FnOnce(&mut StreamingServerSettings),
) {
    let Ok(model) = runtime.model() else {
        return;
    };
    let Loadable::Ready(mut settings) = model.streaming_server.settings.clone() else {
        return;
    };
    drop(model);
    update(&mut settings);
    runtime.dispatch(RuntimeAction {
        field: Some(AppModelField::StreamingServer),
        action: Action::StreamingServer(ActionStreamingServer::UpdateSettings(settings)),
    });
}

pub fn setup(
    ui: &MainWindow,
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
    config: &AppConfig,
    native_playback: Option<&NativePlaybackBridge>,
) {
    ui.set_settings_interface_language_options(interface_language_options());
    ui.set_settings_thumbnail_previews(config.thumbnail_previews_enabled);
    let omniphony_decoder_available =
        native_playback.is_some_and(NativePlaybackBridge::omniphony_decoder_available);
    ui.set_settings_omniphony_audio_available(omniphony_decoder_available);
    let default_omniphony_settings = StoredOmniphonyAudioSettings::default();
    let omniphony_settings = Arc::new(Mutex::new(default_omniphony_settings.clone()));
    ui.set_settings_omniphony_audio((&default_omniphony_settings).into());
    if let Some(playback) = native_playback {
        playback.configure_omniphony_audio(default_omniphony_settings.clone().into());
    }

    let initial_spatial_audio_mode = runtime
        .model()
        .ok()
        .filter(|model| model.ctx.profile.settings.surround_sound)
        .map_or(SpatialAudioMode::Disabled, |_| {
            SpatialAudioMode::SurroundMatrix
        });
    ui.set_settings_spatial_audio_mode(spatial_audio_mode_label(initial_spatial_audio_mode).into());
    ui.set_settings_spatial_audio_status("Checking spatial audio support…".into());
    if let Some(playback) = native_playback {
        playback.set_spatial_audio_mode(initial_spatial_audio_mode);
    }
    {
        let ui_weak = ui.as_weak();
        let native_playback = native_playback.cloned();
        let omniphony_settings = omniphony_settings.clone();
        tokio::spawn(async move {
            let stored = match crate::db::get_setting(SPATIAL_AUDIO_MODE_SETTING).await {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(%error, "could not load spatial audio preference");
                    None
                }
            };
            let mode = stored
                .as_deref()
                .and_then(spatial_audio_mode_from_value)
                .unwrap_or(initial_spatial_audio_mode);
            if stored.is_none()
                && let Err(error) =
                    crate::db::set_setting(SPATIAL_AUDIO_MODE_SETTING, spatial_audio_mode_id(mode))
                        .await
            {
                tracing::warn!(%error, "could not initialize spatial audio preference");
            }
            let stored_omniphony = match crate::db::get_setting(OMNIPHONY_AUDIO_SETTING).await {
                Ok(Some(value)) => {
                    match serde_json::from_str::<StoredOmniphonyAudioSettings>(&value) {
                        Ok(settings) => settings.sanitize(),
                        Err(error) => {
                            tracing::warn!(%error, "could not parse Omniphony audio settings");
                            StoredOmniphonyAudioSettings::default()
                        }
                    }
                }
                Ok(None) => StoredOmniphonyAudioSettings::default(),
                Err(error) => {
                    tracing::warn!(%error, "could not load Omniphony audio settings");
                    StoredOmniphonyAudioSettings::default()
                }
            };
            {
                let mut current = omniphony_settings
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *current = stored_omniphony.clone();
            }
            if let Some(playback) = native_playback.as_ref() {
                playback.configure_omniphony_audio(stored_omniphony.clone().into());
                playback.set_spatial_audio_mode(mode);
            }
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_settings_spatial_audio_mode(spatial_audio_mode_label(mode).into());
                    ui.set_settings_omniphony_audio((&stored_omniphony).into());
                }
            });
        });
    }

    let server_url = runtime
        .model()
        .ok()
        .map(|model| model.ctx.profile.settings.streaming_server_url.to_string())
        .unwrap_or_else(|| config.server_url.clone());
    let connector = Arc::new(AppServerConnector::new(server_url));

    // Fetch initial streaming server settings and coordinate with Turso DB
    let conn_init = connector.clone();
    let ui_weak = ui.as_weak();
    tokio::spawn(async move {
        let db_settings = crate::db::get_settings(&[
            "seeding_enabled",
            "bt_enable_dht",
            "bt_enable_pex",
            "bt_enable_lsd",
        ])
        .await
        .unwrap_or_default();
        let db_seeding = db_settings
            .get("seeding_enabled")
            .map(|value| value == "true");
        let db_dht = db_settings
            .get("bt_enable_dht")
            .map(|value| value == "true");
        let db_pex = db_settings
            .get("bt_enable_pex")
            .map(|value| value == "true");
        let db_lsd = db_settings
            .get("bt_enable_lsd")
            .map(|value| value == "true");

        if let Ok(snapshot) = conn_init.get_settings_snapshot().await {
            let mut settings = snapshot.settings;
            let server_version = snapshot.server_version;
            let mut dirty = false;
            let seeding_value = settings.seeding_enabled.to_string();
            let dht_value = settings.bt_enable_dht.to_string();
            let pex_value = settings.bt_enable_pex.to_string();
            let lsd_value = settings.bt_enable_lsd.to_string();
            let mut missing_settings = Vec::with_capacity(4);
            if let Some(seeding) = db_seeding {
                if settings.seeding_enabled != seeding {
                    settings.seeding_enabled = seeding;
                    dirty = true;
                }
            } else {
                missing_settings.push(("seeding_enabled", seeding_value.as_str()));
            }

            if let Some(dht) = db_dht {
                if settings.bt_enable_dht != dht {
                    settings.bt_enable_dht = dht;
                    dirty = true;
                }
            } else {
                missing_settings.push(("bt_enable_dht", dht_value.as_str()));
            }

            if let Some(pex) = db_pex {
                if settings.bt_enable_pex != pex {
                    settings.bt_enable_pex = pex;
                    dirty = true;
                }
            } else {
                missing_settings.push(("bt_enable_pex", pex_value.as_str()));
            }

            if let Some(lsd) = db_lsd {
                if settings.bt_enable_lsd != lsd {
                    settings.bt_enable_lsd = lsd;
                    dirty = true;
                }
            } else {
                missing_settings.push(("bt_enable_lsd", lsd_value.as_str()));
            }

            if !missing_settings.is_empty() {
                let _ = crate::db::set_settings(&missing_settings).await;
            }

            if dirty {
                let _ = conn_init.apply_settings(settings.clone()).await;
                let _ =
                    crate::db::insert_log("INFO", "Streaming settings synchronized from Turso DB.")
                        .await;
            }

            let cache_size_str = format_cache_size(settings.cache_size);
            let seeding = settings.seeding_enabled;
            let dht = settings.bt_enable_dht;
            let pex = settings.bt_enable_pex;
            let lsd = settings.bt_enable_lsd;
            let max_conn = settings.bt_max_connections;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_torrent_cache_size(cache_size_str.into());
                    ui.set_settings_streaming_seeding(seeding);
                    ui.set_settings_streaming_dht(dht);
                    ui.set_settings_streaming_pex(pex);
                    ui.set_settings_streaming_lsd(lsd);
                    ui.set_settings_server_version(server_version.into());

                    let profile_str = if max_conn >= 200 {
                        "Ultra Fast"
                    } else if max_conn >= 100 {
                        "Fast"
                    } else {
                        "Default"
                    };
                    ui.set_settings_torrent_profile(profile_str.into());
                }
            });
        }
    });

    // Cache size callback
    ui.on_apply_cache_settings({
        let conn = connector.clone();
        let ui_weak = ui.as_weak();
        move |val_gb| {
            let conn = conn.clone();
            let ui_weak = ui_weak.clone();
            tokio::spawn(async move {
                if let Ok(mut settings) = conn.get_settings().await {
                    let bytes = if val_gb >= 50.0 {
                        10.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0
                    } else if val_gb <= 0.0 {
                        0.0
                    } else {
                        (val_gb as f64) * 1024.0 * 1024.0 * 1024.0
                    };
                    settings.cache_size = bytes;
                    if (conn.apply_settings(settings).await).is_ok() {
                        let cache_size_str = format_cache_size(bytes);
                        let _ = crate::db::insert_log(
                            "INFO",
                            &format!("Cache size adjusted to: {}", cache_size_str),
                        )
                        .await;
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                ui.set_torrent_cache_size(cache_size_str.into());
                            }
                        });
                    }
                }
            });
        }
    });

    // Interface language callback
    ui.on_settings_change_interface_language({
        let runtime = runtime.clone();
        move |lang| {
            let rt = runtime.clone();
            let lang = interface_language_code(lang.as_str());
            let locale = map_interface_language_to_locale(&lang);
            if let Err(error) = slint::select_bundled_translation(locale) {
                tracing::error!(%error, %locale, "failed to select bundled translation on language change");
            }
            tokio::spawn(async move {
                let _ = crate::db::set_setting("interface_language", &lang).await;
                let _ = crate::db::insert_log(
                    "INFO",
                    &format!("Interface language changed to: {}", lang),
                )
                .await;
                let model = rt.model().expect("model read failed");
                let mut settings = model.ctx.profile.settings.clone();
                settings.interface_language = lang;
                drop(model);

                rt.dispatch(RuntimeAction {
                    field: None,
                    action: Action::Ctx(ActionCtx::UpdateSettings(settings)),
                });
            });
        }
    });

    // Subtitles language callback
    ui.on_settings_change_subtitles_language({
        let runtime = runtime.clone();
        move |lang| {
            let rt = runtime.clone();
            let lang = language_code(lang.as_str());
            tokio::spawn(async move {
                let _ = crate::db::set_setting("subtitles_language", &lang).await;
                let _ = crate::db::insert_log(
                    "INFO",
                    &format!("Subtitles language changed to: {}", lang),
                )
                .await;
                let model = rt.model().expect("model read failed");
                let mut settings = model.ctx.profile.settings.clone();
                settings.subtitles_language = Some(lang);
                drop(model);

                rt.dispatch(RuntimeAction {
                    field: None,
                    action: Action::Ctx(ActionCtx::UpdateSettings(settings)),
                });
            });
        }
    });

    // Torrent profile callback
    ui.on_settings_change_torrent_profile({
        let conn = connector.clone();
        move |profile| {
            let conn = conn.clone();
            let profile = profile.to_string();
            tokio::spawn(async move {
                if let Ok(mut settings) = conn.get_settings().await {
                    if profile == "default" {
                        settings.bt_max_connections = 35;
                    } else if profile == "fast" {
                        settings.bt_max_connections = 100;
                    } else if profile == "ultrafast" {
                        settings.bt_max_connections = 200;
                    }
                    let _ = crate::db::set_setting("torrent_profile", &profile).await;
                    let _ = crate::db::insert_log(
                        "INFO",
                        &format!("Torrent connections profile set to: {}", profile),
                    )
                    .await;
                    let _ = conn.apply_settings(settings).await;
                }
            });
        }
    });

    // Clear search history callback
    ui.on_settings_clear_search_history({
        let runtime = runtime.clone();
        move || {
            let rt = runtime.clone();
            tokio::spawn(async move {
                let _ = crate::db::insert_log("INFO", "Search history cleared.").await;
                rt.dispatch(RuntimeAction {
                    field: None,
                    action: Action::Ctx(ActionCtx::ClearSearchHistory),
                });
            });
        }
    });

    // Shutdown streaming server callback
    ui.on_shutdown_server(move || {
        tracing::info!("Closing the app and streaming server...");
        if let Err(error) = slint::quit_event_loop() {
            tracing::error!(%error, "failed to stop the UI event loop");
        }
    });

    // Hardware acceleration toggle callback
    ui.on_settings_change_hardware_acceleration({
        let config_cloned = config.clone();
        let runtime = runtime.clone();
        move |enabled| {
            let mut cfg = config_cloned.clone();
            cfg.hardware_acceleration = enabled;
            crate::config::save_config(&cfg);
            let rt = runtime.clone();
            tokio::spawn(async move {
                let _ = crate::db::set_setting("hardware_acceleration", &enabled.to_string()).await;
                let _ = crate::db::insert_log(
                    "INFO",
                    &format!("Hardware acceleration toggle: {}", enabled),
                )
                .await;
                if let Ok(model) = rt.model() {
                    let mut settings = model.ctx.profile.settings.clone();
                    settings.hardware_decoding = enabled;
                    drop(model);
                    rt.dispatch(RuntimeAction {
                        field: None,
                        action: Action::Ctx(ActionCtx::UpdateSettings(settings)),
                    });
                }
            });
            tracing::info!(
                "Hardware acceleration toggled to: {}. Restart required.",
                enabled
            );
        }
    });

    ui.on_settings_change_thumbnail_previews({
        let native_playback = native_playback.cloned();
        move |enabled| {
            let mut config = crate::config::load_config();
            config.thumbnail_previews_enabled = enabled;
            crate::config::save_config(&config);
            if let Some(playback) = native_playback.as_ref() {
                playback.set_thumbnail_previews_enabled(enabled);
            }
            tracing::info!(enabled, "timeline thumbnail preview preference changed");
        }
    });

    ui.on_settings_export_data({
        let runtime = runtime.clone();
        let ui_weak = ui.as_weak();
        move || {
            let authenticated = runtime
                .model()
                .ok()
                .is_some_and(|model| model.ctx.profile.auth.is_some());
            if !authenticated {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_settings_export_loading(false);
                    ui.set_settings_export_state(1);
                    ui.set_settings_export_detail("".into());
                }
                return;
            }
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_settings_export_loading(true);
                ui.set_settings_export_state(2);
                ui.set_settings_export_detail("".into());
            }
            runtime.dispatch(RuntimeAction {
                field: Some(AppModelField::DataExport),
                action: Action::Load(ActionLoad::DataExport),
            });
        }
    });

    ui.on_settings_change_binge_watching({
        let runtime = runtime.clone();
        move |value| update_profile_settings(&runtime, |settings| settings.binge_watching = value)
    });
    ui.on_settings_change_discord_rpc_enabled({
        let runtime = runtime.clone();
        move |value| {
            update_profile_settings(&runtime, |settings| settings.discord_rpc_enabled = value)
        }
    });
    ui.on_settings_change_tidb_api_key({
        let ui_weak = ui.as_weak();
        move |value| {
            if value.as_str() == "••••••••••••" {
                return;
            }
            let value = value.to_string();
            let ui_weak = ui_weak.clone();
            tokio::spawn(async move {
                if let Err(error) = crate::secure_settings::set_tidb_api_key(&value).await {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.set_error_message(error.to_string().into());
                        }
                    });
                }
            });
        }
    });
    ui.on_settings_change_tidb_show_intro({
        move |value| {
            let mut cfg = crate::config::load_config();
            cfg.tidb_show_intro = value;
            crate::config::save_config(&cfg);
        }
    });
    ui.on_settings_change_tidb_show_recap({
        move |value| {
            let mut cfg = crate::config::load_config();
            cfg.tidb_show_recap = value;
            crate::config::save_config(&cfg);
        }
    });
    ui.on_settings_change_tidb_show_credits({
        move |value| {
            let mut cfg = crate::config::load_config();
            cfg.tidb_show_credits = value;
            crate::config::save_config(&cfg);
        }
    });
    ui.on_settings_change_tidb_show_preview({
        move |value| {
            let mut cfg = crate::config::load_config();
            cfg.tidb_show_preview = value;
            crate::config::save_config(&cfg);
        }
    });
    ui.on_settings_change_hide_spoilers({
        let runtime = runtime.clone();
        move |value| update_profile_settings(&runtime, |settings| settings.hide_spoilers = value)
    });
    ui.on_settings_change_gamepad_support({
        let runtime = runtime.clone();
        move |value| update_profile_settings(&runtime, |settings| settings.gamepad_support = value)
    });
    ui.on_settings_change_play_in_background({
        let runtime = runtime.clone();
        move |value| {
            update_profile_settings(&runtime, |settings| settings.play_in_background = value)
        }
    });
    ui.on_settings_change_subtitles_auto_select({
        let runtime = runtime.clone();
        move |value| {
            update_profile_settings(&runtime, |settings| settings.subtitles_auto_select = value)
        }
    });
    ui.on_settings_change_subtitles_font({
        let runtime = runtime.clone();
        move |value| {
            let value = value.trim().to_owned();
            if !value.is_empty() {
                update_profile_settings(&runtime, |settings| settings.subtitles_font = value);
            }
        }
    });
    ui.on_settings_change_subtitles_size({
        let runtime = runtime.clone();
        move |value| {
            if let Ok(value) = value.trim().parse::<u8>() {
                let value = value.clamp(50, 200);
                update_profile_settings(&runtime, |settings| settings.subtitles_size = value);
            }
        }
    });
    ui.on_settings_change_subtitles_bold({
        let runtime = runtime.clone();
        move |value| update_profile_settings(&runtime, |settings| settings.subtitles_bold = value)
    });
    ui.on_settings_change_subtitles_offset({
        let runtime = runtime.clone();
        move |value| {
            if let Ok(value) = value.trim().parse::<u8>() {
                let value = value.min(100);
                update_profile_settings(&runtime, |settings| settings.subtitles_offset = value);
            }
        }
    });
    ui.on_settings_change_seek_duration({
        let runtime = runtime.clone();
        move |value| {
            if let Ok(seconds) = value.trim().parse::<u32>() {
                let milliseconds = seconds.clamp(1, 120).saturating_mul(1_000);
                update_profile_settings(&runtime, |settings| {
                    settings.seek_time_duration = milliseconds;
                });
            }
        }
    });
    ui.on_settings_change_seek_short_duration({
        let runtime = runtime.clone();
        move |value| {
            if let Ok(seconds) = value.trim().parse::<u32>() {
                let milliseconds = seconds.clamp(1, 120).saturating_mul(1_000);
                update_profile_settings(&runtime, |settings| {
                    settings.seek_short_time_duration = milliseconds;
                });
            }
        }
    });
    ui.on_settings_change_next_video_popup_duration({
        let runtime = runtime.clone();
        move |value| {
            if let Ok(seconds) = value.trim().parse::<u32>() {
                let milliseconds = seconds.min(300).saturating_mul(1_000);
                update_profile_settings(&runtime, |settings| {
                    settings.next_video_notification_duration = milliseconds;
                });
            }
        }
    });
    ui.on_settings_change_external_player({
        let runtime = runtime.clone();
        move |value| {
            let value = value.trim().to_owned();
            update_profile_settings(&runtime, |settings| {
                settings.player_type = (!value.is_empty()
                    && !value.eq_ignore_ascii_case("built-in player"))
                .then_some(value);
            });
        }
    });
    ui.on_settings_change_audio_language({
        let runtime = runtime.clone();
        move |value| {
            let value = value.trim().to_owned();
            update_profile_settings(&runtime, |settings| {
                settings.audio_language = (!value.is_empty()).then(|| language_code(&value));
            });
        }
    });
    ui.on_settings_change_spatial_audio_mode({
        let runtime = runtime.clone();
        let native_playback = native_playback.cloned();
        let ui_weak = ui.as_weak();
        move |value| {
            let Some(mode) = spatial_audio_mode_from_value(value.as_str()) else {
                return;
            };
            if mode == SpatialAudioMode::OmniphonySpatial && !omniphony_decoder_available {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_settings_spatial_audio_status(
                        "Omniphony 3D requires the patched libmpv runtime".into(),
                    );
                }
                return;
            }
            update_profile_settings(&runtime, |settings| {
                settings.surround_sound = mode == SpatialAudioMode::SurroundMatrix;
            });
            if let Some(playback) = native_playback.as_ref() {
                playback.set_spatial_audio_mode(mode);
            }
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_settings_spatial_audio_status("Applying spatial audio mode…".into());
            }
            tokio::spawn(async move {
                if let Err(error) =
                    crate::db::set_setting(SPATIAL_AUDIO_MODE_SETTING, spatial_audio_mode_id(mode))
                        .await
                {
                    tracing::warn!(%error, "could not persist spatial audio preference");
                }
            });
        }
    });
    ui.on_settings_change_omniphony_string({
        let state = omniphony_settings.clone();
        let native_playback = native_playback.cloned();
        let ui_weak = ui.as_weak();
        move |key, value| {
            let settings = mutate_omniphony_settings(&state, |settings| {
                change_omniphony_string(settings, key.as_str(), value.as_str());
            });
            let ui = ui_weak.upgrade();
            apply_omniphony_settings(settings, ui.as_ref(), native_playback.as_ref());
        }
    });
    ui.on_settings_change_omniphony_number({
        let state = omniphony_settings.clone();
        let native_playback = native_playback.cloned();
        let ui_weak = ui.as_weak();
        move |key, value| {
            let settings = mutate_omniphony_settings(&state, |settings| {
                change_omniphony_number(settings, key.as_str(), value);
            });
            let ui = ui_weak.upgrade();
            apply_omniphony_settings(settings, ui.as_ref(), native_playback.as_ref());
        }
    });
    ui.on_settings_change_omniphony_bool({
        let state = omniphony_settings.clone();
        let native_playback = native_playback.cloned();
        let ui_weak = ui.as_weak();
        move |key, value| {
            let settings = mutate_omniphony_settings(&state, |settings| {
                change_omniphony_bool(settings, key.as_str(), value);
            });
            let ui = ui_weak.upgrade();
            apply_omniphony_settings(settings, ui.as_ref(), native_playback.as_ref());
        }
    });
    ui.on_settings_recenter_omniphony_head({
        let native_playback = native_playback.cloned();
        move || {
            if let Some(playback) = native_playback.as_ref() {
                playback.recenter_omniphony_head();
            }
        }
    });
    ui.on_settings_change_ass_subtitles_styling({
        let runtime = runtime.clone();
        move |value| {
            update_profile_settings(&runtime, |settings| settings.ass_subtitles_styling = value)
        }
    });
    ui.on_settings_change_subtitles_text_color({
        let runtime = runtime.clone();
        move |value| {
            update_profile_settings(&runtime, |settings| {
                settings.subtitles_text_color = color_to_hex(value);
            });
        }
    });
    ui.on_settings_change_subtitles_background_color({
        let runtime = runtime.clone();
        move |value| {
            update_profile_settings(&runtime, |settings| {
                settings.subtitles_background_color = color_to_hex(value);
            });
        }
    });
    ui.on_settings_change_subtitles_outline_color({
        let runtime = runtime.clone();
        move |value| {
            update_profile_settings(&runtime, |settings| {
                settings.subtitles_outline_color = color_to_hex(value);
            });
        }
    });
    ui.on_settings_toggle_trakt({
        let runtime = runtime.clone();
        move || {
            let trakt_state = runtime.model().ok().and_then(|model| {
                let user_id = model.ctx.profile.auth.as_ref()?.user.id.0.clone();
                Some((model.ctx.profile.has_trakt::<DesktopEnv>(), user_id))
            });
            let Some((has_trakt, user_id)) = trakt_state else {
                return;
            };
            if has_trakt {
                runtime.dispatch(RuntimeAction {
                    field: Some(AppModelField::Ctx),
                    action: Action::Ctx(ActionCtx::LogoutTrakt),
                });
            } else if let Err(error) =
                open::that(format!("https://www.strem.io/trakt/auth/{user_id}"))
            {
                tracing::warn!(%error, "failed to open Trakt authentication");
            }
        }
    });
    ui.on_settings_add_streaming_server_url({
        let runtime = runtime.clone();
        move |value| {
            if let Ok(url) = url::Url::parse(value.trim()) {
                runtime.dispatch(RuntimeAction {
                    field: Some(AppModelField::Ctx),
                    action: Action::Ctx(ActionCtx::AddServerUrl(url)),
                });
            }
        }
    });
    ui.on_settings_delete_streaming_server_url({
        let runtime = runtime.clone();
        move |value| {
            if let Ok(url) = url::Url::parse(value.trim()) {
                runtime.dispatch(RuntimeAction {
                    field: Some(AppModelField::Ctx),
                    action: Action::Ctx(ActionCtx::DeleteServerUrl(url)),
                });
            }
        }
    });
    ui.on_settings_copy_remote_url(|value| {
        if let Err(error) = arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(value.to_string()))
        {
            tracing::warn!(%error, "failed to copy remote access URL");
        }
    });
    ui.on_settings_change_streaming_https_endpoint({
        let runtime = runtime.clone();
        move |value| {
            let value = value.trim().to_owned();
            update_streaming_server_settings(&runtime, |settings| {
                settings.remote_https =
                    (!value.is_empty() && !value.eq_ignore_ascii_case("disabled")).then_some(value);
            });
        }
    });
    ui.on_settings_change_streaming_transcode_profile({
        let runtime = runtime.clone();
        move |value| {
            let value = value.trim().to_owned();
            update_streaming_server_settings(&runtime, |settings| {
                settings.transcode_profile =
                    (!value.is_empty() && !value.eq_ignore_ascii_case("disabled")).then_some(value);
            });
        }
    });
    ui.on_settings_change_pause_on_minimize({
        let runtime = runtime.clone();
        move |value| {
            update_profile_settings(&runtime, |settings| settings.pause_on_minimize = value)
        }
    });
    ui.on_settings_change_quit_on_close({
        let runtime = runtime.clone();
        move |value| update_profile_settings(&runtime, |settings| settings.quit_on_close = value)
    });

    // Custom Client Settings Callbacks
    ui.on_settings_change_seeding_enabled({
        let conn = connector.clone();
        move |enabled| {
            let conn = conn.clone();
            tokio::spawn(async move {
                let _ = crate::db::set_setting("seeding_enabled", &enabled.to_string()).await;
                let _ = crate::db::insert_log(
                    "INFO",
                    &format!("Torrent seeding changed to: {}", enabled),
                )
                .await;
                if let Ok(mut settings) = conn.get_settings().await {
                    settings.seeding_enabled = enabled;
                    let _ = conn.apply_settings(settings).await;
                }
            });
        }
    });

    ui.on_settings_change_dht_enabled({
        let conn = connector.clone();
        move |enabled| {
            let conn = conn.clone();
            tokio::spawn(async move {
                let _ = crate::db::set_setting("bt_enable_dht", &enabled.to_string()).await;
                let _ =
                    crate::db::insert_log("INFO", &format!("DHT network changed to: {}", enabled))
                        .await;
                if let Ok(mut settings) = conn.get_settings().await {
                    settings.bt_enable_dht = enabled;
                    let _ = conn.apply_settings(settings).await;
                }
            });
        }
    });

    ui.on_settings_change_pex_enabled({
        let conn = connector.clone();
        move |enabled| {
            let conn = conn.clone();
            tokio::spawn(async move {
                let _ = crate::db::set_setting("bt_enable_pex", &enabled.to_string()).await;
                let _ =
                    crate::db::insert_log("INFO", &format!("PEX network changed to: {}", enabled))
                        .await;
                if let Ok(mut settings) = conn.get_settings().await {
                    settings.bt_enable_pex = enabled;
                    let _ = conn.apply_settings(settings).await;
                }
            });
        }
    });

    ui.on_settings_change_lsd_enabled({
        let conn = connector.clone();
        move |enabled| {
            let conn = conn.clone();
            tokio::spawn(async move {
                let _ = crate::db::set_setting("bt_enable_lsd", &enabled.to_string()).await;
                let _ =
                    crate::db::insert_log("INFO", &format!("LSD network changed to: {}", enabled))
                        .await;
                if let Ok(mut settings) = conn.get_settings().await {
                    settings.bt_enable_lsd = enabled;
                    let _ = conn.apply_settings(settings).await;
                }
            });
        }
    });

    // Diagnostics Logs Callbacks
    ui.on_settings_refresh_logs({
        let conn = connector.clone();
        let ui_weak = ui.as_weak();
        move || {
            let conn = conn.clone();
            let ui_weak = ui_weak.clone();
            tokio::spawn(async move {
                let mut logs_combined = String::new();

                // 1. Fetch Local Application Logs from Turso DB
                if let Ok(db_logs) = crate::db::get_logs(100).await {
                    logs_combined.push_str("--- Local Application Logs (Turso DB) ---\n");
                    for line in db_logs.iter().rev() {
                        logs_combined.push_str(line);
                        logs_combined.push('\n');
                    }
                    logs_combined.push('\n');
                }

                // 2. Fetch Streaming Server Engine Logs
                if let Ok(logs) = conn.get_logs().await {
                    let content = logs
                        .current_human_log
                        .unwrap_or_else(|| "No engine logs available.".to_string());
                    logs_combined.push_str("--- Streaming Server Engine Logs ---\n");
                    logs_combined.push_str(&content);
                } else {
                    logs_combined.push_str(
                        "--- Streaming Server Engine Logs ---\nFailed to retrieve engine logs.",
                    );
                }

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_settings_streaming_logs_text(logs_combined.into());
                    }
                });
            });
        }
    });

    ui.on_settings_open_logs_folder({
        let conn = connector.clone();
        move || {
            let conn = conn.clone();
            tokio::spawn(async move {
                if let Ok(logs) = conn.get_logs().await {
                    let path = logs.log_dir;
                    #[cfg(target_os = "windows")]
                    {
                        let _ = std::process::Command::new("explorer").arg(&path).spawn();
                    }
                    #[cfg(not(target_os = "windows"))]
                    {
                        let _ = std::process::Command::new("xdg-open").arg(&path).spawn();
                    }
                }
            });
        }
    });
}

#[tracing::instrument(skip_all)]
pub fn sync(
    ui: &MainWindow,
    settings: &ProfileSettings,
    user_id: &str,
    trakt_authenticated: bool,
    server_urls: &ServerUrlsBucket,
    streaming_settings: Option<&StreamingServerSettings>,
) {
    let _span = tracing::info_span!("apply_ui_settings").entered();
    let locale = map_interface_language_to_locale(&settings.interface_language);
    if let Err(error) = slint::select_bundled_translation(locale) {
        tracing::error!(%error, %locale, "failed to select bundled translation on sync");
    }
    ui.set_settings_interface_language(language_display(&settings.interface_language).into());
    ui.set_settings_subtitles_language(
        settings
            .subtitles_language
            .as_deref()
            .map(language_display)
            .unwrap_or_else(|| "English".to_string())
            .into(),
    );
    ui.set_settings_hardware_acceleration(settings.hardware_decoding);
    ui.set_settings_binge_watching(settings.binge_watching);
    ui.set_settings_discord_rpc_enabled(settings.discord_rpc_enabled);
    ui.set_settings_hide_spoilers(settings.hide_spoilers);
    ui.set_settings_gamepad_support(settings.gamepad_support);
    ui.set_settings_play_in_background(settings.play_in_background);
    ui.set_settings_subtitles_auto_select(settings.subtitles_auto_select);
    ui.set_settings_subtitles_font(settings.subtitles_font.as_str().into());
    ui.set_settings_subtitles_size(settings.subtitles_size.to_string().into());
    ui.set_settings_subtitles_bold(settings.subtitles_bold);
    ui.set_settings_subtitles_offset(settings.subtitles_offset.to_string().into());
    ui.set_settings_seek_duration((settings.seek_time_duration / 1_000).to_string().into());
    ui.set_settings_seek_short_duration(
        (settings.seek_short_time_duration / 1_000)
            .to_string()
            .into(),
    );
    ui.set_settings_next_video_popup_duration(
        (settings.next_video_notification_duration / 1_000)
            .to_string()
            .into(),
    );
    ui.set_settings_external_player(settings.player_type.as_deref().unwrap_or_default().into());
    ui.set_settings_audio_language(
        settings
            .audio_language
            .as_deref()
            .map(language_display)
            .unwrap_or_else(|| "English".to_owned())
            .into(),
    );
    ui.set_settings_ass_subtitles_styling(settings.ass_subtitles_styling);
    if let Some(color) = crate::config::parse_color(&settings.subtitles_text_color) {
        ui.set_settings_subtitles_text_color(color);
    }
    if let Some(color) = crate::config::parse_color(&settings.subtitles_background_color) {
        ui.set_settings_subtitles_background_color(color);
    }
    if let Some(color) = crate::config::parse_color(&settings.subtitles_outline_color) {
        ui.set_settings_subtitles_outline_color(color);
    }
    ui.set_settings_user_id(user_id.into());
    ui.set_settings_trakt_authenticated(trakt_authenticated);

    let mut urls = server_urls
        .items
        .keys()
        .map(url::Url::to_string)
        .collect::<Vec<_>>();
    urls.sort_unstable();
    ui.set_settings_streaming_server_urls(slint::ModelRc::new(slint::VecModel::from(
        urls.into_iter()
            .map(slint::SharedString::from)
            .collect::<Vec<_>>(),
    )));
    if let Some(streaming_settings) = streaming_settings {
        ui.set_settings_streaming_https_endpoint(
            streaming_settings
                .remote_https
                .as_deref()
                .unwrap_or("Disabled")
                .into(),
        );
        ui.set_settings_streaming_transcode_profile(
            streaming_settings
                .transcode_profile
                .as_deref()
                .unwrap_or("Disabled")
                .into(),
        );
    }
    ui.set_settings_pause_on_minimize(settings.pause_on_minimize);
    ui.set_settings_quit_on_close(settings.quit_on_close);

    // Apply the same persisted values to the native MPV controls. A
    // stream-specific override is restored when that stream finishes loading.
    ui.set_player_seek_step_seconds(settings.seek_time_duration as f32 / 1_000.0);
    ui.set_player_subtitle_size_percent(f32::from(settings.subtitles_size));
    ui.set_player_subtitle_offset_percent(f32::from(settings.subtitles_offset));
}

#[tracing::instrument(skip_all)]
pub fn sync_data_export(
    ui: &MainWindow,
    data_export: &DataExport,
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
) {
    let _span = tracing::info_span!("apply_data_export_state").entered();
    match data_export.export_url.as_ref().map(|(_, value)| value) {
        None => {
            ui.set_settings_export_loading(false);
        }
        Some(Loadable::Loading) => {
            ui.set_settings_export_loading(true);
            ui.set_settings_export_state(2);
            ui.set_settings_export_detail("".into());
        }
        Some(Loadable::Ready(url)) => {
            ui.set_settings_export_loading(false);
            match open::that(url.as_str()) {
                Ok(()) => {
                    ui.set_settings_export_state(3);
                    ui.set_settings_export_detail("".into());
                }
                Err(error) => {
                    tracing::error!(%error, %url, "failed to open data export");
                    ui.set_settings_export_state(4);
                    ui.set_settings_export_detail(url.as_str().into());
                }
            }
            runtime.dispatch(RuntimeAction {
                field: Some(AppModelField::DataExport),
                action: Action::Unload,
            });
        }
        Some(Loadable::Err(error)) => {
            ui.set_settings_export_loading(false);
            ui.set_settings_export_state(5);
            ui.set_settings_export_detail(format!("{error:?}").into());
        }
    }
}
