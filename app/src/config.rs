use serde::{Deserialize, Serialize};
use std::sync::{OnceLock, RwLock};

static APP_CONFIG: OnceLock<RwLock<AppConfig>> = OnceLock::new();

/// How long the pointer must rest on a card before its trailer opens.
///
/// Shared with the migration below so the shipped default and the value an
/// existing profile is moved to cannot drift apart.
pub const DEFAULT_HOVER_PREVIEW_DELAY_MS: u32 = 1200;

/// The threshold this shipped with originally. It fires while the pointer is
/// still crossing a grid, so profiles still holding it are moved forward.
const LEGACY_HOVER_PREVIEW_DELAY_MS: u32 = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub config_version: u32,
    pub server_url: String,
    pub active_tab: i32,
    pub auto_launch_player: bool,
    pub torrent_port: u16,
    pub hardware_acceleration: bool,
    pub theme: ThemeConfig,
    pub tidb_api_key: String,
    pub tidb_show_intro: bool,
    pub tidb_show_recap: bool,
    pub tidb_show_credits: bool,
    pub tidb_show_preview: bool,
    pub shaders_enabled: bool,
    pub active_shader_preset: u8,
    #[serde(alias = "thumbfast_enabled")]
    pub thumbnail_previews_enabled: bool,
    pub hover_trailer_previews_enabled: bool,
    /// How long the pointer must rest on a card before its trailer opens.
    pub hover_trailer_preview_delay_ms: u32,
    pub onboarding_completed: bool,
    pub download_bandwidth_limit_bps: u64,
    pub region: String,
    pub home_row_order: Vec<String>,
}

fn default_font_family() -> String {
    "Plus Jakarta Sans".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    pub active_preset_idx: i32,
    #[serde(default = "default_font_family")]
    pub font_family: String,
    pub background: String,
    pub secondary_background: String,
    pub sidebar_background: String,
    pub modal_background: String,
    pub drawer_background: String,
    pub control_background: String,
    pub overlay: String,
    pub overlay_hover: String,
    pub overlay_pressed: String,
    pub divider: String,
    pub scrim: String,
    pub accent: String,
    pub accent_hover: String,
    pub success: String,
    pub warning: String,
    pub info: String,
    pub danger: String,
    pub focus: String,
    pub title_bar: String,
    pub card_background: String,
    pub card_border: String,
    pub text_primary: String,
    pub text_secondary: String,
    pub text_muted: String,
    pub skeleton_base: String,
    pub skeleton_shimmer: String,
    /// Palette the colour picker's "Saved" row offers, newest last.
    pub saved_colors: Vec<String>,
    pub saved_gradients: Vec<SavedGradientConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedGradientConfig {
    pub angle: f32,
    pub start: String,
    pub end: String,
}

fn saved_gradient(angle: f32, start: &str, end: &str) -> SavedGradientConfig {
    SavedGradientConfig {
        angle,
        start: start.to_string(),
        end: end.to_string(),
    }
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            active_preset_idx: 0,
            font_family: "Plus Jakarta Sans".to_string(),
            background: "#0c0b11".to_string(),
            secondary_background: "#1a173e".to_string(),
            sidebar_background: "#0f0d2099".to_string(),
            modal_background: "#0f0d20".to_string(),
            drawer_background: "#00000066".to_string(),
            control_background: "#ffffff0d".to_string(),
            overlay: "#ffffff0d".to_string(),
            overlay_hover: "#ffffff14".to_string(),
            overlay_pressed: "#ffffff20".to_string(),
            divider: "#ffffff14".to_string(),
            scrim: "#00000066".to_string(),
            accent: "#7b5bf5".to_string(),
            accent_hover: "#9275f7".to_string(),
            success: "#22b365".to_string(),
            warning: "#f6c700".to_string(),
            info: "#1245a6".to_string(),
            danger: "#dc2626".to_string(),
            focus: "#ffffffe6".to_string(),
            title_bar: "#15122b".to_string(),
            card_background: "#151320".to_string(),
            card_border: "#201e2f".to_string(),
            text_primary: "#ffffffe6".to_string(),
            text_secondary: "#ffffff99".to_string(),
            text_muted: "#ffffff66".to_string(),
            skeleton_base: "#1a1828".to_string(),
            skeleton_shimmer: "#2a2838".to_string(),
            saved_colors: [
                "#10b981", "#3b82f6", "#6366f1", "#8b5cf6", "#d946ef", "#f43f5e", "#ef4444",
                "#f97316", "#7b5bf5",
            ]
            .iter()
            .map(|hex| hex.to_string())
            .collect(),
            saved_gradients: vec![
                saved_gradient(135.0, "#7b5bf5", "#1a173e"),
                saved_gradient(135.0, "#3b82f6", "#1c2541"),
                saved_gradient(135.0, "#10b981", "#047857"),
                saved_gradient(135.0, "#f43f5e", "#881337"),
                saved_gradient(135.0, "#a855f7", "#121212"),
                saved_gradient(135.0, "#f97316", "#431407"),
            ],
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            config_version: 3,
            server_url: "http://127.0.0.1:11470".to_string(),
            active_tab: 0,
            auto_launch_player: true,
            torrent_port: 11470,
            hardware_acceleration: true,
            theme: ThemeConfig::default(),
            tidb_api_key: String::new(),
            tidb_show_intro: true,
            tidb_show_recap: true,
            tidb_show_credits: true,
            tidb_show_preview: true,
            shaders_enabled: true,
            active_shader_preset: 0,
            thumbnail_previews_enabled: true,
            hover_trailer_previews_enabled: true,
            hover_trailer_preview_delay_ms: DEFAULT_HOVER_PREVIEW_DELAY_MS,
            onboarding_completed: false,
            download_bandwidth_limit_bps: 0,
            region: "US".to_owned(),
            home_row_order: Vec::new(),
        }
    }
}

