use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub const MAX_THEME_ASSET_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HomeLayoutPreset {
    SideRail,
    TopBar,
    Minimal,
    Classic,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ThemeManifestV1 {
    pub version: u32,
    pub name: String,
    pub layout: HomeLayoutPreset,
    pub colors: ThemeColors,
    pub density: f32,
    pub spacing_scale: f32,
    pub backdrop: Option<String>,
    pub font: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ThemeColors {
    pub background: String,
    pub surface: String,
    pub text: String,
    pub accent: String,
    pub focus: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlayerControl {
    PlayPause,
    Seek,
    Volume,
    RestoreExit,
    Subtitles,
    Audio,
    Episodes,
    Streams,
    PictureInPicture,
    Screenshot,
    SleepTimer,
    Fullscreen,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlayerLayoutV1 {
    pub version: u32,
    pub controls: Vec<PlayerControl>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CustomizationError {
    #[error("customization schema version is unsupported")]
    UnsupportedVersion,
    #[error("theme name is invalid")]
    InvalidName,
    #[error("theme color is invalid")]
    InvalidColor,
    #[error("theme density or spacing is outside the supported range")]
    InvalidScale,
    #[error("theme asset path is unsafe")]
    UnsafePath,
    #[error("theme asset type is unsupported")]
    UnsupportedAsset,
    #[error("theme asset is too large")]
    AssetTooLarge,
    #[error("mandatory player controls cannot be hidden")]
    MandatoryControlMissing,
    #[error("player controls contain duplicates")]
    DuplicateControl,
    #[error("customization file operation failed")]
    Filesystem,
}

pub fn import_theme_manifest(
    source: &Path,
    managed_theme_dir: &Path,
) -> Result<ThemeManifestV1, CustomizationError> {
    let contents = std::fs::read_to_string(source).map_err(|_| CustomizationError::Filesystem)?;
    let mut manifest: ThemeManifestV1 =
        serde_json::from_str(&contents).map_err(|_| CustomizationError::UnsupportedVersion)?;
    manifest.validate()?;
    let source_dir = source.parent().ok_or(CustomizationError::UnsafePath)?;
    if let Some(backdrop) = manifest.backdrop.clone() {
        let imported = import_asset(
            &source_dir.join(&backdrop),
            managed_theme_dir,
            &["png", "jpg", "jpeg", "webp"],
        )?;
        manifest.backdrop = imported
            .file_name()
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned);
    }
    if let Some(font) = manifest.font.clone() {
        let imported = import_asset(&source_dir.join(&font), managed_theme_dir, &["ttf", "otf"])?;
        manifest.font = imported
            .file_name()
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned);
    }
    manifest.validate()?;
    Ok(manifest)
}

pub fn export_theme_manifest(
    destination: &Path,
    manifest: &ThemeManifestV1,
) -> Result<(), CustomizationError> {
    manifest.validate()?;
    write_json(destination, manifest)
}

pub fn import_player_layout(source: &Path) -> Result<PlayerLayoutV1, CustomizationError> {
    let contents = std::fs::read_to_string(source).map_err(|_| CustomizationError::Filesystem)?;
    let layout: PlayerLayoutV1 =
        serde_json::from_str(&contents).map_err(|_| CustomizationError::UnsupportedVersion)?;
    layout.validate()?;
    Ok(layout)
}

pub fn export_player_layout(
    destination: &Path,
    layout: &PlayerLayoutV1,
) -> Result<(), CustomizationError> {
    layout.validate()?;
    write_json(destination, layout)
}

pub fn default_player_layout() -> PlayerLayoutV1 {
    PlayerLayoutV1 {
        version: 1,
        controls: vec![
            PlayerControl::PlayPause,
            PlayerControl::Seek,
            PlayerControl::Volume,
            PlayerControl::RestoreExit,
            PlayerControl::Subtitles,
            PlayerControl::Audio,
            PlayerControl::Episodes,
            PlayerControl::Streams,
            PlayerControl::PictureInPicture,
            PlayerControl::Screenshot,
            PlayerControl::SleepTimer,
            PlayerControl::Fullscreen,
        ],
    }
}

pub fn default_theme(layout: HomeLayoutPreset) -> ThemeManifestV1 {
    ThemeManifestV1 {
        version: 1,
        name: "Stremio Native".to_owned(),
        layout,
        colors: ThemeColors {
            background: "#0c0b11".to_owned(),
            surface: "#151320".to_owned(),
            text: "#ffffffe6".to_owned(),
            accent: "#7b5bf5".to_owned(),
            focus: "#ffffffe6".to_owned(),
        },
        density: 1.0,
        spacing_scale: 1.0,
        backdrop: None,
        font: None,
    }
}

fn write_json<T: Serialize>(destination: &Path, value: &T) -> Result<(), CustomizationError> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|_| CustomizationError::Filesystem)?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(|_| CustomizationError::Filesystem)?;
    let temporary = destination.with_extension("tmp");
    std::fs::write(&temporary, bytes).map_err(|_| CustomizationError::Filesystem)?;
    std::fs::rename(temporary, destination).map_err(|_| CustomizationError::Filesystem)
}

impl ThemeManifestV1 {
    pub fn validate(&self) -> Result<(), CustomizationError> {
        if self.version != 1 {
            return Err(CustomizationError::UnsupportedVersion);
        }
        if self.name.trim().is_empty() || self.name.chars().count() > 64 {
            return Err(CustomizationError::InvalidName);
        }
        for color in [
            &self.colors.background,
            &self.colors.surface,
            &self.colors.text,
            &self.colors.accent,
            &self.colors.focus,
        ] {
            if !valid_color(color) {
                return Err(CustomizationError::InvalidColor);
            }
        }
        if !(0.75..=1.5).contains(&self.density) || !(0.75..=1.5).contains(&self.spacing_scale) {
            return Err(CustomizationError::InvalidScale);
        }
        if let Some(path) = self.backdrop.as_deref() {
            validate_managed_asset_path(path, &["png", "jpg", "jpeg", "webp"])?;
        }
        if let Some(path) = self.font.as_deref() {
            validate_managed_asset_path(path, &["ttf", "otf"])?;
        }
        Ok(())
    }
}

impl PlayerLayoutV1 {
    pub fn validate(&self) -> Result<(), CustomizationError> {
        if self.version != 1 {
            return Err(CustomizationError::UnsupportedVersion);
        }
        let controls = self.controls.iter().copied().collect::<HashSet<_>>();
        if controls.len() != self.controls.len() {
            return Err(CustomizationError::DuplicateControl);
        }
        for mandatory in [
            PlayerControl::PlayPause,
            PlayerControl::Seek,
            PlayerControl::Volume,
            PlayerControl::RestoreExit,
        ] {
            if !controls.contains(&mandatory) {
                return Err(CustomizationError::MandatoryControlMissing);
            }
        }
        Ok(())
    }
}

pub fn import_asset(
    source: &Path,
    managed_theme_dir: &Path,
    supported_extensions: &[&str],
) -> Result<PathBuf, CustomizationError> {
    let metadata = source
        .metadata()
        .map_err(|_| CustomizationError::Filesystem)?;
    if !metadata.is_file() {
        return Err(CustomizationError::UnsupportedAsset);
    }
    if metadata.len() > MAX_THEME_ASSET_BYTES {
        return Err(CustomizationError::AssetTooLarge);
    }
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .ok_or(CustomizationError::UnsupportedAsset)?;
    if !supported_extensions
        .iter()
        .any(|supported| extension.eq_ignore_ascii_case(supported))
    {
        return Err(CustomizationError::UnsupportedAsset);
    }
    std::fs::create_dir_all(managed_theme_dir).map_err(|_| CustomizationError::Filesystem)?;
    let filename = source.file_name().ok_or(CustomizationError::UnsafePath)?;
    let destination = managed_theme_dir.join(filename);
    if destination
        .canonicalize()
        .ok()
        .is_some_and(|path| !path.starts_with(managed_theme_dir))
    {
        return Err(CustomizationError::UnsafePath);
    }
    std::fs::copy(source, &destination).map_err(|_| CustomizationError::Filesystem)?;
    Ok(destination)
}

fn valid_color(value: &str) -> bool {
    let value = value.strip_prefix('#').unwrap_or_default();
    matches!(value.len(), 6 | 8) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_managed_asset_path(value: &str, supported: &[&str]) -> Result<(), CustomizationError> {
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(CustomizationError::UnsafePath);
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .ok_or(CustomizationError::UnsupportedAsset)?;
    if !supported
        .iter()
        .any(|supported| extension.eq_ignore_ascii_case(supported))
    {
        return Err(CustomizationError::UnsupportedAsset);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal_and_executable_assets() {
        assert_eq!(
            validate_managed_asset_path("../theme.png", &["png"]),
            Err(CustomizationError::UnsafePath)
        );
        assert_eq!(
            validate_managed_asset_path("theme.exe", &["png"]),
            Err(CustomizationError::UnsupportedAsset)
        );
    }

    #[test]
    fn mandatory_player_controls_cannot_be_removed() {
        let layout = PlayerLayoutV1 {
            version: 1,
            controls: vec![PlayerControl::PlayPause],
        };
        assert_eq!(
            layout.validate(),
            Err(CustomizationError::MandatoryControlMissing)
        );
    }
}
