use std::{path::PathBuf, sync::OnceLock, time::Duration};

#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProfileMode {
    #[default]
    Off,
    Ui,
    Io,
    Playback,
    Full,
}

impl ProfileMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "ui" => Some(Self::Ui),
            "io" => Some(Self::Io),
            "playback" => Some(Self::Playback),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    pub fn enabled(self) -> bool {
        self != Self::Off
    }

    #[cfg(any(debug_assertions, feature = "profiling"))]
    pub fn includes_target(self, target: &str) -> bool {
        match self {
            Self::Off => false,
            Self::Full => true,
            Self::Ui => target == "stremio_native" || target.starts_with("stremio_native::"),
            Self::Io => {
                target == "stremio_native"
                    || target.starts_with("stremio_native::image_cache")
                    || target.starts_with("stremio_native::db")
                    || target.starts_with("core_env")
            }
            Self::Playback => {
                target.starts_with("stremio_native::mpv_integration")
                    || target.starts_with("playback_mpv")
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProfileConfig {
    pub mode: ProfileMode,
    pub output: Option<PathBuf>,
}

impl ProfileConfig {
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Self {
        let mut config = Self::default();
        for argument in args.into_iter().skip(1) {
            if argument == "--profile" {
                config.mode = ProfileMode::Full;
            } else if let Some(value) = argument.strip_prefix("--profile=") {
                if let Some(mode) = ProfileMode::parse(value) {
                    config.mode = mode;
                } else {
                    tracing::warn!(%value, "ignoring unknown profiling mode");
                }
            } else if let Some(value) = argument.strip_prefix("--profile-output=")
                && !value.is_empty()
            {
                config.output = Some(PathBuf::from(value));
            }
        }
        config
    }
}

#[derive(Default)]
pub struct PerformanceCounters {
    #[cfg(debug_assertions)]
    ui_patches: AtomicU64,
    #[cfg(debug_assertions)]
    ui_patch_micros: AtomicU64,
    #[cfg(debug_assertions)]
    image_memory_hits: AtomicU64,
    #[cfg(debug_assertions)]
    image_disk_hits: AtomicU64,
    #[cfg(debug_assertions)]
    image_downloads: AtomicU64,
    #[cfg(debug_assertions)]
    image_failures: AtomicU64,
    #[cfg(debug_assertions)]
    mpv_redraw_posts: AtomicU64,
    #[cfg(debug_assertions)]
    mpv_frames_published: AtomicU64,
}

#[cfg(debug_assertions)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PerformanceSnapshot {
    pub ui_patches: u64,
    pub ui_patch_micros: u64,
    pub image_memory_hits: u64,
    pub image_disk_hits: u64,
    pub image_downloads: u64,
    pub image_failures: u64,
    pub mpv_redraw_posts: u64,
    pub mpv_frames_published: u64,
}

impl PerformanceCounters {
    pub fn record_ui_patch(&self, _elapsed: Duration) {
        #[cfg(debug_assertions)]
        {
            self.ui_patches.fetch_add(1, Ordering::Relaxed);
            self.ui_patch_micros.fetch_add(
                _elapsed.as_micros().try_into().unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
    }

    pub fn record_image_memory_hit(&self) {
        #[cfg(debug_assertions)]
        self.image_memory_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_image_disk_hit(&self) {
        #[cfg(debug_assertions)]
        self.image_disk_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_image_download(&self) {
        #[cfg(debug_assertions)]
        self.image_downloads.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_image_failure(&self) {
        #[cfg(debug_assertions)]
        self.image_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_mpv_redraw_post(&self) {
        #[cfg(debug_assertions)]
        self.mpv_redraw_posts.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_mpv_frame_published(&self) {
        #[cfg(debug_assertions)]
        self.mpv_frames_published.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(debug_assertions)]
    pub fn snapshot(&self) -> PerformanceSnapshot {
        PerformanceSnapshot {
            ui_patches: self.ui_patches.load(Ordering::Relaxed),
            ui_patch_micros: self.ui_patch_micros.load(Ordering::Relaxed),
            image_memory_hits: self.image_memory_hits.load(Ordering::Relaxed),
            image_disk_hits: self.image_disk_hits.load(Ordering::Relaxed),
            image_downloads: self.image_downloads.load(Ordering::Relaxed),
            image_failures: self.image_failures.load(Ordering::Relaxed),
            mpv_redraw_posts: self.mpv_redraw_posts.load(Ordering::Relaxed),
            mpv_frames_published: self.mpv_frames_published.load(Ordering::Relaxed),
        }
    }
}

pub fn counters() -> &'static PerformanceCounters {
    static COUNTERS: OnceLock<PerformanceCounters> = OnceLock::new();
    COUNTERS.get_or_init(PerformanceCounters::default)
}

pub fn spawn_reporter() -> Option<tokio::task::JoinHandle<()>> {
    #[cfg(not(debug_assertions))]
    {
        None
    }
    #[cfg(debug_assertions)]
    {
        Some(tokio::spawn(async {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await;
            loop {
                interval.tick().await;
                let snapshot = counters().snapshot();
                tracing::info!(
                    target: "stremio_native::performance",
                    ui_patches = snapshot.ui_patches,
                    ui_patch_ms = snapshot.ui_patch_micros / 1_000,
                    image_memory_hits = snapshot.image_memory_hits,
                    image_disk_hits = snapshot.image_disk_hits,
                    image_downloads = snapshot.image_downloads,
                    image_failures = snapshot.image_failures,
                    mpv_redraw_posts = snapshot.mpv_redraw_posts,
                    mpv_frames_published = snapshot.mpv_frames_published,
                    "performance counters"
                );
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{ProfileConfig, ProfileMode};

    #[test]
    fn legacy_profile_flag_enables_full_profile() {
        let config =
            ProfileConfig::from_args(["stremio-native".to_owned(), "--profile".to_owned()]);
        assert_eq!(config.mode, ProfileMode::Full);
    }

    #[test]
    fn profile_output_is_parsed_without_enabling_an_unrequested_mode() {
        let config = ProfileConfig::from_args([
            "stremio-native".to_owned(),
            "--profile-output=trace.json".to_owned(),
        ]);
        assert_eq!(config.mode, ProfileMode::Off);
    }
}