impl AppConfig {
    fn migrate(&mut self) -> bool {
        let original_version = self.config_version;

        if self.config_version < 1 {
            // Version zero was generated with a palette that predates the
            // official stremio-web tokens. Only replace it when all legacy
            // values still match, preserving genuinely customized themes.
            let legacy_theme = self.theme.background.eq_ignore_ascii_case("#08070d")
                && self
                    .theme
                    .sidebar_background
                    .eq_ignore_ascii_case("#13111f")
                && self.theme.accent.eq_ignore_ascii_case("#7b5bf5")
                && self.theme.card_background.eq_ignore_ascii_case("#1a1829")
                && self.theme.card_border.eq_ignore_ascii_case("#2c2842")
                && self.theme.text_primary.eq_ignore_ascii_case("#ffffff")
                && self.theme.text_secondary.eq_ignore_ascii_case("#8d8a9f");

            if legacy_theme {
                self.theme = ThemeConfig::default();
            }
            self.config_version = 1;
        }
        if self.config_version < 2 {
            self.config_version = 2;
        }
        if self.config_version < 3 {
            // The delay is persisted, so raising the shipped default alone
            // leaves every existing profile on the old threshold. Only the
            // value this used to ship with is moved, which keeps a delay the
            // user picked deliberately.
            if self.hover_trailer_preview_delay_ms == LEGACY_HOVER_PREVIEW_DELAY_MS {
                self.hover_trailer_preview_delay_ms = DEFAULT_HOVER_PREVIEW_DELAY_MS;
            }
            self.config_version = 3;
        }
        self.config_version != original_version
    }
}

pub async fn init_config() {
    let mut config = AppConfig::default();

    // 1. Try to load from database settings table
    let mut loaded_from_db = false;
    if let Ok(conn) = crate::db::get_conn().await
        && let Ok(mut rows) = conn
            .query("SELECT value FROM settings WHERE key = 'app_config'", ())
            .await
        && let Ok(Some(row)) = rows.next().await
        && let Ok(val_str) = row.get::<String>(0)
        && let Ok(parsed) = serde_json::from_str::<AppConfig>(&val_str)
    {
        config = parsed;
        loaded_from_db = true;
    }

    // A clean install starts from database-backed defaults. Legacy working-
    // directory files are intentionally left untouched for manual recovery.
    if !loaded_from_db
        && let Ok(conn) = crate::db::get_conn().await
        && let Ok(serialized) = serde_json::to_string(&config)
    {
        let _ = conn
            .execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES ('app_config', ?)",
                [serialized],
            )
            .await;
    }

    let migrated = config.migrate();
    if migrated
        && loaded_from_db
        && let Ok(conn) = crate::db::get_conn().await
        && let Ok(serialized) = serde_json::to_string(&config)
    {
        let _ = conn
            .execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES ('app_config', ?)",
                [serialized],
            )
            .await;
    }

    let _ = APP_CONFIG.set(RwLock::new(config));
}

pub fn load_config() -> AppConfig {
    if let Some(lock) = APP_CONFIG.get()
        && let Ok(guard) = lock.read()
    {
        return guard.clone();
    }
    AppConfig::default()
}

pub fn with_config<R>(read: impl FnOnce(&AppConfig) -> R) -> R {
    if let Some(lock) = APP_CONFIG.get()
        && let Ok(config) = lock.read()
    {
        return read(&config);
    }
    read(&AppConfig::default())
}

pub fn save_config(config: &AppConfig) {
    if let Some(lock) = APP_CONFIG.get()
        && let Ok(mut guard) = lock.write()
    {
        *guard = config.clone();
    }

    let config_cloned = config.clone();
    tokio::spawn(async move {
        if let Ok(conn) = crate::db::get_conn().await
            && let Ok(serialized) = serde_json::to_string(&config_cloned)
        {
            let _ = conn
                .execute(
                    "INSERT OR REPLACE INTO settings (key, value) VALUES ('app_config', ?)",
                    [serialized],
                )
                .await;
        }
    });
}

pub async fn save_config_async(config: &AppConfig) -> anyhow::Result<()> {
    if let Some(lock) = APP_CONFIG.get()
        && let Ok(mut guard) = lock.write()
    {
        *guard = config.clone();
    }
    let serialized = serde_json::to_string(config)?;
    let conn = crate::db::get_conn().await?;
    conn.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES ('app_config', ?)",
        [serialized],
    )
    .await?;
    Ok(())
}

pub fn parse_color(hex: &str) -> Option<slint::Color> {
    let hex = hex.trim_start_matches('#');
    if hex.len() == 6 {
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        Some(slint::Color::from_rgb_u8(r, g, b))
    } else if hex.len() == 8 {
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
        Some(slint::Color::from_argb_u8(a, r, g, b))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_config() -> AppConfig {
        let mut config = AppConfig {
            config_version: 0,
            ..Default::default()
        };
        config.theme.background = "#08070d".to_string();
        config.theme.sidebar_background = "#13111f".to_string();
        config.theme.accent = "#7B5BF5".to_string();
        config.theme.card_background = "#1a1829".to_string();
        config.theme.card_border = "#2c2842".to_string();
        config.theme.text_primary = "#ffffff".to_string();
        config.theme.text_secondary = "#8d8a9f".to_string();
        config
    }

    #[test]
    fn migrates_generated_legacy_theme_to_official_defaults() {
        let mut config = legacy_config();

        assert!(config.migrate());
        assert_eq!(config.config_version, 3);
        assert_eq!(config.theme.background, "#0c0b11");
        assert_eq!(config.theme.secondary_background, "#1a173e");
        assert_eq!(config.theme.success, "#22b365");
    }

    #[test]
    fn preserves_custom_theme_during_version_migration() {
        let mut config = legacy_config();
        config.theme.accent = "#ff3366".to_string();

        assert!(config.migrate());
        assert_eq!(config.config_version, 3);
        assert_eq!(config.theme.background, "#08070d");
        assert_eq!(config.theme.accent, "#ff3366");
    }

    #[test]
    fn migration_moves_the_shipped_hover_delay_forward() {
        // A stored value shadows the default, so without this the threshold
        // stays at 500ms however the default is edited.
        let mut config = AppConfig {
            config_version: 2,
            hover_trailer_preview_delay_ms: LEGACY_HOVER_PREVIEW_DELAY_MS,
            ..Default::default()
        };

        assert!(config.migrate());
        assert_eq!(config.config_version, 3);
        assert_eq!(
            config.hover_trailer_preview_delay_ms,
            DEFAULT_HOVER_PREVIEW_DELAY_MS
        );
    }

    #[test]
    fn migration_keeps_a_deliberately_chosen_hover_delay() {
        let mut config = AppConfig {
            config_version: 2,
            hover_trailer_preview_delay_ms: 250,
            ..Default::default()
        };

        assert!(config.migrate());
        assert_eq!(config.hover_trailer_preview_delay_ms, 250);
    }

    #[test]
    fn fills_new_semantic_theme_slots_when_reading_old_json() {
        let config: AppConfig = serde_json::from_str(
            r##"{
                "server_url": "http://127.0.0.1:11470",
                "theme": {
                    "background": "#111111",
                    "accent": "#abcdef"
                }
            }"##,
        )
        .expect("old config should deserialize with semantic defaults");

        assert_eq!(config.theme.background, "#111111");
        assert_eq!(config.theme.accent, "#abcdef");
        assert_eq!(config.theme.drawer_background, "#00000066");
        assert_eq!(config.theme.text_muted, "#ffffff66");
    }

    #[test]
    fn old_thumbfast_setting_deserializes_under_the_new_name() {
        let config: AppConfig = serde_json::from_str(r#"{"thumbfast_enabled":false}"#)
            .expect("legacy thumbnail setting should deserialize");
        assert!(!config.thumbnail_previews_enabled);
    }

    #[test]
    fn thumbnail_setting_serializes_only_the_new_name() {
        let serialized = serde_json::to_string(&AppConfig::default()).expect("serialize config");
        assert!(serialized.contains("thumbnail_previews_enabled"));
        assert!(!serialized.contains("thumbfast_enabled"));
    }
}
