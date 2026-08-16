use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow};
use playback_mpv::{
    EndReason, HdrMode, OmniphonyAudioSettings, PlaybackCommand, PlaybackController, PlaybackEvent,
    PlaybackRuntime, PlaybackState, PlayerConfig, PreviewConfig, PreviewRuntime, RenderContext,
    RenderOutcome, RenderSource, SpatialAudioMode, ThumbnailConfig, ThumbnailRuntime,
    ThumbnailSource,
};
use slint::{
    BorrowedOpenGLTextureBuilder, BorrowedOpenGLTextureOrigin, ComponentHandle, ModelRc,
    SharedString, VecModel, winit_030::WinitWindowAccessor,
};
use stremio_core::{
    models::{
        common::Loadable,
        player::{Player, Selected, VideoParams},
    },
    runtime::{
        Runtime, RuntimeAction,
        msg::{Action, ActionLoad, ActionPlayer, ActionStreamingServer, PlayOnDeviceArgs},
    },
    types::{
        addon::ResourcePath,
        resource::{StreamSource, Subtitles},
        streaming_server::StatisticsRequest,
        streams::{AudioTrack, StreamItemState, StreamsBucket, StreamsItemKey, SubtitleTrack},
    },
};
use tokio_util::sync::CancellationToken;

use crate::{AppModel, AppModelField, MainWindow, NavigationController, NavigationIntent};
use crate::{
    EpisodeItem,
    models::{Fingerprint, SyncFingerprint},
};
use core_env::DesktopEnv;

const PLAYER_DEVICE: &str = "libmpv";

type SharedPlaybackState = Arc<RwLock<Arc<PlaybackState>>>;
type SharedShaderCoordinator = Arc<Mutex<crate::shaders::ShaderCoordinator>>;

fn dispatch_shader_update(
    controller: &PlaybackController,
    ui: &slint::Weak<MainWindow>,
    update: crate::shaders::ShaderUpdate,
) {
    if let Some(command) = update.command {
        tracing::info!(
            request_id = command.request_id,
            shader_count = command.paths.len(),
            "configuring MPV video shaders"
        );
        log_command(controller.send(PlaybackCommand::ConfigureVideoShaders {
            request_id: command.request_id,
            paths: command.paths,
        }));
    }

    let projection = update.projection;
    let ui = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui.upgrade() {
            ui.set_player_active_shader_preset(projection.active_preset.index() as i32);
            ui.set_player_shader_preset_available(ModelRc::new(VecModel::from(
                projection.availability.to_vec(),
            )));
            ui.set_player_shader_status(SharedString::from(projection.status));
        }
    });
}

fn lock_shader_coordinator(
    coordinator: &SharedShaderCoordinator,
) -> std::sync::MutexGuard<'_, crate::shaders::ShaderCoordinator> {
    coordinator
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Default)]
struct PlaybackEventInbox {
    queue: Mutex<VecDeque<PlaybackEvent>>,
    notify: tokio::sync::Notify,
    closed: AtomicBool,
}

#[cfg(test)]
mod tests {
    use super::{
        ExternalSubtitle, PlaybackEvent, PlaybackEventInbox, PlaybackState, SessionState,
        UNKNOWN_SUBTITLE_ORIGIN, addon_origin, subtitle_track_label,
    };
    use std::sync::Arc;

    fn state_at(time: f64) -> PlaybackEvent {
        PlaybackEvent::State(Arc::new(PlaybackState {
            time,
            ..PlaybackState::default()
        }))
    }

    #[tokio::test]
    async fn inbox_coalesces_adjacent_states_without_reordering_control_events() {
        let inbox = PlaybackEventInbox::default();
        inbox.push(state_at(1.0));
        inbox.push(state_at(2.0));
        inbox.push(PlaybackEvent::FileLoaded);
        inbox.push(state_at(3.0));

        match inbox.recv().await {
            Some(PlaybackEvent::State(state)) => assert_eq!(state.time, 2.0),
            event => panic!("expected latest coalesced state, got {event:?}"),
        }
        assert!(matches!(
            inbox.recv().await,
            Some(PlaybackEvent::FileLoaded)
        ));
        match inbox.recv().await {
            Some(PlaybackEvent::State(state)) => assert_eq!(state.time, 3.0),
            event => panic!("expected state after control event, got {event:?}"),
        }
    }

    #[tokio::test]
    async fn shutdown_drains_then_closes_the_inbox() {
        let inbox = PlaybackEventInbox::default();
        inbox.push(PlaybackEvent::Warning("before shutdown".to_owned()));
        inbox.push(PlaybackEvent::Shutdown);

        assert!(matches!(
            inbox.recv().await,
            Some(PlaybackEvent::Warning(_))
        ));
        assert!(matches!(inbox.recv().await, Some(PlaybackEvent::Shutdown)));
        assert!(inbox.recv().await.is_none());
    }

    fn external_subtitle(url: &str) -> ExternalSubtitle {
        ExternalSubtitle {
            url: url.to_owned(),
            title: Some(url.to_owned()),
            language: Some("eng".to_owned()),
            origin: "OpenSubtitles v3".to_owned(),
        }
    }

    fn subtitle_track(title: Option<&str>, language: Option<&str>) -> playback_mpv::SubtitleTrack {
        playback_mpv::SubtitleTrack {
            id: "1".to_owned(),
            title: title.map(ToOwned::to_owned),
            language: language.map(ToOwned::to_owned),
            codec: None,
            selected: false,
            external: true,
            source_url: None,
        }
    }

    #[test]
    fn a_url_label_falls_back_to_the_language_name() {
        assert_eq!(
            subtitle_track_label(&subtitle_track(
                Some("https://opensubtitles.example/abc.srt"),
                Some("eng"),
            )),
            "English"
        );
    }

    #[test]
    fn a_real_label_is_kept_verbatim() {
        assert_eq!(
            subtitle_track_label(&subtitle_track(Some("English (SDH)"), Some("eng"))),
            "English (SDH)"
        );
    }

    #[test]
    fn a_blank_label_falls_back_to_the_language_name() {
        assert_eq!(
            subtitle_track_label(&subtitle_track(Some("   "), Some("spa"))),
            "Spanish"
        );
    }

    #[test]
    fn an_unknown_addon_transport_gets_a_neutral_origin() {
        let addons = [(
            "https://opensubtitles.example/manifest.json".to_owned(),
            "OpenSubtitles v3".to_owned(),
        )];

        assert_eq!(
            addon_origin("https://opensubtitles.example/manifest.json", &addons),
            "OpenSubtitles v3"
        );
        assert_eq!(
            addon_origin("https://removed.example/manifest.json", &addons),
            UNKNOWN_SUBTITLE_ORIGIN
        );
    }

    #[test]
    fn subtitles_wait_for_the_media_and_are_only_added_once_per_load() {
        let mut session = SessionState::default();
        session.register_subtitles([
            external_subtitle("https://example.com/english.srt"),
            external_subtitle("https://example.com/english.srt"),
        ]);

        assert!(session.take_pending_subtitles().is_empty());
        assert_eq!(session.on_file_loaded().len(), 1);
        assert!(session.take_pending_subtitles().is_empty());
    }

    #[test]
    fn subtitles_are_added_again_after_a_same_source_recovery() {
        let mut session = SessionState::default();
        session.register_subtitles([external_subtitle("https://example.com/addon-subtitle.srt")]);

        assert_eq!(session.on_file_loaded().len(), 1);
        session.begin_subtitle_reload();
        assert!(session.take_pending_subtitles().is_empty());
        assert_eq!(session.on_file_loaded().len(), 1);
    }
}

impl PlaybackEventInbox {
    fn push(&self, event: PlaybackEvent) {
        let closes_inbox = matches!(&event, PlaybackEvent::Shutdown);
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(&event, PlaybackEvent::State(_))
            && matches!(queue.back(), Some(PlaybackEvent::State(_)))
        {
            if let Some(latest) = queue.back_mut() {
                *latest = event;
            }
        } else {
            queue.push_back(event);
        }
        drop(queue);

        if closes_inbox {
            self.closed.store(true, Ordering::Release);
            self.notify.notify_waiters();
        } else {
            self.notify.notify_one();
        }
    }

    async fn recv(&self) -> Option<PlaybackEvent> {
        loop {
            let notified = self.notify.notified();
            if let Some(event) = self
                .queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pop_front()
            {
                return Some(event);
            }
            if self.closed.load(Ordering::Acquire) {
                return None;
            }
            notified.await;
        }
    }
}

#[derive(Default)]
struct PlayerUiProjectionCache {
    previous: Option<Arc<PlaybackState>>,
}

#[derive(Default)]
struct UiStateScheduler {
    pending: AtomicBool,
    generation: AtomicU64,
    projection: Mutex<PlayerUiProjectionCache>,
}

#[derive(Clone, PartialEq)]
struct DiscordActivity {
    state: String,
    details: String,
    image: Option<String>,
    start_timestamp: Option<i64>,
    end_timestamp: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExternalSubtitle {
    url: String,
    title: Option<String>,
    language: Option<String>,
    /// Name of the add-on that supplied this track, shown as the menu's second
    /// line the way stremio-web renders `t(track.origin)`.
    origin: String,
}

impl ExternalSubtitle {
    fn from_resource(subtitle: &Subtitles, origin: &str) -> Self {
        let url = subtitle.url.to_string();
        let title = subtitle
            .label
            .as_deref()
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| Some(url.clone()));
        let language = Some(subtitle.lang.trim())
            .filter(|language| !language.is_empty())
            .map(ToOwned::to_owned);
        Self {
            url,
            title,
            language,
            origin: origin.to_owned(),
        }
    }
}

/// URL to add-on name for every external subtitle handed to MPV. MPV only
/// reports `external-filename` back on the track, so this is the only way to
/// map a track to the add-on that supplied it.
static SUBTITLE_ORIGINS: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();

fn subtitle_origins() -> &'static RwLock<HashMap<String, String>> {
    SUBTITLE_ORIGINS.get_or_init(Default::default)
}

fn record_subtitle_origins(subtitles: &[ExternalSubtitle]) {
    let mut origins = subtitle_origins()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for subtitle in subtitles {
        origins.insert(subtitle.url.clone(), subtitle.origin.clone());
    }
}

/// Shown for subtitles carried on the stream itself rather than fetched from an
/// add-on's subtitles resource.
const STREAM_SUBTITLE_ORIGIN: &str = "Stream";
/// Fallback when an add-on's transport URL is no longer in the profile, e.g. it
/// was uninstalled while the player was open.
const UNKNOWN_SUBTITLE_ORIGIN: &str = "Addon";

fn addon_origin(base: &str, addon_names: &[(String, String)]) -> String {
    addon_names
        .iter()
        .find(|(transport_url, _)| transport_url == base)
        .map_or_else(
            || UNKNOWN_SUBTITLE_ORIGIN.to_owned(),
            |(_, name)| name.clone(),
        )
}

fn subtitle_origin(source_url: Option<&str>) -> Option<String> {
    let source_url = source_url?;
    subtitle_origins()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(source_url)
        .cloned()
}

#[derive(Default)]
struct SessionState {
    url: Option<String>,
    subtitle_catalog: Vec<ExternalSubtitle>,
    loaded_subtitle_urls: HashSet<String>,
    media_loaded: bool,
    last_time: u64,
    last_time_dispatch: Option<Instant>,
    last_paused: Option<bool>,
    last_video_params: Option<VideoParams>,
    load_requested_at: Option<Instant>,
    last_discord_enabled: Option<bool>,
    last_discord_activity: Option<DiscordActivity>,
    last_discord_projection_at: Option<Instant>,
    last_discord_paused: Option<bool>,
    // OS media-session throttle: metadata is keyed on (generation, duration, title, image) so
    // it refreshes on an episode switch, duration update, or when metadata/poster arrives.
    last_media_meta_key: Option<(u64, i64, String, Option<String>)>,
    last_media_playing: Option<bool>,
    last_media_push_at: Option<Instant>,
    tidb_segments: Vec<crate::theintrodb::TidbSegment>,
    tidb_fetched_id: Option<String>,
    tidb_task: Option<tokio::task::JoinHandle<()>>,
    playback_generation: u64,
    last_skip_button_state: Option<SkipButtonState>,
    video_hash_resolved: bool,
    cached_video_hash: Option<String>,
    episode_selector_meta_id: Option<String>,
    episode_selector_video_id: Option<String>,
    episode_selector_season: Option<i32>,
    episode_selector_fingerprint: Option<SyncFingerprint>,
    recovery: crate::player_features::RecoveryState,
    recovery_task: Option<tokio::task::JoinHandle<()>>,
    sleep_timer: Option<crate::player_features::SleepTimerState>,
    sleep_task: Option<tokio::task::JoinHandle<()>>,
    preserve_end_timer_for_next_load: bool,
    last_capture_path: Option<PathBuf>,
    stream_switch_position: Option<f64>,
    pending_pause_restore: Option<bool>,
}

impl SessionState {
    fn begin_subtitle_source(&mut self) {
        self.subtitle_catalog.clear();
        self.begin_subtitle_reload();
    }

    fn begin_subtitle_reload(&mut self) {
        self.loaded_subtitle_urls.clear();
        self.media_loaded = false;
        subtitle_origins()
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    fn register_subtitles(&mut self, subtitles: impl IntoIterator<Item = ExternalSubtitle>) {
        for subtitle in subtitles {
            if !self
                .subtitle_catalog
                .iter()
                .any(|known| known.url == subtitle.url)
            {
                self.subtitle_catalog.push(subtitle);
            }
        }
    }

    fn on_file_loaded(&mut self) -> Vec<ExternalSubtitle> {
        self.media_loaded = true;
        self.take_pending_subtitles()
    }

    fn take_pending_subtitles(&mut self) -> Vec<ExternalSubtitle> {
        if !self.media_loaded {
            return Vec::new();
        }
        let mut pending = Vec::new();
        for subtitle in &self.subtitle_catalog {
            if self.loaded_subtitle_urls.insert(subtitle.url.clone()) {
                pending.push(subtitle.clone());
            }
        }
        pending
    }
}

/// Core requests subtitles from every installed add-on that offers the resource
/// (`AggrRequest::AllOfResource`), so `player.subtitles` holds one entry per
/// add-on. Take them all, tagging each with the add-on's name for the menu.
fn collect_player_subtitles(
    player: &Player,
    addon_names: &[(String, String)],
) -> Vec<ExternalSubtitle> {
    let mut subtitles = Vec::new();
    if let Some(selected) = player.selected.as_ref() {
        subtitles.extend(
            selected
                .stream
                .subtitles
                .iter()
                .map(|subtitle| ExternalSubtitle::from_resource(subtitle, STREAM_SUBTITLE_ORIGIN)),
        );
    }
    for resource in &player.subtitles {
        let Some(Loadable::Ready(addon_subtitles)) = resource.content.as_ref() else {
            continue;
        };
        let origin = addon_origin(resource.request.base.as_str(), addon_names);
        subtitles.extend(
            addon_subtitles
                .iter()
                .map(|subtitle| ExternalSubtitle::from_resource(subtitle, &origin)),
        );
    }
    subtitles
}

fn send_external_subtitles(
    controller: &PlaybackController,
    subtitles: Vec<ExternalSubtitle>,
    ui: &slint::Weak<MainWindow>,
) {
    // Recorded here rather than at the call sites: subtitles reach MPV both
    // from `sync_player` and from the file-loaded handler, and only tracks whose
    // origin was registered can show their add-on name in the menu.
    record_subtitle_origins(&subtitles);
    for subtitle in subtitles {
        send_or_show(
            controller,
            PlaybackCommand::AddSubtitle {
                url: subtitle.url,
                title: subtitle.title,
                language: subtitle.language,
            },
            ui,
        );
    }
}

#[derive(Clone)]
struct PlayerEpisodeProjection {
    id: String,
    title: String,
    released: String,
    thumbnail_url: String,
    season: i32,
    episode_num: i32,
    is_upcoming: bool,
    is_watched: bool,
    is_scheduled: bool,
    progress: f32,
    can_play: bool,
}

struct PlayerEpisodeSelectorProjection {
    fingerprint: SyncFingerprint,
    meta_id: String,
    seasons: Vec<i32>,
    active_season: i32,
    active_episode_idx: i32,
    active_video_id: String,
    has_next_episode: bool,
    next_episode_title: String,
    next_episode_thumbnail_url: String,
    series_name: String,
    series_runtime: String,
    series_release_year: String,
    series_description: String,
    series_logo_url: String,
    active_season_watched: bool,
    episodes: Vec<PlayerEpisodeProjection>,
}

#[cfg_attr(feature = "profiling", hotpath::measure)]
pub(crate) fn format_player_title(
    player: &Player,
    library: Option<&stremio_core::types::library::LibraryBucket>,
) -> Option<String> {
    let selected = player.selected.as_ref()?;
    let stream_request = selected.stream_request.as_ref()?;
    let video_id = &stream_request.path.id;

    if let Some(Loadable::Ready(meta_item)) =
        player.meta_item.as_ref().and_then(|m| m.content.as_ref())
    {
        let meta_name = &meta_item.preview.name;
        if meta_item.preview.behavior_hints.default_video_id.is_some() {
            return Some(meta_name.clone());
        }
        if let Some(video) = meta_item.videos.iter().find(|v| v.id == *video_id) {
            if let Some(series_info) = &video.series_info {
                if !video.title.is_empty() && video.title != *meta_name {
                    return Some(format!(
                        "{} - {} ({}x{})",
                        meta_name, video.title, series_info.season, series_info.episode
                    ));
                } else {
                    return Some(format!(
                        "{} ({}x{})",
                        meta_name, series_info.season, series_info.episode
                    ));
                }
            } else if !video.title.is_empty() && video.title != *meta_name {
                return Some(format!("{} - {}", meta_name, video.title));
            } else {
                return Some(meta_name.clone());
            }
        }
        return Some(meta_name.clone());
    }

    if let Some(library) = library {
        let meta_id = stream_request
            .path
            .id
            .split(':')
            .next()
            .unwrap_or(&stream_request.path.id);
        if let Some(lib_item) = library
            .items
            .get(&stream_request.path.id)
            .or_else(|| library.items.get(meta_id))
        {
            return Some(lib_item.name.clone());
        }
    }

    None
}

fn selected_player_video_id(player: &Player) -> String {
    player
        .selected
        .as_ref()
        .and_then(|selected| selected.stream_request.as_ref())
        .map(|request| request.path.id.clone())
        .unwrap_or_default()
}

fn player_episode_selector_projection(
    player: &Player,
    streams: &StreamsBucket,
    requested_season: Option<i32>,
) -> Option<PlayerEpisodeSelectorProjection> {
    let meta_item = player
        .meta_item
        .as_ref()?
        .content
        .as_ref()
        .and_then(Loadable::ready)?;
    let seasons = crate::models::details::ordered_series_seasons(meta_item);
    if seasons.is_empty() {
        return None;
    }

    let active_video_id = selected_player_video_id(player);
    let selected_season = player
        .series_info
        .as_ref()
        .map(|info| info.season as i32)
        .or_else(|| {
            meta_item
                .videos
                .iter()
                .find(|video| video.id == active_video_id)
                .and_then(|video| video.series_info.as_ref())
                .map(|info| info.season as i32)
        });
    let active_season = requested_season
        .filter(|season| seasons.contains(season))
        .or_else(|| selected_season.filter(|season| seasons.contains(season)))
        .unwrap_or(seasons[0]);
    let videos = crate::models::details::series_videos(meta_item, active_season);
    let is_scheduled = meta_item.preview.behavior_hints.has_scheduled_videos;
    let now = chrono::Utc::now();
    let mut fingerprint = Fingerprint::new();
    fingerprint.str(&meta_item.preview.id);
    fingerprint.usize(seasons.len());
    for season in &seasons {
        fingerprint.u64(*season as u64);
    }
    fingerprint.u64(active_season as u64);
    fingerprint.str(&active_video_id);
    fingerprint.bool(player.next_video.is_some());
    let next_episode_title = player
        .next_video
        .as_ref()
        .map(|video| {
            let base_title = if video.title.is_empty() {
                format!(
                    "Episode {}",
                    video.series_info.as_ref().map(|i| i.episode).unwrap_or(1)
                )
            } else {
                video.title.clone()
            };
            if let Some(info) = &video.series_info {
                format!("{base_title} (S{}E{})", info.season, info.episode)
            } else {
                base_title
            }
        })
        .unwrap_or_default();
    let next_episode_thumbnail_url = player
        .next_video
        .as_ref()
        .and_then(|video| video.thumbnail.clone())
        .or_else(|| meta_item.preview.poster.as_ref().map(ToString::to_string))
        .unwrap_or_default();
    let series_name = meta_item.preview.name.clone();
    let series_runtime = meta_item.preview.runtime.clone().unwrap_or_default();
    let series_release_year = meta_item
        .preview
        .released
        .map(|released| released.format("%Y").to_string())
        .unwrap_or_default();
    let series_description = meta_item.preview.description.clone().unwrap_or_default();
    let series_logo_url = meta_item
        .preview
        .logo
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    fingerprint.str(&next_episode_title);
    fingerprint.str(&next_episode_thumbnail_url);
    fingerprint.str(&series_name);
    fingerprint.str(&series_runtime);
    fingerprint.str(&series_release_year);
    fingerprint.str(&series_description);
    fingerprint.str(&series_logo_url);

    let episodes = videos
        .into_iter()
        .map(|video| {
            let episode_num = video
                .series_info
                .as_ref()
                .map(|info| info.episode as i32)
                .unwrap_or_default();
            let released = video
                .released
                .map(|date| date.format("%b %d, %Y").to_string())
                .unwrap_or_default();
            let thumbnail_url = video
                .thumbnail
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            let is_upcoming = is_scheduled
                && video
                    .released
                    .map(|released| released > now)
                    .unwrap_or(false);
            let is_watched = player
                .watched
                .as_ref()
                .map(|watched| watched.get_video(&video.id))
                .unwrap_or_default();
            let progress = player
                .library_item
                .as_ref()
                .filter(|item| item.state.video_id.as_deref() == Some(video.id.as_str()))
                .map(|item| item.progress() as f32)
                .unwrap_or_default();
            let can_play = streams.items.contains_key(&StreamsItemKey {
                meta_id: meta_item.preview.id.clone(),
                video_id: video.id.clone(),
            });

            fingerprint.str(&video.id);
            fingerprint.str(&video.title);
            fingerprint.str(&released);
            fingerprint.str(&thumbnail_url);
            fingerprint.u64(episode_num as u64);
            fingerprint.bool(is_upcoming);
            fingerprint.bool(is_watched);
            fingerprint.bool(is_scheduled);
            fingerprint.bool(can_play);

            PlayerEpisodeProjection {
                id: video.id.clone(),
                title: video.title.clone(),
                released,
                thumbnail_url,
                season: active_season,
                episode_num,
                is_upcoming,
                is_watched,
                is_scheduled,
                progress,
                can_play,
            }
        })
        .collect::<Vec<_>>();
    let active_episode_idx = episodes
        .iter()
        .position(|episode| episode.id == active_video_id)
        .unwrap_or_default() as i32;
    let active_season_watched =
        !episodes.is_empty() && episodes.iter().all(|episode| episode.is_watched);

    Some(PlayerEpisodeSelectorProjection {
        fingerprint: fingerprint.finish(),
        meta_id: meta_item.preview.id.clone(),
        seasons,
        active_season,
        active_episode_idx,
        active_video_id,
        has_next_episode: player.next_video.is_some(),
        next_episode_title,
        next_episode_thumbnail_url,
        series_name,
        series_runtime,
        series_release_year,
        series_description,
        series_logo_url,
        active_season_watched,
        episodes,
    })
}

fn apply_player_episode_selector(
    ui: &MainWindow,
    ui_weak: &slint::Weak<MainWindow>,
    projection: PlayerEpisodeSelectorProjection,
) {
    let next_episode_thumbnail_url = url::Url::parse(&projection.next_episode_thumbnail_url).ok();
    let next_episode_poster =
        crate::image_cache::get_poster_image(&next_episode_thumbnail_url, ui_weak);
    let series_logo_url = url::Url::parse(&projection.series_logo_url).ok();
    let series_logo = crate::image_cache::get_poster_image(&series_logo_url, ui_weak);
    let episodes = projection
        .episodes
        .into_iter()
        .map(|episode| {
            let thumbnail_url = url::Url::parse(&episode.thumbnail_url).ok();
            EpisodeItem {
                id: episode.id.into(),
                title: episode.title.into(),
                released: episode.released.into(),
                thumbnail_url: thumbnail_url
                    .as_ref()
                    .map(url::Url::as_str)
                    .unwrap_or_default()
                    .into(),
                thumbnail: crate::image_cache::get_poster_image(&thumbnail_url, ui_weak),
                season: episode.season,
                episode_num: episode.episode_num,
                is_upcoming: episode.is_upcoming,
                is_watched: episode.is_watched,
                is_scheduled: episode.is_scheduled,
                progress: episode.progress,
                can_play: episode.can_play,
            }
        })
        .collect::<Vec<_>>();

    ui.set_player_is_series(true);
    ui.set_player_seasons(ModelRc::new(VecModel::from(projection.seasons)));
    ui.set_player_active_season(projection.active_season);
    ui.set_player_episodes(ModelRc::new(VecModel::from(episodes)));
    ui.set_player_active_episode_idx(projection.active_episode_idx);
    ui.set_player_active_video_id(projection.active_video_id.into());
    ui.set_player_has_next_episode(projection.has_next_episode);
    ui.set_player_next_episode_title(projection.next_episode_title.into());
    ui.set_player_next_episode_poster(next_episode_poster);
    ui.set_player_series_name(projection.series_name.as_str().into());
    ui.set_player_next_series_name(projection.series_name.into());
    ui.set_player_series_runtime(projection.series_runtime.into());
    ui.set_player_series_release_year(projection.series_release_year.into());
    ui.set_player_series_description(projection.series_description.into());
    ui.set_player_series_logo_url(projection.series_logo_url.into());
    ui.set_player_series_logo(series_logo);
    ui.set_player_active_season_watched(projection.active_season_watched);
}

fn clear_player_episode_selector(ui: &MainWindow) {
    ui.set_player_is_series(false);
    ui.set_player_seasons(ModelRc::new(VecModel::from(Vec::<i32>::new())));
    ui.set_player_episodes(ModelRc::new(VecModel::from(Vec::<EpisodeItem>::new())));
    ui.set_player_active_video_id("".into());
    ui.set_player_active_episode_idx(0);
    ui.set_player_has_next_episode(false);
    ui.set_player_next_episode_title("".into());
    ui.set_player_next_episode_poster(slint::Image::default());
    ui.set_player_series_name("".into());
    ui.set_player_next_series_name("".into());
    ui.set_player_series_runtime("".into());
    ui.set_player_series_release_year("".into());
    ui.set_player_series_description("".into());
    ui.set_player_series_logo_url("".into());
    ui.set_player_series_logo(slint::Image::default());
    ui.set_player_active_season_watched(false);
    ui.set_player_show_playlist_drawer(false);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SkipButtonState {
    Hidden,
    Intro,
    Recap,
    Credits,
    Outro,
    Preview,
}

impl SkipButtonState {
    fn label(self) -> &'static str {
        match self {
            Self::Hidden => "",
            Self::Intro => "Skip Intro",
            Self::Recap => "Skip Recap",
            Self::Credits => "Skip Credits",
            Self::Outro => "Skip Outro",
            Self::Preview => "Skip Preview",
        }
    }

    fn is_visible(self) -> bool {
        self != Self::Hidden
    }
}

fn embedded_chapter_skip(state: &PlaybackState) -> Option<(SkipButtonState, f64)> {
    state
        .chapters
        .iter()
        .find(|chapter| state.time >= chapter.start && state.time < chapter.end)
        .and_then(|chapter| {
            let title = chapter.title.to_ascii_lowercase();
            let kind = crate::config::with_config(|config| {
                if config.tidb_show_recap
                    && (title.contains("recap") || title.contains("previously"))
                {
                    SkipButtonState::Recap
                } else if config.tidb_show_intro
                    && (title.contains("intro") || title.contains("opening"))
                {
                    SkipButtonState::Intro
                } else if config.tidb_show_credits && title.contains("outro") {
                    SkipButtonState::Outro
                } else if config.tidb_show_credits
                    && (title.contains("credit") || title.contains("ending"))
                {
                    SkipButtonState::Credits
                } else if config.tidb_show_preview && title.contains("preview") {
                    SkipButtonState::Preview
                } else {
                    SkipButtonState::Hidden
                }
            });
            kind.is_visible().then_some((kind, chapter.end))
        })
}

struct StatisticsPoll {
    key: (String, u16),
    cancellation: CancellationToken,
}

#[derive(Clone)]
pub struct NativePlaybackBridge {
    controller: PlaybackController,
    omniphony_decoder_available: bool,
    core: Arc<Runtime<DesktopEnv, AppModel>>,
    state: SharedPlaybackState,
    session: Arc<Mutex<SessionState>>,
    statistics_poll: Arc<Mutex<Option<StatisticsPoll>>>,
    discord_rpc: Arc<crate::discord::DiscordRpc>,
    media_session: Arc<crate::media_session::MediaSession>,
    autohide_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    runtime_handle: tokio::runtime::Handle,
    shaders: SharedShaderCoordinator,
    thumbnails: crate::thumbnail_preview::ThumbnailPreview,
    previews: crate::preview_player::PreviewPlayer,
    capture_directory: Arc<PathBuf>,
    capture_sequence: Arc<AtomicU64>,
}

pub struct NativePlayback {
    runtime: PlaybackRuntime,
    thumbnail_runtime: Option<ThumbnailRuntime>,
    preview_runtime: Option<PreviewRuntime>,
    bridge: NativePlaybackBridge,
    event_task: tokio::task::JoinHandle<()>,
}

pub(crate) struct PreparedPlaybackFiles {
    config_dir: PathBuf,
    capture_directory: PathBuf,
    shader_readiness: [bool; crate::shaders::SHADER_PRESET_COUNT],
}

pub(crate) async fn prepare_playback_files() -> anyhow::Result<PreparedPlaybackFiles> {
    tokio::task::spawn_blocking(|| {
        let config_dir = resolve_config_dir();
        std::fs::create_dir_all(&config_dir).with_context(|| {
            format!(
                "could not create MPV config directory {}",
                config_dir.display()
            )
        })?;
        if let Err(error) = crate::shaders::ensure_anime4k_shaders(&config_dir) {
            tracing::warn!(%error, "could not prepare the bundled video shaders");
        }
        if let Err(error) = crate::thumbnail_preview::disable_legacy_script(&config_dir) {
            tracing::warn!(%error, "could not disable the obsolete generated ThumbFast script");
        }
        let shader_readiness = crate::shaders::preset_readiness(&config_dir.join("shaders"));
        let capture_directory = crate::player_features::prepare_capture_directory()
            .context("could not prepare the frame capture directory")?;
        Ok(PreparedPlaybackFiles {
            config_dir,
            capture_directory,
            shader_readiness,
        })
    })
    .await
    .context("MPV file preparation task stopped")?
}

impl NativePlayback {
    #[expect(
        clippy::too_many_arguments,
        reason = "startup wiring passes each independent collaborator (UI, core, navigation, presence integrations, runtime) once; a bundle struct would only be built and destructured at this single call site"
    )]
    pub fn start(
        ui: &MainWindow,
        core: &Arc<Runtime<DesktopEnv, AppModel>>,
        hardware_decoding: bool,
        navigation: NavigationController,
        discord_rpc: Arc<crate::discord::DiscordRpc>,
        media_session: Arc<crate::media_session::MediaSession>,
        runtime_handle: tokio::runtime::Handle,
        prepared_files: PreparedPlaybackFiles,
    ) -> anyhow::Result<Self> {
        let state = Arc::new(RwLock::new(Arc::new(PlaybackState::default())));
        let session = Arc::new(Mutex::new(SessionState::default()));
        let statistics_poll = Arc::new(Mutex::new(None));
        let controller_slot = Arc::new(OnceLock::<PlaybackController>::new());
        let ui_state_scheduler = Arc::new(UiStateScheduler::default());
        let autohide_task = Arc::new(Mutex::new(None));

        let PreparedPlaybackFiles {
            config_dir,
            capture_directory,
            shader_readiness,
        } = prepared_files;
        tracing::info!(
            hardware_decoding,
            config_dir = %config_dir.display(),
            "initializing native MPV playback"
        );
        let app_config = crate::config::load_config();
        let thumbnails = crate::thumbnail_preview::ThumbnailPreview::new(
            app_config.thumbnail_previews_enabled,
            ui.as_weak(),
        );
        let thumbnail_events = thumbnails.clone();
        let thumbnail_runtime = match ThumbnailRuntime::start(
            ThumbnailConfig {
                hardware_decoding,
                ..ThumbnailConfig::default()
            },
            move |event| {
                thumbnail_events.handle_event(event);
            },
        ) {
            Ok(runtime) => {
                thumbnails.attach_controller(runtime.controller());
                Some(runtime)
            }
            Err(error) => {
                tracing::warn!(%error, "timeline thumbnail decoder could not be initialized");
                thumbnails.worker_failed(error.to_string());
                None
            }
        };
        let previews = crate::preview_player::PreviewPlayer::new(
            app_config.hover_trailer_previews_enabled,
            ui.as_weak(),
        );
        let preview_events = previews.clone();
        let preview_runtime = match PreviewRuntime::start(
            PreviewConfig {
                hardware_decoding,
                ytdl_path: resolve_ytdl_path(),
                ..PreviewConfig::default()
            },
            move |event| {
                preview_events.handle_event(event);
            },
        ) {
            Ok(runtime) => {
                previews.attach_controller(runtime.controller());
                Some(runtime)
            }
            Err(error) => {
                tracing::warn!(%error, "hover trailer preview decoder could not be initialized");
                previews.worker_failed(error.to_string());
                None
            }
        };
        let desired_shader_preset =
            crate::shaders::preset_from_config(app_config.active_shader_preset);
        let shader_coordinator = Arc::new(Mutex::new(
            crate::shaders::ShaderCoordinator::with_readiness(
                desired_shader_preset,
                shader_readiness,
            ),
        ));
        tracing::info!(
            desired_preset = ?desired_shader_preset,
            shaders_enabled = app_config.shaders_enabled,
            "loaded video shader preference"
        );
        let download_config_dir = config_dir.clone();
        let spatial_audio_sofa_path = resolve_spatial_audio_sofa_path(&config_dir);
        let omniphony_config_path = config_dir.join("omniphony").join("config.yaml");
        let event_inbox = Arc::new(PlaybackEventInbox::default());
        let runtime_event_inbox = event_inbox.clone();
        let runtime = PlaybackRuntime::start(
            PlayerConfig {
                config_dir: Some(config_dir),
                hardware_decoding,
                spatial_audio_sofa_path,
                omniphony_config_path: Some(omniphony_config_path),
                ytdl_path: resolve_ytdl_path(),
            },
            move |event| {
                runtime_event_inbox.push(event);
            },
        )
        .context("could not initialize the MPV playback engine")?;
        let omniphony_decoder_available = runtime.omniphony_decoder_available();
        let controller = runtime.controller();
        controller_slot
            .set(controller.clone())
            .map_err(|_| anyhow!("MPV controller was initialized twice"))?;
        if let Ok(model) = core.model() {
            let settings = &model.ctx.profile.settings;
            log_command(controller.send(PlaybackCommand::SetSubtitleScale(
                f64::from(settings.subtitles_size) / 100.0,
            )));
            log_command(
                controller.send(PlaybackCommand::SetSubtitlePosition(f64::from(
                    100_u8.saturating_sub(settings.subtitles_offset),
                ))),
            );
            ui.set_player_seek_step_seconds(settings.seek_time_duration as f32 / 1_000.0);
            ui.set_player_short_seek_step_seconds(
                settings.seek_short_time_duration as f32 / 1_000.0,
            );
            ui.set_player_subtitle_size_percent(f32::from(settings.subtitles_size));
            ui.set_player_subtitle_offset_percent(f32::from(settings.subtitles_offset));
        }
        let shader_ui = ui.as_weak();
        let initial_shader_update = {
            let mut coordinator = lock_shader_coordinator(&shader_coordinator);
            coordinator.initial_update()
        };
        dispatch_shader_update(&controller, &shader_ui, initial_shader_update);

        if !crate::shaders::anime4k_presets_ready(&shader_readiness) {
            let download_started = {
                let mut coordinator = lock_shader_coordinator(&shader_coordinator);
                coordinator.set_download_state(true, None)
            };
            dispatch_shader_update(&controller, &shader_ui, download_started);

            let download_controller = controller.clone();
            let download_coordinator = shader_coordinator.clone();
            let download_ui = shader_ui.clone();
            runtime_handle.spawn(async move {
                let error = crate::shaders::download_shaders_if_needed(&download_config_dir)
                    .await
                    .err()
                    .map(|error| error.to_string());
                let update = {
                    let mut coordinator = lock_shader_coordinator(&download_coordinator);
                    coordinator
                        .complete_download(&download_config_dir.join("shaders"), error.clone())
                };
                if let Some(error) = error {
                    tracing::warn!(%error, "Anime4K shader download failed");
                }
                dispatch_shader_update(&download_controller, &download_ui, update);
            });
        }
        install_renderer(
            ui,
            runtime.render_source(),
            state.clone(),
            session.clone(),
            controller.clone(),
            shader_coordinator.clone(),
        )?;

        let event_state = state.clone();
        let event_session = session.clone();
        let event_core = core.clone();
        let event_ui = ui.as_weak();
        let event_controller = controller_slot.clone();
        let event_scheduler = ui_state_scheduler.clone();
        let event_discord_rpc = discord_rpc.clone();
        let event_media_session = media_session.clone();
        let event_autohide_task = autohide_task.clone();
        let event_runtime_handle = runtime_handle.clone();
        let event_shader_coordinator = shader_coordinator.clone();
        let event_thumbnails = thumbnails.clone();
        let event_task = runtime_handle.spawn(async move {
            while let Some(event) = event_inbox.recv().await {
                handle_event(
                    event,
                    &event_state,
                    &event_session,
                    &event_core,
                    &event_controller,
                    &event_ui,
                    &event_scheduler,
                    &event_discord_rpc,
                    &event_media_session,
                    &event_autohide_task,
                    &event_runtime_handle,
                    &event_shader_coordinator,
                    &event_thumbnails,
                );
            }
            tracing::debug!("MPV application event pump stopped");
        });
        tracing::info!("native MPV playback initialized");

        let bridge = NativePlaybackBridge {
            controller,
            omniphony_decoder_available,
            core: core.clone(),
            state,
            session,
            statistics_poll,
            discord_rpc,
            media_session,
            autohide_task: autohide_task.clone(),
            runtime_handle,
            shaders: shader_coordinator,
            thumbnails,
            previews,
            capture_directory: Arc::new(capture_directory),
            capture_sequence: Arc::new(AtomicU64::new(1)),
        };
        bridge.install_callbacks(ui, core, navigation);
        Ok(Self {
            runtime,
            thumbnail_runtime,
            preview_runtime,
            bridge,
            event_task,
        })
    }

    pub fn bridge(&self) -> NativePlaybackBridge {
        self.bridge.clone()
    }

    pub fn shutdown(self) -> anyhow::Result<()> {
        self.bridge.cancel_statistics_poll();
        self.bridge.cancel_background_tasks();
        let _ = self.bridge.discord_rpc.disconnect();
        let thumbnail_result = self
            .thumbnail_runtime
            .map(ThumbnailRuntime::shutdown)
            .transpose();
        let preview_result = self
            .preview_runtime
            .map(PreviewRuntime::shutdown)
            .transpose();
        let result = self.runtime.shutdown().map_err(Into::into);
        self.event_task.abort();
        thumbnail_result.context("thumbnail worker did not shut down cleanly")?;
        preview_result.context("hover preview worker did not shut down cleanly")?;
        result
    }
}

impl NativePlaybackBridge {
    pub fn omniphony_decoder_available(&self) -> bool {
        self.omniphony_decoder_available
    }

    /// Handle onto the hover trailer preview engine for UI callback wiring.
    pub fn previews(&self) -> crate::preview_player::PreviewPlayer {
        self.previews.clone()
    }

    pub fn set_spatial_audio_mode(&self, mode: SpatialAudioMode) {
        log_command(
            self.controller
                .send(PlaybackCommand::SetSpatialAudioMode(mode)),
        );
    }

    pub fn configure_omniphony_audio(&self, settings: OmniphonyAudioSettings) {
        log_command(
            self.controller
                .send(PlaybackCommand::ConfigureOmniphonyAudio(settings)),
        );
    }

    pub fn recenter_omniphony_head(&self) {
        log_command(self.controller.send(PlaybackCommand::RecenterOmniphonyHead));
    }

    pub fn stop_for_profile_switch(&self) {
        self.cancel_statistics_poll();
        self.cancel_background_tasks();
        {
            let mut session = lock_session(&self.session);
            cancel_sleep_timer(&mut session);
            *session = SessionState::default();
        }
        log_command(self.controller.send(PlaybackCommand::Stop));
        self.thumbnails.leave();
    }

    pub fn current_source(&self) -> Option<String> {
        lock_session(&self.session).url.clone()
    }

    pub fn current_external_subtitle(&self) -> Option<String> {
        lock_session(&self.session)
            .loaded_subtitle_urls
            .iter()
            .next()
            .cloned()
    }

    pub fn play_local_file(
        &self,
        ui: &MainWindow,
        navigation: &NavigationController,
        path: &std::path::Path,
        title: &str,
    ) {
        let Some(url) = path.to_str().map(ToOwned::to_owned) else {
            ui.set_error_message("The local media path is not valid Unicode.".into());
            return;
        };
        if !path.is_file() {
            ui.set_error_message("The downloaded media file is no longer available.".into());
            return;
        }
        {
            let mut session = lock_session(&self.session);
            if let Some(task) = session.recovery_task.take() {
                task.abort();
            }
            if let Some(task) = session.tidb_task.take() {
                task.abort();
            }
            cancel_sleep_timer(&mut session);
            session.playback_generation = session.playback_generation.wrapping_add(1);
            session.url = Some(url.clone());
            session.begin_subtitle_source();
            session.last_time = 0;
            session.last_paused = None;
            session.load_requested_at = Some(Instant::now());
            session.recovery.reset_for_source();
            session.episode_selector_meta_id = None;
            session.episode_selector_video_id = None;
            session.episode_selector_fingerprint = None;
        }
        navigation.dispatch_and_project(ui, NavigationIntent::OpenPlayer);
        ui.set_player_title(title.into());
        ui.set_player_stream_name("Local media".into());
        ui.set_player_is_series(false);
        ui.set_player_loading(true);
        ui.set_player_error("".into());
        ui.set_player_video_frame(slint::Image::default());
        ui.set_player_has_video_frame(false);
        ui.set_player_ab_loop_a(-1.0);
        ui.set_player_ab_loop_b(-1.0);
        log_command(
            self.controller
                .send(PlaybackCommand::SetAbLoop { a: None, b: None }),
        );
        log_command(self.controller.send(PlaybackCommand::Load {
            url,
            start_at: None,
        }));
    }

    #[cfg_attr(feature = "profiling", hotpath::measure)]
    fn sync_episode_selector(
        &self,
        player: &Player,
        streams: &StreamsBucket,
        ui: &slint::Weak<MainWindow>,
    ) {
        let meta_id = player
            .meta_item
            .as_ref()
            .and_then(|resource| resource.content.as_ref().and_then(Loadable::ready))
            .map(|meta_item| meta_item.preview.id.clone());
        let Some(meta_id) = meta_id else {
            return;
        };
        let active_video_id = selected_player_video_id(player);

        let requested_season = {
            let mut session = lock_session(&self.session);
            if session.episode_selector_meta_id.as_deref() != Some(meta_id.as_str())
                || session.episode_selector_video_id.as_deref() != Some(active_video_id.as_str())
            {
                session.episode_selector_meta_id = Some(meta_id.clone());
                session.episode_selector_video_id = Some(active_video_id);
                session.episode_selector_season =
                    player.series_info.as_ref().map(|info| info.season as i32);
                session.episode_selector_fingerprint = None;
            }
            session.episode_selector_season
        };
        let Some(projection) =
            player_episode_selector_projection(player, streams, requested_season)
        else {
            let mut fingerprint = Fingerprint::new();
            fingerprint.str(&meta_id);
            fingerprint.bool(false);
            let fingerprint = fingerprint.finish();
            {
                let mut session = lock_session(&self.session);
                session.episode_selector_season = None;
                if session.episode_selector_fingerprint == Some(fingerprint) {
                    return;
                }
                session.episode_selector_fingerprint = Some(fingerprint);
            }
            let expected_video_id = selected_player_video_id(player);
            let session = self.session.clone();
            let ui_weak = ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let selector_is_current = {
                    let session = lock_session(&session);
                    session.episode_selector_meta_id.as_deref() == Some(meta_id.as_str())
                        && session.episode_selector_video_id.as_deref()
                            == Some(expected_video_id.as_str())
                        && session.episode_selector_season.is_none()
                };
                if selector_is_current && let Some(ui) = ui_weak.upgrade() {
                    clear_player_episode_selector(&ui);
                }
            });
            return;
        };

        {
            let mut session = lock_session(&self.session);
            session.episode_selector_season = Some(projection.active_season);
            if session.episode_selector_fingerprint == Some(projection.fingerprint) {
                return;
            }
            session.episode_selector_fingerprint = Some(projection.fingerprint);
        }

        let expected_meta_id = projection.meta_id.clone();
        let expected_season = projection.active_season;
        let expected_video_id = projection.active_video_id.clone();
        let session = self.session.clone();
        let ui_weak = ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let selector_is_current = {
                let session = lock_session(&session);
                session.episode_selector_meta_id.as_deref() == Some(expected_meta_id.as_str())
                    && session.episode_selector_video_id.as_deref()
                        == Some(expected_video_id.as_str())
                    && session.episode_selector_season == Some(expected_season)
            };
            if selector_is_current && let Some(ui) = ui_weak.upgrade() {
                apply_player_episode_selector(&ui, &ui_weak, projection);
            }
        });
    }

    fn select_episode_season(
        &self,
        player: &Player,
        streams: &StreamsBucket,
        season: i32,
        ui: &slint::Weak<MainWindow>,
    ) {
        let Some(projection) = player_episode_selector_projection(player, streams, Some(season))
        else {
            return;
        };
        {
            let mut session = lock_session(&self.session);
            session.episode_selector_meta_id = Some(projection.meta_id);
            session.episode_selector_video_id = Some(projection.active_video_id.clone());
            session.episode_selector_season = Some(projection.active_season);
            session.episode_selector_fingerprint = None;
        }
        self.sync_episode_selector(player, streams, ui);
    }

    fn step_episode_season(
        &self,
        player: &Player,
        streams: &StreamsBucket,
        direction: i32,
        ui: &slint::Weak<MainWindow>,
    ) {
        let requested_season = lock_session(&self.session).episode_selector_season;
        let Some(projection) =
            player_episode_selector_projection(player, streams, requested_season)
        else {
            return;
        };
        let season = crate::models::details::adjacent_series_season(
            &projection.seasons,
            projection.active_season,
            direction,
        );
        self.select_episode_season(player, streams, season, ui);
    }

    #[tracing::instrument(skip_all)]
    #[cfg_attr(feature = "profiling", hotpath::measure)]
    pub fn sync_player(
        &self,
        player: &Player,
        streams: &StreamsBucket,
        addon_names: &[(String, String)],
        ui: &slint::Weak<MainWindow>,
        navigation: &NavigationController,
    ) {
        // Per-model-update while the player is visible, so it is a `debug_span!`
        // for the same reason as the event loop's spans.
        let _span = tracing::debug_span!("sync_player").entered();
        if !navigation.is_player_visible() {
            return;
        }
        let route_revision = navigation.snapshot().revision;
        self.sync_statistics_poll(player);
        if let Some(ui_upgraded) = ui.upgrade() {
            let model_guard = self.core.model().ok();
            let library = model_guard.as_ref().map(|m| &m.ctx.library);
            if let Some(formatted_title) = format_player_title(player, library)
                && ui_upgraded.get_player_title().as_str() != formatted_title
            {
                ui_upgraded.set_player_title(formatted_title.into());
            }
        }
        let Some(Loadable::Ready((stream_urls, converted))) = player.stream.as_ref() else {
            self.sync_episode_selector(player, streams, ui);
            if let Some(Loadable::Err(error)) = player.stream.as_ref() {
                show_player_error(ui, format!("Could not resolve this stream: {error}"));
            }
            return;
        };
        let Some(url) = youtube_watch_url(converted)
            .or_else(|| stream_urls.streaming_url.as_ref().map(ToString::to_string))
        else {
            show_player_error(
                ui,
                "This stream does not provide a playable URL.".to_owned(),
            );
            return;
        };

        let resume_at = resume_time(player);
        let pending_load = {
            let mut session = lock_session(&self.session);
            if navigation.snapshot().revision != route_revision || !navigation.is_player_visible() {
                return;
            }
            if session.url.as_deref() == Some(url.as_str()) {
                None
            } else {
                if let Some(task) = session.tidb_task.take() {
                    task.abort();
                }
                if let Some(task) = session.recovery_task.take() {
                    task.abort();
                }
                session.recovery.reset_for_source();
                let preserve_end_timer = session.preserve_end_timer_for_next_load;
                session.preserve_end_timer_for_next_load = false;
                if !preserve_end_timer
                    && session.sleep_timer.as_ref().is_some_and(|timer| {
                        matches!(
                            timer.mode,
                            crate::player_features::SleepMode::EndOfCurrent
                                | crate::player_features::SleepMode::EndOfNext
                        )
                    })
                {
                    cancel_sleep_timer(&mut session);
                }
                session.playback_generation = session.playback_generation.wrapping_add(1);
                session.url = Some(url.clone());
                session.begin_subtitle_source();
                let start_at = session.stream_switch_position.take().or(resume_at);
                session.last_time = start_at.unwrap_or_default().round().max(0.0) as u64;
                session.last_time_dispatch = None;
                session.last_paused = None;
                session.last_video_params = None;
                session.video_hash_resolved = false;
                session.cached_video_hash = None;
                session.load_requested_at = Some(Instant::now());
                session.last_discord_activity = None;
                session.last_discord_projection_at = None;
                session.last_discord_paused = None;
                session.tidb_fetched_id = None;
                session.tidb_segments.clear();
                Some((session.playback_generation, url.clone(), start_at))
            }
        };
        if let Some((generation, url, start_at)) = pending_load {
            log_command(
                self.controller
                    .send(PlaybackCommand::SetAbLoop { a: None, b: None }),
            );
            self.thumbnails.begin_load(generation);
            send_or_show(
                &self.controller,
                PlaybackCommand::Load { url, start_at },
                ui,
            );
            let ui_for_update = ui.clone();
            let navigation_for_update = navigation.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if navigation_for_update.snapshot().revision != route_revision
                    || !navigation_for_update.is_player_visible()
                {
                    return;
                }
                if let Some(ui) = ui_for_update.upgrade() {
                    ui.set_player_error("".into());
                    ui.set_player_video_frame(slint::Image::default());
                    ui.set_player_has_video_frame(false);
                    ui.set_player_loading(true);
                    ui.set_player_buffering(false);
                    ui.set_player_buffering_percent(0.0);
                    ui.set_player_ab_loop_a(-1.0);
                    ui.set_player_ab_loop_b(-1.0);
                }
            });
        }
        self.sync_episode_selector(player, streams, ui);
        if !navigation.is_player_visible() {
            return;
        }
        let pending_subtitles = {
            let mut session = lock_session(&self.session);
            session.register_subtitles(collect_player_subtitles(player, addon_names));
            session.take_pending_subtitles()
        };
        send_external_subtitles(&self.controller, pending_subtitles, ui);
    }

    fn install_callbacks(
        &self,
        ui: &MainWindow,
        core: &Arc<Runtime<DesktopEnv, AppModel>>,
        navigation: NavigationController,
    ) {
        let pip_controller = Rc::new(RefCell::new(
            crate::player_features::PipController::default(),
        ));

        ui.on_player_activity({
            let bridge = self.clone();
            let weak_ui = ui.as_weak();
            move || {
                if let Some(ui) = weak_ui.upgrade() {
                    reset_autohide_timer(&ui, &bridge.autohide_task, &bridge.runtime_handle);
                }
            }
        });

        ui.on_player_toggle_pause({
            let controller = self.controller.clone();
            move || log_command(controller.send(PlaybackCommand::TogglePaused))
        });

        ui.on_player_seek({
            let controller = self.controller.clone();
            let state = self.state.clone();
            let session = self.session.clone();
            let core = core.clone();
            move |progress| {
                let state = read_state(&state).clone();
                let time = state.duration * f64::from(progress.clamp(0.0, 1.0));
                log_command(controller.send(PlaybackCommand::SeekAbsolute(time)));
                lock_session(&session).last_time = time.round().max(0.0) as u64;
                dispatch_player(
                    &core,
                    ActionPlayer::Seek {
                        time: time.round().max(0.0) as u64,
                        duration: state.duration.round().max(0.0) as u64,
                        device: PLAYER_DEVICE.to_owned(),
                    },
                );
            }
        });

        ui.on_player_seek_relative({
            let controller = self.controller.clone();
            let state = self.state.clone();
            let session = self.session.clone();
            let core = core.clone();
            move |seconds| {
                let state = read_state(&state).clone();
                let time = (state.time + f64::from(seconds)).clamp(0.0, state.duration.max(0.0));
                log_command(controller.send(PlaybackCommand::SeekRelative(f64::from(seconds))));
                lock_session(&session).last_time = time.round().max(0.0) as u64;
                dispatch_player(
                    &core,
                    ActionPlayer::Seek {
                        time: time.round().max(0.0) as u64,
                        duration: state.duration.round().max(0.0) as u64,
                        device: PLAYER_DEVICE.to_owned(),
                    },
                );
            }
        });

        ui.on_player_change_volume({
            let controller = self.controller.clone();
            move |volume| {
                log_command(controller.send(PlaybackCommand::SetVolume(f64::from(volume))))
            }
        });

        ui.on_player_toggle_mute({
            let controller = self.controller.clone();
            let state = self.state.clone();
            move || {
                let muted = !read_state(&state).muted;
                log_command(controller.send(PlaybackCommand::SetMuted(muted)));
            }
        });

        ui.on_player_change_audio({
            let controller = self.controller.clone();
            let state = self.state.clone();
            let core = core.clone();
            move |index| {
                let track = usize::try_from(index)
                    .ok()
                    .and_then(|index| read_state(&state).audio_tracks.get(index).cloned());
                let track_id = track.as_ref().map(|track| track.id.clone());
                log_command(controller.send(PlaybackCommand::SetAudioTrack(track_id)));
                update_stream_state(&core, |stream_state| {
                    stream_state.audio_track = track.map(|track| AudioTrack {
                        id: track.id,
                        language: track.language,
                    });
                });
            }
        });

        ui.on_player_change_subtitle({
            let controller = self.controller.clone();
            let state = self.state.clone();
            let core = core.clone();
            move |index| {
                let track = usize::try_from(index)
                    .ok()
                    .and_then(|index| read_state(&state).subtitle_tracks.get(index).cloned());
                let track_id = track.as_ref().map(|track| track.id.clone());
                log_command(controller.send(PlaybackCommand::SetSubtitleTrack(track_id)));
                update_stream_state(&core, |stream_state| {
                    stream_state.subtitle_track = track.map(|track| SubtitleTrack {
                        id: track.id,
                        embedded: !track.external,
                        language: track.language,
                    });
                });
            }
        });

        ui.on_player_change_secondary_subtitle({
            let controller = self.controller.clone();
            let state = self.state.clone();
            let weak = ui.as_weak();
            move |index| {
                let snapshot = read_state(&state).clone();
                let track_id = usize::try_from(index)
                    .ok()
                    .and_then(|index| snapshot.subtitle_tracks.get(index))
                    .map(|track| track.id.clone());
                if track_id.as_ref() == snapshot.active_subtitle_track.as_ref() {
                    queue_player_status(&weak, "Primary and secondary subtitles must be different");
                    return;
                }
                log_command(controller.send(PlaybackCommand::SetSecondarySubtitleTrack(track_id)));
            }
        });

        ui.on_player_change_subtitle_delay({
            let controller = self.controller.clone();
            let core = core.clone();
            move |seconds| {
                let milliseconds = (f64::from(seconds) * 1_000.0).round() as i64;
                log_command(controller.send(PlaybackCommand::SetSubtitleDelay(milliseconds)));
                update_stream_state(&core, |stream_state| {
                    stream_state.subtitle_delay = Some(milliseconds);
                });
            }
        });

        ui.on_player_change_subtitle_size({
            let controller = self.controller.clone();
            let core = core.clone();
            move |percent| {
                let percent = percent.clamp(50.0, 250.0);
                log_command(controller.send(PlaybackCommand::SetSubtitleScale(
                    f64::from(percent) / 100.0,
                )));
                update_stream_state(&core, |stream_state| {
                    stream_state.subtitle_size = Some(percent);
                });
            }
        });

        ui.on_player_change_subtitle_offset({
            let controller = self.controller.clone();
            let core = core.clone();
            move |percent| {
                let percent = percent.clamp(0.0, 100.0);
                log_command(
                    controller.send(PlaybackCommand::SetSubtitlePosition(f64::from(
                        100.0 - percent,
                    ))),
                );
                update_stream_state(&core, |stream_state| {
                    stream_state.subtitle_offset = Some(percent);
                });
            }
        });

        ui.on_player_change_secondary_subtitle_size({
            let controller = self.controller.clone();
            move |percent| {
                let percent = percent.clamp(50.0, 200.0);
                log_command(controller.send(PlaybackCommand::SetSecondarySubtitleScale(
                    f64::from(percent) / 100.0,
                )));
            }
        });

        ui.on_player_change_secondary_subtitle_offset({
            let controller = self.controller.clone();
            move |percent| {
                let percent = percent.clamp(0.0, 100.0);
                log_command(
                    controller.send(PlaybackCommand::SetSecondarySubtitlePosition(f64::from(
                        100.0 - percent,
                    ))),
                );
            }
        });

        ui.on_player_change_audio_delay({
            let controller = self.controller.clone();
            let core = core.clone();
            move |seconds| {
                let milliseconds = (f64::from(seconds) * 1_000.0).round() as i64;
                log_command(controller.send(PlaybackCommand::SetAudioDelay(milliseconds)));
                update_stream_state(&core, |stream_state| {
                    stream_state.audio_delay = Some(milliseconds);
                });
            }
        });

        ui.on_player_change_speed({
            let controller = self.controller.clone();
            let core = core.clone();
            move |speed| {
                log_command(controller.send(PlaybackCommand::SetSpeed(f64::from(speed))));
                update_stream_state(&core, |stream_state| {
                    stream_state.playback_speed = Some(speed);
                });
            }
        });

        ui.on_player_change_video_scale({
            let controller = self.controller.clone();
            move |mode| {
                let mode = u8::try_from(mode).unwrap_or_default() % 3;
                log_command(controller.send(PlaybackCommand::SetVideoScale(mode)));
            }
        });

        ui.on_player_change_shader_preset({
            let controller = self.controller.clone();
            let ui_weak = ui.as_weak();
            let shader_coordinator = self.shaders.clone();
            move |preset_idx| {
                let preset = crate::shaders::preset_from_ui(preset_idx);
                let update = {
                    let mut coordinator = lock_shader_coordinator(&shader_coordinator);
                    coordinator.select(preset)
                };
                let Some(update) = update else {
                    return;
                };
                dispatch_shader_update(&controller, &ui_weak, update);

                let mut cfg = crate::config::load_config();
                cfg.active_shader_preset = preset as u8;
                cfg.shaders_enabled = preset != crate::shaders::ShaderPreset::Off;
                crate::config::save_config(&cfg);
            }
        });

        ui.on_player_set_ab_loop_point({
            let controller = self.controller.clone();
            let state = self.state.clone();
            let weak = ui.as_weak();
            move |action| {
                let snapshot = read_state(&state).clone();
                let time = snapshot.time.max(0.0);
                let (a, b) = match action {
                    0 => {
                        let b = snapshot.ab_loop_b.filter(|b| *b - time >= 0.25);
                        (Some(time), b)
                    }
                    1 => {
                        let Some(a) = snapshot.ab_loop_a else {
                            queue_player_status(&weak, "Set point A before point B");
                            return;
                        };
                        if time - a < 0.25 {
                            queue_player_status(
                                &weak,
                                "Point B must be at least 0.25 seconds after A",
                            );
                            return;
                        }
                        (Some(a), Some(time))
                    }
                    2 => (None, None),
                    _ => return,
                };
                log_command(controller.send(PlaybackCommand::SetAbLoop { a, b }));
            }
        });

        ui.on_player_set_sleep_timer({
            let controller = self.controller.clone();
            let session = self.session.clone();
            let runtime_handle = self.runtime_handle.clone();
            let weak = ui.as_weak();
            move |value| {
                let mode = crate::player_features::SleepMode::from_ui_value(value);
                {
                    let mut current = lock_session(&session);
                    cancel_sleep_timer(&mut current);
                }
                let Some(mode) = mode else {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_player_sleep_timer_label("".into());
                        ui.set_player_status_message("Sleep timer cancelled".into());
                    }
                    return;
                };

                let timer = crate::player_features::SleepTimerState::new(mode);
                let cancellation = timer.cancellation.clone();
                let label = timer.mode.label();
                lock_session(&session).sleep_timer = Some(timer);
                if let Some(ui) = weak.upgrade() {
                    ui.set_player_sleep_timer_label(label.into());
                    ui.set_player_status_message("Sleep timer set".into());
                }

                let crate::player_features::SleepMode::After(duration) = mode else {
                    return;
                };
                let controller = controller.clone();
                let session_for_task = session.clone();
                let weak_for_task = weak.clone();
                let task = runtime_handle.spawn(async move {
                    tokio::select! {
                        _ = cancellation.cancelled() => return,
                        _ = tokio::time::sleep(duration) => {}
                    }
                    log_command(controller.send(PlaybackCommand::SetPaused(true)));
                    {
                        let mut current = lock_session(&session_for_task);
                        current.sleep_timer = None;
                        current.sleep_task = None;
                    }
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak_for_task.upgrade() {
                            ui.set_player_sleep_timer_label("".into());
                            ui.set_player_status_message("Sleep timer finished".into());
                        }
                    });
                });
                lock_session(&session).sleep_task = Some(task);
            }
        });

        ui.on_player_capture_frame({
            let controller = self.controller.clone();
            let directory = self.capture_directory.clone();
            let sequence = self.capture_sequence.clone();
            let weak = ui.as_weak();
            move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                let request_id = sequence.fetch_add(1, Ordering::Relaxed);
                let episode = ui.get_player_active_video_id();
                let path = crate::player_features::capture_path(
                    directory.as_path(),
                    ui.get_player_title().as_str(),
                    (!episode.is_empty()).then_some(episode.as_str()),
                    chrono::Utc::now(),
                    request_id,
                );
                log_command(controller.send(PlaybackCommand::CaptureFrame {
                    request_id,
                    path,
                    include_subtitles: true,
                }));
            }
        });

        ui.on_player_reveal_last_capture({
            let session = self.session.clone();
            move || {
                let path = lock_session(&session).last_capture_path.clone();
                let Some(path) = path else {
                    return;
                };
                let reveal_target = path.parent().unwrap_or(path.as_path());
                if let Err(error) = open::that(reveal_target) {
                    tracing::error!(%error, path = %reveal_target.display(), "failed to reveal frame capture");
                }
            }
        });

        ui.on_player_cycle_hdr_mode({
            let controller = self.controller.clone();
            let weak = ui.as_weak();
            move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                let (mode, index) = match ui.get_player_hdr_mode() {
                    0 => (HdrMode::Passthrough, 1),
                    1 => (HdrMode::ToneMap, 2),
                    2 => (HdrMode::Disabled, 3),
                    _ => (HdrMode::Auto, 0),
                };
                ui.set_player_hdr_mode(index);
                log_command(controller.send(PlaybackCommand::SetHdrMode(mode)));
            }
        });

        ui.on_player_toggle_pip({
            let weak = ui.as_weak();
            let pip_controller = pip_controller.clone();
            move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                let active = ui
                    .window()
                    .with_winit_window(|window| pip_controller.borrow_mut().toggle(window))
                    .unwrap_or(false);
                ui.set_player_pip_active(active);
                ui.set_is_fullscreen(false);
                ui.invoke_close_player_menus();
            }
        });

        ui.on_player_retry_playback({
            let controller = self.controller.clone();
            let state = self.state.clone();
            let session = self.session.clone();
            let weak = ui.as_weak();
            move || {
                let snapshot = read_state(&state).clone();
                let (url, last_time) = {
                    let current = lock_session(&session);
                    (current.url.clone(), current.last_time)
                };
                let Some(url) = url else {
                    return;
                };
                let start_at =
                    (snapshot.duration > 0.0).then_some(last_time.saturating_sub(3) as f64);
                if let Some(ui) = weak.upgrade() {
                    ui.set_player_error("".into());
                    ui.set_player_loading(true);
                    ui.set_player_status_message("Retrying the same source…".into());
                }
                log_command(controller.send(PlaybackCommand::Load { url, start_at }));
            }
        });

        ui.on_player_choose_another_stream({
            let weak = ui.as_weak();
            move || {
                if let Some(ui) = weak.upgrade() {
                    ui.invoke_player_close();
                }
            }
        });

        ui.on_player_return_to_details({
            let weak = ui.as_weak();
            move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_detail_in_stream_view(false);
                    ui.invoke_player_close();
                }
            }
        });

        ui.on_player_seek_hover({
            let state = self.state.clone();
            let thumbnails = self.thumbnails.clone();
            move |progress| {
                let duration = read_state(&state).duration;
                thumbnails.hover(progress, duration);
            }
        });

        ui.on_player_seek_leave({
            let thumbnails = self.thumbnails.clone();
            move || thumbnails.leave()
        });

        ui.on_player_copy_stream_link({
            let session = self.session.clone();
            move || {
                let url = lock_session(&session).url.clone();
                let Some(url) = url else {
                    tracing::warn!("cannot copy stream link before a stream is loaded");
                    return;
                };
                match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(url)) {
                    Ok(()) => tracing::info!("stream link copied to clipboard"),
                    Err(error) => tracing::error!(%error, "failed to copy stream link"),
                }
            }
        });

        ui.on_player_copy_magnet_link({
            let core = core.clone();
            move || {
                let link = core.model().ok().and_then(|model| {
                    let selected = model.player.selected.as_ref()?;
                    let deep_links = stremio_core::deep_links::StreamDeepLinks::from((
                        &selected.stream,
                        model.streaming_server.base_url.as_ref(),
                        &model.ctx.profile.settings,
                    ));
                    deep_links.external_player.magnet.clone().or_else(|| {
                        if let StreamSource::Torrent { info_hash, .. } = &selected.stream.source {
                            Some(format!("magnet:?xt=urn:btih:{}", hex::encode(info_hash)))
                        } else {
                            None
                        }
                    })
                });
                let Some(link) = link else {
                    tracing::warn!("no magnet link is available for the current stream");
                    return;
                };
                match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(link)) {
                    Ok(()) => tracing::info!("magnet link copied to clipboard"),
                    Err(error) => tracing::error!(%error, "failed to copy magnet link"),
                }
            }
        });

        ui.on_player_download_subs({
            let session = self.session.clone();
            move || {
                let subtitle_url = lock_session(&session)
                    .loaded_subtitle_urls
                    .iter()
                    .next()
                    .cloned();
                let Some(subtitle_url) = subtitle_url else {
                    tracing::warn!("no downloadable external subtitle is loaded");
                    return;
                };
                if let Err(error) = open::that(&subtitle_url) {
                    tracing::error!(%error, %subtitle_url, "failed to open subtitle URL");
                }
            }
        });

        ui.on_player_open_external_player({
            let session = self.session.clone();
            move || {
                let url = lock_session(&session).url.clone();
                let Some(url) = url else {
                    tracing::warn!("cannot open an external player before a stream is loaded");
                    return;
                };
                if let Err(error) = open::that(&url) {
                    tracing::error!(%error, %url, "failed to open stream in an external player");
                }
            }
        });

        ui.on_player_refresh_cast_devices({
            let core = core.clone();
            move || {
                core.dispatch(RuntimeAction {
                    field: Some(AppModelField::StreamingServer),
                    action: Action::StreamingServer(ActionStreamingServer::RefreshPlaybackDevices),
                });
            }
        });

        ui.on_player_cast_device({
            let core = core.clone();
            let session = self.session.clone();
            move |device| {
                let session = lock_session(&session);
                let Some(source) = session.url.clone() else {
                    tracing::warn!("cannot cast before a stream is loaded");
                    return;
                };
                let time = Some(session.last_time);
                drop(session);
                core.dispatch(RuntimeAction {
                    field: Some(AppModelField::StreamingServer),
                    action: Action::StreamingServer(ActionStreamingServer::PlayOnDevice(
                        PlayOnDeviceArgs {
                            device: device.to_string(),
                            source,
                            time,
                        },
                    )),
                });
            }
        });

        ui.on_player_download_video({
            let session = self.session.clone();
            move || {
                let url = lock_session(&session).url.clone();
                let Some(url) = url else {
                    tracing::warn!("cannot download video before a stream is loaded");
                    return;
                };
                if let Err(error) = open::that(&url) {
                    tracing::error!(%error, %url, "failed to open video download URL");
                }
            }
        });

        ui.on_player_season_changed({
            let bridge = self.clone();
            let core = core.clone();
            let weak = ui.as_weak();
            move |season| {
                let snapshot = core
                    .model()
                    .ok()
                    .map(|model| (model.player.clone(), model.ctx.streams.clone()));
                if let Some((player, streams)) = snapshot {
                    bridge.select_episode_season(&player, &streams, season, &weak);
                }
            }
        });

        ui.on_player_season_step({
            let bridge = self.clone();
            let core = core.clone();
            let weak = ui.as_weak();
            move |direction| {
                let snapshot = core
                    .model()
                    .ok()
                    .map(|model| (model.player.clone(), model.ctx.streams.clone()));
                if let Some((player, streams)) = snapshot {
                    bridge.step_episode_season(&player, &streams, direction, &weak);
                }
            }
        });

        ui.on_player_toggle_episode_watched({
            let core = core.clone();
            move |video_id| {
                let selection = core.model().ok().and_then(|model| {
                    let player = &model.player;
                    let meta_item = player
                        .meta_item
                        .as_ref()?
                        .content
                        .as_ref()
                        .and_then(Loadable::ready)?;
                    let video = meta_item
                        .videos
                        .iter()
                        .find(|video| video.id == video_id.as_str())?
                        .clone();
                    let watched = player
                        .watched
                        .as_ref()
                        .map(|watched| watched.get_video(&video.id))
                        .unwrap_or_default();
                    Some((video, !watched))
                });
                if let Some((video, watched)) = selection {
                    dispatch_player(&core, ActionPlayer::MarkVideoAsWatched(video, watched));
                }
            }
        });

        ui.on_player_mark_season_watched({
            let core = core.clone();
            move |season| {
                if season <= 0 {
                    return;
                }
                let season_u32 = season as u32;
                let selection = core.model().ok().and_then(|model| {
                    let player = &model.player;
                    let meta_item = player
                        .meta_item
                        .as_ref()?
                        .content
                        .as_ref()
                        .and_then(Loadable::ready)?;
                    let season_videos: Vec<_> = meta_item
                        .videos
                        .iter()
                        .filter(|v| {
                            v.series_info
                                .as_ref()
                                .map(|info| info.season == season_u32)
                                .unwrap_or(false)
                        })
                        .cloned()
                        .collect();
                    if season_videos.is_empty() {
                        return None;
                    }
                    let all_watched = season_videos.iter().all(|v| {
                        player
                            .watched
                            .as_ref()
                            .map(|w| w.get_video(&v.id))
                            .unwrap_or_default()
                    });
                    Some((season_videos, !all_watched))
                });
                if let Some((videos, target_watched)) = selection {
                    for video in videos {
                        dispatch_player(
                            &core,
                            ActionPlayer::MarkVideoAsWatched(video, target_watched),
                        );
                    }
                }
            }
        });

        ui.on_player_next_episode({
            let core = core.clone();
            let controller = self.controller.clone();
            let weak = ui.as_weak();
            let statistics_poll = self.statistics_poll.clone();
            let session = self.session.clone();
            let navigation = navigation.clone();
            let discord_rpc = self.discord_rpc.clone();
            let thumbnails = self.thumbnails.clone();
            let pip_controller = pip_controller.clone();
            move || {
                if play_next(&core) {
                    return;
                }
                let selection = core.model().ok().and_then(|model| {
                    let player = &model.player;
                    let next_video = player.next_video.as_ref()?;
                    let video_id = next_video.id.clone();
                    let meta_id = player
                        .meta_item
                        .as_ref()?
                        .content
                        .as_ref()
                        .and_then(Loadable::ready)?
                        .preview
                        .id
                        .clone();
                    let saved_stream = model
                        .ctx
                        .streams
                        .items
                        .get(&StreamsItemKey {
                            meta_id: meta_id.clone(),
                            video_id: video_id.clone(),
                        })
                        .cloned();
                    let (season, index) = player
                        .meta_item
                        .as_ref()
                        .and_then(|resource| resource.content.as_ref().and_then(Loadable::ready))
                        .and_then(|meta_item| {
                            meta_item
                                .videos
                                .iter()
                                .enumerate()
                                .find(|(_, video)| video.id == video_id)
                        })
                        .map(|(idx, video)| {
                            let s = video.series_info.as_ref().map(|info| info.season as i32);
                            (s, idx as i32)
                        })
                        .unwrap_or((None, 0));
                    Some((meta_id, video_id, saved_stream, season, index))
                });
                let Some((meta_id, video_id, saved_stream, season, index)) = selection else {
                    return;
                };
                if let (Some(ui), Some(saved_stream)) = (weak.upgrade(), saved_stream) {
                    crate::deep_link::open_saved_stream(&ui, &core, &navigation, saved_stream);
                    return;
                }

                unload_player(
                    &controller,
                    &core,
                    &statistics_poll,
                    &session,
                    &discord_rpc,
                    &thumbnails,
                );
                if let Some(ui) = weak.upgrade() {
                    if !navigation.is_player_visible() {
                        return;
                    }
                    let _ = ui.window().with_winit_window(|window| {
                        pip_controller.borrow_mut().exit(window);
                    });
                    ui.set_player_pip_active(false);
                    if let Some(season) = season {
                        ui.set_detail_active_season(season);
                        ui.invoke_details_season_changed(season);
                    }
                    navigation.dispatch_and_project(
                        &ui,
                        NavigationIntent::OpenDetails { media_id: meta_id },
                    );
                    ui.set_player_active_episode_idx(index);
                    ui.set_detail_active_episode_idx(index);
                    ui.invoke_details_episode_changed(index, video_id.into());
                    ui.set_player_loading(false);
                    ui.set_player_buffering(false);
                    ui.set_player_has_video_frame(false);
                    ui.set_player_video_frame(slint::Image::default());
                    clear_player_episode_selector(&ui);
                    if ui.window().is_fullscreen() {
                        ui.window().set_fullscreen(false);
                    }
                }
            }
        });

        ui.on_player_play_saved_episode({
            let core = core.clone();
            let controller = self.controller.clone();
            let weak = ui.as_weak();
            let statistics_poll = self.statistics_poll.clone();
            let session = self.session.clone();
            let navigation = navigation.clone();
            let discord_rpc = self.discord_rpc.clone();
            let thumbnails = self.thumbnails.clone();
            let pip_controller = pip_controller.clone();
            move |video_id| {
                let video_id = video_id.to_string();
                let selection = core.model().ok().and_then(|model| {
                    let player = &model.player;
                    let is_current = selected_player_video_id(player) == video_id;
                    let is_next = player
                        .next_video
                        .as_ref()
                        .is_some_and(|video| video.id == video_id);
                    let meta_id = player
                        .meta_item
                        .as_ref()?
                        .content
                        .as_ref()
                        .and_then(Loadable::ready)?
                        .preview
                        .id
                        .clone();
                    let saved_stream = model
                        .ctx
                        .streams
                        .items
                        .get(&StreamsItemKey {
                            meta_id: meta_id.clone(),
                            video_id: video_id.clone(),
                        })
                        .cloned();
                    let (season, index) = player
                        .meta_item
                        .as_ref()
                        .and_then(|resource| resource.content.as_ref().and_then(Loadable::ready))
                        .and_then(|meta_item| {
                            meta_item
                                .videos
                                .iter()
                                .enumerate()
                                .find(|(_, video)| video.id == video_id)
                        })
                        .map(|(idx, video)| {
                            let s = video.series_info.as_ref().map(|info| info.season as i32);
                            (s, idx as i32)
                        })
                        .unwrap_or((None, 0));
                    Some((is_current, is_next, meta_id, saved_stream, season, index))
                });
                let Some((is_current, is_next, meta_id, saved_stream, season, index)) = selection
                else {
                    return;
                };
                if is_current {
                    return;
                }
                if is_next && play_next(&core) {
                    return;
                }
                if let (Some(ui), Some(saved_stream)) = (weak.upgrade(), saved_stream) {
                    crate::deep_link::open_saved_stream(&ui, &core, &navigation, saved_stream);
                    return;
                }

                // Fallback: If no saved stream or play_next returned false, switch to Details for this episode
                unload_player(
                    &controller,
                    &core,
                    &statistics_poll,
                    &session,
                    &discord_rpc,
                    &thumbnails,
                );
                if let Some(ui) = weak.upgrade() {
                    if !navigation.is_player_visible() {
                        return;
                    }
                    let _ = ui.window().with_winit_window(|window| {
                        pip_controller.borrow_mut().exit(window);
                    });
                    ui.set_player_pip_active(false);
                    if let Some(season) = season {
                        ui.set_detail_active_season(season);
                        ui.invoke_details_season_changed(season);
                    }
                    navigation.dispatch_and_project(
                        &ui,
                        NavigationIntent::OpenDetails { media_id: meta_id },
                    );
                    ui.set_player_active_episode_idx(index);
                    ui.set_detail_active_episode_idx(index);
                    ui.invoke_details_episode_changed(index, video_id.into());
                    ui.set_player_loading(false);
                    ui.set_player_buffering(false);
                    ui.set_player_has_video_frame(false);
                    ui.set_player_video_frame(slint::Image::default());
                    clear_player_episode_selector(&ui);
                    if ui.window().is_fullscreen() {
                        ui.window().set_fullscreen(false);
                    }
                }
            }
        });

        ui.on_player_play_episode({
            let core = core.clone();
            let controller = self.controller.clone();
            let weak = ui.as_weak();
            let statistics_poll = self.statistics_poll.clone();
            let session = self.session.clone();
            let navigation = navigation.clone();
            let discord_rpc = self.discord_rpc.clone();
            let thumbnails = self.thumbnails.clone();
            let pip_controller = pip_controller.clone();
            move |index, video_id| {
                let video_id = video_id.to_string();
                let selection = core.model().ok().map(|model| {
                    let player = &model.player;
                    let is_current = selected_player_video_id(player) == video_id;
                    let is_next = player
                        .next_video
                        .as_ref()
                        .is_some_and(|video| video.id == video_id);
                    let meta_id = player
                        .meta_item
                        .as_ref()
                        .and_then(|resource| resource.content.as_ref().and_then(Loadable::ready))
                        .map(|meta_item| meta_item.preview.id.clone());
                    let season = player
                        .meta_item
                        .as_ref()
                        .and_then(|resource| resource.content.as_ref().and_then(Loadable::ready))
                        .and_then(|meta_item| {
                            meta_item.videos.iter().find(|video| video.id == video_id)
                        })
                        .and_then(|video| video.series_info.as_ref())
                        .map(|info| info.season as i32);
                    (is_current, is_next, meta_id, season)
                });
                let (is_current, is_next, meta_id, season) = selection.unwrap_or_default();
                if is_current {
                    return;
                }
                if is_next && play_next(&core) {
                    return;
                }

                unload_player(
                    &controller,
                    &core,
                    &statistics_poll,
                    &session,
                    &discord_rpc,
                    &thumbnails,
                );
                if let Some(ui) = weak.upgrade() {
                    if !navigation.is_player_visible() {
                        return;
                    }
                    let _ = ui.window().with_winit_window(|window| {
                        pip_controller.borrow_mut().exit(window);
                    });
                    ui.set_player_pip_active(false);
                    if let Some(season) = season {
                        ui.set_detail_active_season(season);
                        ui.invoke_details_season_changed(season);
                    }
                    if let Some(meta_id) = meta_id {
                        navigation.dispatch_and_project(
                            &ui,
                            NavigationIntent::OpenDetails { media_id: meta_id },
                        );
                    } else {
                        navigation.dispatch_and_project(&ui, NavigationIntent::Back);
                    }
                    ui.set_player_active_episode_idx(index);
                    ui.set_detail_active_episode_idx(index);
                    ui.invoke_details_episode_changed(index, video_id.into());
                    ui.set_player_loading(false);
                    ui.set_player_buffering(false);
                    ui.set_player_has_video_frame(false);
                    ui.set_player_video_frame(slint::Image::default());
                    clear_player_episode_selector(&ui);
                    if ui.window().is_fullscreen() {
                        ui.window().set_fullscreen(false);
                    }
                }
            }
        });

        ui.on_player_close({
            let controller = self.controller.clone();
            let core = core.clone();
            let weak = ui.as_weak();
            let statistics_poll = self.statistics_poll.clone();
            let session = self.session.clone();
            let navigation = navigation.clone();
            let discord_rpc = self.discord_rpc.clone();
            let media_session = self.media_session.clone();
            let autohide_task = self.autohide_task.clone();
            let thumbnails = self.thumbnails.clone();
            let pip_controller = pip_controller.clone();
            move || {
                // Release the OS media controls and wake lock up front, so an exit
                // that emits no further playback state can't strand either.
                media_session.clear();
                crate::taskbar_media::set_state(crate::taskbar_media::ButtonState::Hidden);
                if let Some(ui) = weak.upgrade() {
                    if !navigation.is_player_visible() {
                        return;
                    }
                    let _ = ui.window().with_winit_window(|window| {
                        pip_controller.borrow_mut().exit(window);
                    });
                    ui.set_player_pip_active(false);
                    navigation.dispatch_and_project(&ui, NavigationIntent::Back);
                    ui.invoke_close_player_menus();
                    // Never leave the player with the pointer hidden after exit.
                    set_player_cursor_hidden(&ui, false);
                    ui.set_player_loading(false);
                    ui.set_player_buffering(false);
                    ui.set_player_has_video_frame(false);
                    ui.set_player_video_frame(slint::Image::default());
                    clear_player_episode_selector(&ui);
                    if ui.window().is_fullscreen() {
                        ui.window().set_fullscreen(false);
                        ui.set_is_fullscreen(false);
                    }
                }
                if let Some(handle) = lock_autohide_task(&autohide_task).take() {
                    handle.abort();
                }
                unload_player(
                    &controller,
                    &core,
                    &statistics_poll,
                    &session,
                    &discord_rpc,
                    &thumbnails,
                );
            }
        });

        ui.on_player_skip_segment({
            let controller = self.controller.clone();
            let state = self.state.clone();
            let session = self.session.clone();
            let core = core.clone();
            move || {
                let state_val = read_state(&state).clone();
                let mut session_lock = lock_session(&session);
                let active_segment = crate::theintrodb::check_active_segment(
                    state_val.time,
                    &session_lock.tidb_segments,
                )
                .map(|segment| (segment.segment_type.as_str(), segment.end_secs));

                if let Some((segment_type, end_secs)) = active_segment {
                    tracing::info!(%segment_type, end_secs, "skipping TheIntroDB segment");
                    log_command(controller.send(PlaybackCommand::SeekAbsolute(end_secs)));
                    session_lock.last_time = end_secs.round().max(0.0) as u64;
                    dispatch_player(
                        &core,
                        ActionPlayer::Seek {
                            time: end_secs.round().max(0.0) as u64,
                            duration: state_val.duration.round().max(0.0) as u64,
                            device: PLAYER_DEVICE.to_owned(),
                        },
                    );
                } else if let Some((chapter_kind, end_secs)) = embedded_chapter_skip(&state_val) {
                    tracing::info!(?chapter_kind, end_secs, "skipping embedded chapter");
                    log_command(controller.send(PlaybackCommand::SeekAbsolute(end_secs)));
                    session_lock.last_time = end_secs.round().max(0.0) as u64;
                    dispatch_player(
                        &core,
                        ActionPlayer::Seek {
                            time: end_secs.round().max(0.0) as u64,
                            duration: state_val.duration.round().max(0.0) as u64,
                            device: PLAYER_DEVICE.to_owned(),
                        },
                    );
                }
            }
        });

        ui.on_player_toggle_fullscreen({
            let weak = ui.as_weak();
            let pip_controller = pip_controller.clone();
            move || {
                if let Some(ui) = weak.upgrade() {
                    if pip_controller.borrow().is_active() {
                        let _ = ui.window().with_winit_window(|window| {
                            pip_controller.borrow_mut().exit(window);
                        });
                        ui.set_player_pip_active(false);
                    }
                    let fs = !ui.window().is_fullscreen();
                    ui.window().set_fullscreen(fs);
                    ui.set_is_fullscreen(fs);
                }
            }
        });
    }

    fn sync_statistics_poll(&self, player: &Player) {
        let request = player.selected.as_ref().and_then(|selected| {
            let StreamSource::Torrent {
                info_hash,
                file_idx,
                ..
            } = &selected.stream.source
            else {
                return None;
            };
            Some(StatisticsRequest {
                info_hash: info_hash.iter().map(|byte| format!("{byte:02x}")).collect(),
                file_idx: file_idx.unwrap_or_default(),
            })
        });
        let Some(request) = request else {
            self.cancel_statistics_poll();
            return;
        };
        let key = (request.info_hash.clone(), request.file_idx);
        {
            let current = lock_statistics_poll(&self.statistics_poll);
            if current.as_ref().is_some_and(|poll| poll.key == key) {
                return;
            }
        }
        self.cancel_statistics_poll();
        let cancellation = CancellationToken::new();
        *lock_statistics_poll(&self.statistics_poll) = Some(StatisticsPoll {
            key,
            cancellation: cancellation.clone(),
        });
        let core_request = request.clone();
        let core = self.core.clone();
        self.runtime_handle.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    _ = interval.tick() => core.dispatch(RuntimeAction {
                        field: None,
                        action: Action::StreamingServer(
                            ActionStreamingServer::GetStatistics(core_request.clone())
                        ),
                    }),
                }
            }
        });
    }

    fn cancel_statistics_poll(&self) {
        cancel_statistics_poll(&self.statistics_poll);
    }

    pub fn set_thumbnail_previews_enabled(&self, enabled: bool) {
        let current_source = if enabled && read_state(&self.state).loaded {
            let session = lock_session(&self.session);
            session.url.as_ref().map(|url| ThumbnailSource {
                generation: session.playback_generation,
                url: url.clone(),
                initial_position: session.last_time as f64,
            })
        } else {
            None
        };
        self.thumbnails.set_enabled(enabled, current_source);
    }

    /// Preserve same-media playback state across an explicit in-player source
    /// change. The next resolved URL consumes this snapshot exactly once.
    pub fn prepare_stream_switch(&self) {
        let state = read_state(&self.state).clone();
        let mut session = lock_session(&self.session);
        session.stream_switch_position = Some(state.time.max(0.0));
        session.pending_pause_restore = Some(state.paused);
        session.last_time = state.time.round().max(0.0) as u64;
        session.recovery.reset_for_source();
    }

    fn cancel_background_tasks(&self) {
        let mut session = lock_session(&self.session);
        if let Some(task) = session.tidb_task.take() {
            task.abort();
        }
        if let Some(task) = session.recovery_task.take() {
            task.abort();
        }
        cancel_sleep_timer(&mut session);
        drop(session);
        if let Some(task) = lock_autohide_task(&self.autohide_task).take() {
            task.abort();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryDisposition {
    Scheduled,
    InFlight,
    Exhausted,
    Unavailable,
}

fn schedule_automatic_recovery(
    state: &PlaybackState,
    session: &Arc<Mutex<SessionState>>,
    controller: &Arc<OnceLock<PlaybackController>>,
    ui: &slint::Weak<MainWindow>,
    runtime_handle: &tokio::runtime::Handle,
) -> RecoveryDisposition {
    let Some(controller) = controller.get().cloned() else {
        return RecoveryDisposition::Unavailable;
    };
    let (generation, url, start_at) = {
        let mut current = lock_session(session);
        if current
            .recovery_task
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return RecoveryDisposition::InFlight;
        }
        current.recovery_task = None;
        if !current.recovery.claim_automatic_retry() {
            return RecoveryDisposition::Exhausted;
        }
        let Some(url) = current.url.clone() else {
            return RecoveryDisposition::Unavailable;
        };
        let start_at = (state.duration > 0.0).then_some(current.last_time.saturating_sub(3) as f64);
        (current.playback_generation, url, start_at)
    };

    let task_session = session.clone();
    let task_ui = ui.clone();
    let task = runtime_handle.spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let current_source = {
            let mut current = lock_session(&task_session);
            let current_source = current.playback_generation == generation
                && current.url.as_deref() == Some(url.as_str());
            if current_source {
                current.load_requested_at = Some(Instant::now());
                current.begin_subtitle_reload();
            }
            current.recovery_task = None;
            current_source
        };
        if !current_source {
            tracing::debug!(generation, "discarded stale playback recovery callback");
            return;
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = task_ui.upgrade() {
                ui.set_player_error("".into());
                ui.set_player_loading(true);
                ui.set_player_status_message("Retrying the same source (1/1)…".into());
            }
        });
        log_command(controller.send(PlaybackCommand::Load { url, start_at }));
    });
    lock_session(session).recovery_task = Some(task);
    RecoveryDisposition::Scheduled
}

fn consume_end_sleep_timer(session: &Arc<Mutex<SessionState>>) -> bool {
    let mut current = lock_session(session);
    let should_stop = current
        .sleep_timer
        .as_mut()
        .is_some_and(crate::player_features::SleepTimerState::consume_episode_end);
    if should_stop {
        cancel_sleep_timer(&mut current);
    } else if current.sleep_timer.as_ref().is_some_and(|timer| {
        matches!(
            timer.mode,
            crate::player_features::SleepMode::EndOfCurrent
                | crate::player_features::SleepMode::EndOfNext
        )
    }) {
        current.preserve_end_timer_for_next_load = true;
    }
    should_stop
}

fn queue_player_status(ui: &slint::Weak<MainWindow>, message: impl Into<SharedString>) {
    let message = message.into();
    let ui = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui.upgrade() {
            ui.set_player_status_message(message);
        }
    });
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal event dispatcher wiring independent subsystem handles; grouping them into a struct would add indirection without reuse"
)]
fn handle_event(
    event: PlaybackEvent,
    state_slot: &SharedPlaybackState,
    session: &Arc<Mutex<SessionState>>,
    core: &Arc<Runtime<DesktopEnv, AppModel>>,
    controller: &Arc<OnceLock<PlaybackController>>,
    ui: &slint::Weak<MainWindow>,
    ui_state_scheduler: &Arc<UiStateScheduler>,
    discord_rpc: &Arc<crate::discord::DiscordRpc>,
    media_session: &Arc<crate::media_session::MediaSession>,
    autohide_task: &Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    runtime_handle: &tokio::runtime::Handle,
    shader_coordinator: &SharedShaderCoordinator,
    thumbnails: &crate::thumbnail_preview::ThumbnailPreview,
) {
    match event {
        PlaybackEvent::State(state) => {
            let previous = read_state(state_slot).clone();
            if previous.loading != state.loading
                || previous.loaded != state.loaded
                || previous.paused != state.paused
                || previous.buffering != state.buffering
                || previous.seeking != state.seeking
            {
                tracing::info!(
                    loading = state.loading,
                    loaded = state.loaded,
                    paused = state.paused,
                    buffering = state.buffering,
                    seeking = state.seeking,
                    duration_seconds = state.duration,
                    "MPV playback state changed"
                );
            }
            match state_slot.write() {
                Ok(mut current) => *current = state.clone(),
                Err(poisoned) => *poisoned.into_inner() = state.clone(),
            }
            lock_session(session).recovery.observe_playback(
                state.loaded,
                state.paused,
                state.buffering,
                Instant::now(),
            );
            dispatch_state_to_core(
                &state,
                session,
                core,
                discord_rpc,
                media_session,
                ui,
                runtime_handle,
            );
            schedule_ui_state(
                ui,
                state_slot,
                ui_state_scheduler,
                autohide_task,
                runtime_handle,
            );
        }
        PlaybackEvent::FileLoaded => {
            let (load_elapsed_ms, thumbnail_source, pause_restore, pending_subtitles) = {
                let mut session = lock_session(session);
                (
                    session
                        .load_requested_at
                        .map(|started_at| started_at.elapsed().as_millis()),
                    session.url.as_ref().map(|url| ThumbnailSource {
                        generation: session.playback_generation,
                        url: url.clone(),
                        initial_position: session.last_time as f64,
                    }),
                    session.pending_pause_restore.take(),
                    session.on_file_loaded(),
                )
            };
            tracing::info!(?load_elapsed_ms, "MPV file loaded");
            if let Some(source) = thumbnail_source {
                thumbnails.prewarm(source);
            }
            restore_stream_state(core, controller, ui);
            if let Some(controller) = controller.get() {
                send_external_subtitles(controller, pending_subtitles, ui);
            }
            if let (Some(controller), Some(paused)) = (controller.get(), pause_restore) {
                log_command(controller.send(PlaybackCommand::SetPaused(paused)));
            }
            // Binge/autoplay advances episodes with no user interaction, so the
            // UI-side activity hooks never fire to reclaim keyboard focus. Anchor
            // it here: every started file routes through FileLoaded.
            {
                let ui = ui.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui.upgrade() {
                        ui.invoke_focus_app_shortcuts();
                    }
                });
            }
        }
        PlaybackEvent::Ended { reason, error } => {
            tracing::info!(?reason, error = error.as_deref(), "MPV playback ended");
            if reason == EndReason::Eof {
                dispatch_player(core, ActionPlayer::Ended);
                if consume_end_sleep_timer(session) {
                    if let Some(controller) = controller.get() {
                        log_command(controller.send(PlaybackCommand::SetPaused(true)));
                    }
                    queue_player_status(ui, "Sleep timer finished at the episode boundary");
                    let ui = ui.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui.upgrade() {
                            ui.set_player_sleep_timer_label("".into());
                            if ui.get_player_pip_active() {
                                ui.invoke_player_toggle_pip();
                            }
                        }
                    });
                    return;
                }
                let binge = core
                    .model()
                    .ok()
                    .map(|model| model.ctx.profile.settings.binge_watching)
                    .unwrap_or(false);
                let advanced = binge && play_next(core);
                if advanced {
                    let ui = ui.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui.upgrade() {
                            ui.set_player_active_episode_idx(
                                ui.get_player_active_episode_idx() + 1,
                            );
                        }
                    });
                } else {
                    let ui = ui.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui.upgrade()
                            && ui.get_player_pip_active()
                        {
                            ui.invoke_player_toggle_pip();
                        }
                    });
                }
            } else {
                match schedule_automatic_recovery(
                    &read_state(state_slot),
                    session,
                    controller,
                    ui,
                    runtime_handle,
                ) {
                    RecoveryDisposition::Scheduled | RecoveryDisposition::InFlight => {}
                    RecoveryDisposition::Exhausted | RecoveryDisposition::Unavailable => {
                        show_player_error(
                            ui,
                            error.unwrap_or_else(|| {
                                "Playback failed after retrying this source".to_owned()
                            }),
                        );
                    }
                }
            }
        }
        PlaybackEvent::FrameCaptured { request_id, path } => {
            tracing::info!(request_id, path = %path.display(), "frame captured");
            lock_session(session).last_capture_path = Some(path.clone());
            let ui = ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui.upgrade() {
                    ui.set_player_last_capture_path(path.to_string_lossy().into_owned().into());
                    ui.set_player_status_message("Frame captured".into());
                }
            });
        }
        PlaybackEvent::FrameCaptureFailed {
            request_id,
            path,
            message,
        } => {
            tracing::error!(request_id, path = %path.display(), %message, "frame capture failed");
            queue_player_status(ui, format!("Frame capture failed: {message}"));
        }
        PlaybackEvent::ChaptersUpdated(chapters) => {
            tracing::debug!(chapter_count = chapters.len(), "embedded chapters updated");
        }
        PlaybackEvent::HdrStateChanged {
            requested,
            applied,
            content_hdr,
            passthrough_available,
        } => {
            tracing::info!(
                ?requested,
                ?applied,
                content_hdr,
                passthrough_available,
                "HDR state changed"
            );
            let ui = ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui.upgrade() {
                    ui.set_player_hdr_mode(hdr_mode_index(applied));
                    ui.set_player_hdr_content(content_hdr);
                    ui.set_player_hdr_passthrough_available(passthrough_available);
                    if requested == HdrMode::Passthrough && applied != requested {
                        ui.set_player_status_message(
                            "HDR passthrough is unavailable; tone mapping is active".into(),
                        );
                    }
                }
            });
        }
        PlaybackEvent::SpatialAudioChanged {
            requested,
            applied,
            message,
        } => {
            tracing::info!(?requested, ?applied, %message, "spatial audio state changed");
            let fallback = requested != applied;
            let ui = ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui.upgrade() {
                    ui.set_settings_spatial_audio_status(message.clone().into());
                    if fallback {
                        ui.set_player_status_message(message.into());
                    }
                }
            });
        }
        PlaybackEvent::ClientMessage(args) => {
            tracing::debug!(argument_count = args.len(), "MPV client message received");
        }
        PlaybackEvent::VideoShadersConfigured { request_id } => {
            let update = {
                let mut coordinator = lock_shader_coordinator(shader_coordinator);
                coordinator.configured(request_id)
            };
            if let (Some(controller), Some(update)) = (controller.get(), update) {
                tracing::info!(
                    request_id,
                    effective_preset = ?update.projection.active_preset,
                    "MPV video shaders configured"
                );
                dispatch_shader_update(controller, ui, update);
            } else {
                tracing::debug!(request_id, "ignored stale video shader acknowledgement");
            }
        }
        PlaybackEvent::VideoShadersRejected {
            request_id,
            message,
        } => {
            let update = {
                let mut coordinator = lock_shader_coordinator(shader_coordinator);
                coordinator.rejected(request_id, message.clone())
            };
            if let (Some(controller), Some(update)) = (controller.get(), update) {
                tracing::warn!(request_id, %message, "MPV rejected video shader configuration");
                dispatch_shader_update(controller, ui, update);
            } else {
                tracing::debug!(request_id, "ignored stale video shader rejection");
            }
        }
        PlaybackEvent::Warning(error) => tracing::warn!(%error, "MPV command failed"),
        PlaybackEvent::Error(error) => {
            tracing::error!(%error, "MPV playback error");
            match schedule_automatic_recovery(
                &read_state(state_slot),
                session,
                controller,
                ui,
                runtime_handle,
            ) {
                RecoveryDisposition::Scheduled | RecoveryDisposition::InFlight => {}
                RecoveryDisposition::Exhausted | RecoveryDisposition::Unavailable => {
                    show_player_error(ui, error);
                }
            }
        }
        PlaybackEvent::Shutdown => tracing::info!("MPV playback shutdown event received"),
    }
}

fn hdr_mode_index(mode: HdrMode) -> i32 {
    match mode {
        HdrMode::Auto => 0,
        HdrMode::Passthrough => 1,
        HdrMode::ToneMap => 2,
        HdrMode::Disabled => 3,
    }
}

fn format_discord_time(seconds: i64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds / 60) % 60;
    let remaining_seconds = seconds % 60;
    if hours > 0 {
        format!("{:02}:{:02}:{:02}", hours, minutes, remaining_seconds)
    } else {
        format!("{:02}:{:02}", minutes, remaining_seconds)
    }
}

struct TidbRequest {
    video_key: String,
    id_type: &'static str,
    media_id: String,
    season: Option<u32>,
    episode: Option<u32>,
    duration_secs: i64,
}

struct DiscordMedia {
    title: String,
    image: Option<String>,
    /// Next-best cover candidate (e.g. the background) used only by the OS media
    /// session, so a broken poster URL still yields a thumbnail. Discord fetches
    /// `image` remotely itself and does not use this.
    image_fallback: Option<String>,
}

struct CorePlaybackProjection {
    discord_enabled: bool,
    media: Option<DiscordMedia>,
    tidb_request: Option<TidbRequest>,
    resolved_video_hash: Option<Option<String>>,
}

#[cfg_attr(feature = "profiling", hotpath::measure)]
fn project_core_playback_state(
    model: &AppModel,
    duration_secs: i64,
    needs_media_meta: bool,
    needs_discord_media: bool,
    needs_tidb_request: bool,
    needs_video_hash: bool,
) -> CorePlaybackProjection {
    let discord_enabled = model.ctx.profile.settings.discord_rpc_enabled;
    let meta_item = model
        .player
        .meta_item
        .as_ref()
        .and_then(|meta_item| meta_item.content.as_ref().and_then(Loadable::ready));

    let tidb_request = if needs_tidb_request {
        meta_item.map(|meta_item| {
            let season = model
                .player
                .series_info
                .as_ref()
                .map(|series| series.season);
            let episode = model
                .player
                .series_info
                .as_ref()
                .map(|series| series.episode);
            let source_id = meta_item.preview.id.as_str();
            let (id_type, media_id) = if source_id.starts_with("tt") {
                ("imdb_id", source_id.to_owned())
            } else if let Some(stripped) = source_id.strip_prefix("tmdb:") {
                ("tmdb_id", stripped.to_owned())
            } else {
                ("tmdb_id", source_id.to_owned())
            };
            TidbRequest {
                video_key: format!(
                    "{}:{}:{}:{duration_secs}",
                    source_id,
                    season.unwrap_or_default(),
                    episode.unwrap_or_default()
                ),
                id_type,
                media_id,
                season,
                episode,
                duration_secs,
            }
        })
    } else {
        None
    };

    // Built for the OS media session whenever its metadata is stale, and for
    // Discord on its own cadence. Gating the Discord case on `discord_enabled`
    // here keeps this off the hot path when Discord is disabled.
    let media = (needs_media_meta || (needs_discord_media && discord_enabled)).then(|| {
        let title = model
            .player
            .selected
            .as_ref()
            .and_then(|selected| {
                meta_item
                    .zip(selected.stream_request.as_ref())
                    .map(|(meta_item, stream_request)| {
                        match meta_item
                            .videos
                            .iter()
                            .find(|video| video.id == stream_request.path.id)
                        {
                            Some(video)
                                if meta_item.preview.behavior_hints.default_video_id.is_none() =>
                            {
                                match &video.series_info {
                                    Some(series_info) => format!(
                                        "{} - {} ({}x{})",
                                        meta_item.preview.name,
                                        video.title,
                                        series_info.season,
                                        series_info.episode
                                    ),
                                    None => format!("{} - {}", meta_item.preview.name, video.title),
                                }
                            }
                            _ => meta_item.preview.name.to_owned(),
                        }
                    })
                    .or_else(|| selected.stream.name.to_owned())
            })
            .unwrap_or_else(|| "Unknown".to_owned());
        // Ordered cover candidates: poster first, then background, then the
        // library poster. `image` is the primary; the OS media session falls
        // through to the next when an earlier URL cannot be fetched or decoded.
        let library_poster = model
            .player
            .selected
            .as_ref()
            .and_then(|selected| selected.stream_request.as_ref())
            .and_then(|req| model.ctx.library.items.get(&req.path.id))
            .and_then(|lib_item| lib_item.poster.as_ref().map(ToString::to_string));
        let mut candidates: Vec<String> = Vec::new();
        for candidate in [
            meta_item
                .and_then(|meta_item| meta_item.preview.poster.as_ref().map(ToString::to_string)),
            meta_item.and_then(|meta_item| {
                meta_item
                    .preview
                    .background
                    .as_ref()
                    .map(ToString::to_string)
            }),
            library_poster,
        ]
        .into_iter()
        .flatten()
        {
            if !candidate.is_empty() && !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
        DiscordMedia {
            title,
            image: candidates.first().cloned(),
            image_fallback: candidates.get(1).cloned(),
        }
    });

    let resolved_video_hash = needs_video_hash
        .then(|| {
            model
                .player
                .stream
                .as_ref()
                .and_then(Loadable::ready)
                .map(|(_, stream)| stream.behavior_hints.video_hash.clone())
        })
        .flatten();

    CorePlaybackProjection {
        discord_enabled,
        media,
        tidb_request,
        resolved_video_hash,
    }
}

fn dispatch_state_to_core(
    state: &PlaybackState,
    session: &Arc<Mutex<SessionState>>,
    core: &Arc<Runtime<DesktopEnv, AppModel>>,
    discord_rpc: &Arc<crate::discord::DiscordRpc>,
    media_session: &Arc<crate::media_session::MediaSession>,
    ui: &slint::Weak<MainWindow>,
    runtime_handle: &tokio::runtime::Handle,
) {
    let now = Instant::now();
    let current_time_secs = state.time.round().max(0.0) as i64;
    let duration_secs = state.duration.round().max(0.0) as i64;
    let (needs_tidb_request, needs_discord_media, needs_media_meta, needs_video_hash) = {
        let current = lock_session(session);
        (
            state.loaded && duration_secs > 0 && current.tidb_fetched_id.is_none(),
            state.loaded
                && (current
                    .last_discord_projection_at
                    .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(5))
                    || current.last_discord_paused != Some(state.paused)),
            state.loaded,
            !current.video_hash_resolved,
        )
    };

    let core_projection = core.model().ok().map(|model| {
        project_core_playback_state(
            &model,
            duration_secs,
            needs_media_meta,
            needs_discord_media,
            needs_tidb_request,
            needs_video_hash,
        )
    });

    if let Some(request) = core_projection
        .as_ref()
        .and_then(|projection| projection.tidb_request.as_ref())
    {
        let fetch_generation = {
            let mut current = lock_session(session);
            if current.tidb_fetched_id.is_some() {
                None
            } else {
                if let Some(task) = current.tidb_task.take() {
                    task.abort();
                }
                current.tidb_fetched_id = Some(request.video_key.clone());
                current.tidb_segments.clear();
                Some(current.playback_generation)
            }
        };
        if let Some(fetch_generation) = fetch_generation {
            let expected_id = request.video_key.clone();
            let session_clone = session.clone();
            let fetch_task = crate::theintrodb::fetch_segments(
                runtime_handle,
                crate::secure_settings::tidb_api_key(),
                request.id_type,
                request.media_id.clone(),
                request.season,
                request.episode,
                request.duration_secs,
                move |segments| {
                    let mut current = lock_session(&session_clone);
                    if current.playback_generation != fetch_generation
                        || current.tidb_fetched_id.as_deref() != Some(expected_id.as_str())
                    {
                        tracing::debug!("ignored stale TheIntroDB response");
                        return;
                    }
                    current.tidb_segments = segments;
                    current.tidb_task = None;
                },
            );
            let mut current = lock_session(session);
            if current.playback_generation == fetch_generation
                && current.tidb_fetched_id.as_deref() == Some(request.video_key.as_str())
            {
                current.tidb_task = Some(fetch_task);
            } else {
                fetch_task.abort();
            }
        }
    }

    let discord_enabled = core_projection
        .as_ref()
        .map(|projection| projection.discord_enabled)
        .unwrap_or_else(|| lock_session(session).last_discord_enabled.unwrap_or(false));
    let discord_connection_change = {
        let mut current = lock_session(session);
        if current.last_discord_enabled == Some(discord_enabled) {
            None
        } else {
            current.last_discord_enabled = Some(discord_enabled);
            if !discord_enabled {
                current.last_discord_activity = None;
                current.last_discord_projection_at = None;
                current.last_discord_paused = None;
            }
            Some(discord_enabled)
        }
    };
    match discord_connection_change {
        Some(true) => {
            let _ = discord_rpc.connect();
        }
        Some(false) => {
            let _ = discord_rpc.disconnect();
        }
        None => {}
    }

    if discord_enabled && state.loaded {
        if let Some(media) = core_projection
            .as_ref()
            .and_then(|projection| projection.media.as_ref())
        {
            let discord_state = if state.paused {
                if duration_secs > 0 {
                    format!(
                        "Paused at {} / {}",
                        format_discord_time(current_time_secs),
                        format_discord_time(duration_secs)
                    )
                } else {
                    "Paused".to_owned()
                }
            } else {
                "Watching".to_owned()
            };
            let (start_timestamp, end_timestamp) = if state.paused {
                (None, None)
            } else {
                let now_unix = chrono::Utc::now().timestamp();
                (
                    Some(now_unix - current_time_secs),
                    (duration_secs > 0).then_some(now_unix + (duration_secs - current_time_secs)),
                )
            };
            let activity = DiscordActivity {
                state: discord_state,
                details: media.title.clone(),
                image: media.image.clone(),
                start_timestamp,
                end_timestamp,
            };
            let activity_changed = {
                let mut current = lock_session(session);
                current.last_discord_projection_at = Some(now);
                current.last_discord_paused = Some(state.paused);
                let changed = current.last_discord_activity.as_ref() != Some(&activity);
                if changed {
                    current.last_discord_activity = Some(activity.clone());
                }
                changed
            };
            if activity_changed {
                let _ = discord_rpc.set_activity(
                    &activity.state,
                    &activity.details,
                    activity.image.as_deref(),
                    activity.start_timestamp,
                    activity.end_timestamp,
                );
            }
        }
    } else if !state.loaded {
        let should_clear = {
            let mut current = lock_session(session);
            let changed = current.last_discord_activity.take().is_some();
            current.last_discord_projection_at = None;
            current.last_discord_paused = None;
            changed
        };
        if should_clear {
            let _ = discord_rpc.clear_activity();
        }
    }

    // OS media session (system controls + wake lock). Independent of Discord:
    // reflects playback whether or not Discord presence is enabled.
    if state.loaded {
        if let Some(media) = core_projection
            .as_ref()
            .and_then(|projection| projection.media.as_ref())
        {
            let should_update = {
                let mut current = lock_session(session);
                let key = (
                    current.playback_generation,
                    duration_secs,
                    media.title.clone(),
                    media.image.clone(),
                );
                if current.last_media_meta_key.as_ref() != Some(&key) {
                    current.last_media_meta_key = Some(key);
                    true
                } else {
                    false
                }
            };
            if should_update {
                media_session.set_metadata(
                    &media.title,
                    media.image.as_deref(),
                    media.image_fallback.as_deref(),
                    duration_secs,
                );
            }
        }

        let playing = !state.paused;
        let push_playback = {
            let mut current = lock_session(session);
            let changed = current.last_media_playing != Some(playing)
                || current
                    .last_media_push_at
                    .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1));
            if changed {
                current.last_media_playing = Some(playing);
                current.last_media_push_at = Some(now);
            }
            changed
        };
        if push_playback {
            media_session.set_playback(playing, current_time_secs);
            crate::taskbar_media::set_state(if playing {
                crate::taskbar_media::ButtonState::Playing
            } else {
                crate::taskbar_media::ButtonState::Paused
            });
            crate::taskbar_media::set_progress(current_time_secs, duration_secs);
        }
    } else {
        let should_clear = {
            let mut current = lock_session(session);
            let changed = current.last_media_meta_key.take().is_some()
                || current.last_media_playing.take().is_some();
            current.last_media_push_at = None;
            changed
        };
        if should_clear {
            media_session.clear();
            crate::taskbar_media::set_state(crate::taskbar_media::ButtonState::Hidden);
        }
    }

    let mut paused_action = None;
    let mut time_action = None;
    let mut video_params_action = None;
    let skip_button_state;
    {
        let mut current = lock_session(session);
        if let Some(resolved_hash) = core_projection
            .as_ref()
            .and_then(|projection| projection.resolved_video_hash.as_ref())
        {
            current.video_hash_resolved = true;
            current.cached_video_hash = resolved_hash.clone();
        }

        if current.last_paused != Some(state.paused) {
            current.last_paused = Some(state.paused);
            paused_action = Some(ActionPlayer::PausedChanged {
                paused: state.paused,
            });
        }

        let time = current_time_secs.max(0) as u64;
        if state.loaded
            && !state.seeking
            && time >= current.last_time
            && current
                .last_time_dispatch
                .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1))
        {
            current.last_time = time;
            current.last_time_dispatch = Some(now);
            time_action = Some(ActionPlayer::TimeChanged {
                time,
                duration: duration_secs.max(0) as u64,
                device: PLAYER_DEVICE.to_owned(),
            });
        }

        let params_changed = current.last_video_params.as_ref().is_none_or(|previous| {
            previous.hash.as_deref() != current.cached_video_hash.as_deref()
                || previous.size != state.file_size
                || previous.filename.as_deref() != state.filename.as_deref()
        });
        if params_changed
            && (current.cached_video_hash.is_some()
                || state.file_size.is_some()
                || state.filename.is_some())
        {
            let params = VideoParams {
                hash: current.cached_video_hash.clone(),
                size: state.file_size,
                filename: state.filename.clone(),
            };
            current.last_video_params = Some(params.clone());
            video_params_action = Some(ActionPlayer::VideoParamsChanged {
                video_params: Some(params),
            });
        }

        let next_skip_button_state = if state.loaded {
            crate::theintrodb::check_active_segment(state.time, &current.tidb_segments)
                .map(|segment| {
                    crate::config::with_config(|config| match segment.segment_type.as_str() {
                        "intro" if config.tidb_show_intro => SkipButtonState::Intro,
                        "recap" if config.tidb_show_recap => SkipButtonState::Recap,
                        "credits" if config.tidb_show_credits => SkipButtonState::Credits,
                        "preview" if config.tidb_show_preview => SkipButtonState::Preview,
                        _ => SkipButtonState::Hidden,
                    })
                })
                .unwrap_or_else(|| {
                    embedded_chapter_skip(state)
                        .map(|(kind, _)| kind)
                        .unwrap_or(SkipButtonState::Hidden)
                })
        } else {
            SkipButtonState::Hidden
        };
        skip_button_state = (current.last_skip_button_state != Some(next_skip_button_state))
            .then_some(next_skip_button_state);
        if skip_button_state.is_some() {
            current.last_skip_button_state = Some(next_skip_button_state);
        }
    }

    if let Some(action) = paused_action {
        dispatch_player(core, action);
    }
    if let Some(action) = time_action {
        dispatch_player(core, action);
    }
    if let Some(action) = video_params_action {
        dispatch_player(core, action);
    }
    if let Some(skip_button_state) = skip_button_state {
        let ui = ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                ui.set_player_show_skip_button(skip_button_state.is_visible());
                ui.set_player_skip_button_label(skip_button_state.label().into());
            }
        });
    }
}

fn expand_language_code(code: &str) -> String {
    let code_lower = code.to_lowercase();
    match code_lower.as_str() {
        "eng" | "en" | "english" => "eng,en,english".to_string(),
        "fre" | "fra" | "fr" | "french" => "fre,fra,fr,french".to_string(),
        "ger" | "deu" | "de" | "german" => "ger,deu,de,german".to_string(),
        "spa" | "es" | "spanish" => "spa,es,spanish".to_string(),
        "ita" | "it" | "italian" => "ita,it,italian".to_string(),
        "por" | "pt" | "portuguese" => "por,pt,portuguese".to_string(),
        "rus" | "ru" | "russian" => "rus,ru,russian".to_string(),
        "zho" | "chi" | "zh" | "chinese" => "zho,chi,zh,chinese".to_string(),
        "hin" | "hi" | "hindi" => "hin,hi,hindi".to_string(),
        "mal" | "ml" | "malayalam" => "mal,ml,malayalam".to_string(),
        "tam" | "ta" | "tamil" => "tam,ta,tamil".to_string(),
        "tel" | "te" | "telugu" => "tel,te,telugu".to_string(),
        "jpn" | "ja" | "japanese" => "jpn,ja,japanese".to_string(),
        "kor" | "ko" | "korean" => "kor,ko,korean".to_string(),
        other => {
            if other.len() == 3 {
                format!("{other},{}", &other[..2])
            } else {
                other.to_string()
            }
        }
    }
}

fn restore_stream_state(
    core: &Arc<Runtime<DesktopEnv, AppModel>>,
    controller: &Arc<OnceLock<PlaybackController>>,
    ui: &slint::Weak<MainWindow>,
) {
    let Some(controller) = controller.get() else {
        return;
    };
    let model = core.model().ok();
    let settings = model.as_ref().map(|m| m.ctx.profile.settings.clone());
    let stream_state = model.and_then(|m| m.player.stream_state.clone());

    if let Some(ref settings) = settings {
        if let Some(ref audio_lang) = settings.audio_language {
            let alang_expanded = expand_language_code(audio_lang);
            log_command(controller.send(PlaybackCommand::SetAudioLanguage(alang_expanded)));
        }
        if let Some(ref sub_lang) = settings.subtitles_language
            && sub_lang != "none"
            && !sub_lang.is_empty()
        {
            let slang_expanded = expand_language_code(sub_lang);
            log_command(controller.send(PlaybackCommand::SetSubtitleLanguage(slang_expanded)));
        }
    }

    if let Some(ref stream_state) = stream_state {
        if let Some(speed) = stream_state.playback_speed {
            log_command(controller.send(PlaybackCommand::SetSpeed(f64::from(speed))));
        }
        if let Some(ref audio) = stream_state.audio_track {
            log_command(controller.send(PlaybackCommand::SetAudioTrack(Some(audio.id.clone()))));
        } else {
            log_command(controller.send(PlaybackCommand::SetAudioTrack(Some("auto".to_string()))));
        }
        if let Some(ref subtitle) = stream_state.subtitle_track {
            log_command(
                controller.send(PlaybackCommand::SetSubtitleTrack(Some(subtitle.id.clone()))),
            );
        } else if let Some(ref settings) = settings {
            if !settings.subtitles_auto_select
                || settings.subtitles_language.as_deref() == Some("none")
            {
                log_command(controller.send(PlaybackCommand::SetSubtitleTrack(None)));
            } else {
                log_command(
                    controller.send(PlaybackCommand::SetSubtitleTrack(Some("auto".to_string()))),
                );
            }
        }
        if let Some(delay) = stream_state.subtitle_delay {
            log_command(controller.send(PlaybackCommand::SetSubtitleDelay(delay)));
        }
        if let Some(scale) = stream_state.subtitle_size {
            log_command(
                controller.send(PlaybackCommand::SetSubtitleScale(f64::from(scale) / 100.0)),
            );
        }
        if let Some(offset) = stream_state.subtitle_offset {
            log_command(
                controller.send(PlaybackCommand::SetSubtitlePosition(f64::from(
                    100.0 - offset.clamp(0.0, 100.0),
                ))),
            );
        }
        if let Some(delay) = stream_state.audio_delay {
            log_command(controller.send(PlaybackCommand::SetAudioDelay(delay)));
        }
    } else {
        // No previous stream state exists: apply core default language preferences directly
        log_command(controller.send(PlaybackCommand::SetAudioTrack(Some("auto".to_string()))));
        if let Some(ref settings) = settings {
            if !settings.subtitles_auto_select
                || settings.subtitles_language.as_deref() == Some("none")
            {
                log_command(controller.send(PlaybackCommand::SetSubtitleTrack(None)));
            } else {
                log_command(
                    controller.send(PlaybackCommand::SetSubtitleTrack(Some("auto".to_string()))),
                );
            }
        }
    }

    let weak = ui.clone();
    let subtitle_delay = stream_state
        .as_ref()
        .and_then(|s| s.subtitle_delay)
        .unwrap_or_default() as f32
        / 1_000.0;
    let subtitle_size = stream_state
        .as_ref()
        .and_then(|s| s.subtitle_size)
        .unwrap_or(100.0);
    let subtitle_offset = stream_state
        .as_ref()
        .and_then(|s| s.subtitle_offset)
        .unwrap_or(100.0);
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_player_subtitle_delay_seconds(subtitle_delay);
            ui.set_player_subtitle_size_percent(subtitle_size);
            ui.set_player_subtitle_offset_percent(subtitle_offset);
        }
    });
}

fn schedule_ui_state(
    ui: &slint::Weak<MainWindow>,
    state: &SharedPlaybackState,
    scheduler: &Arc<UiStateScheduler>,
    autohide_task: &Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    runtime_handle: &tokio::runtime::Handle,
) {
    scheduler.generation.fetch_add(1, Ordering::AcqRel);
    enqueue_ui_state(ui, state, scheduler, autohide_task, runtime_handle);
}

fn enqueue_ui_state(
    ui: &slint::Weak<MainWindow>,
    state: &SharedPlaybackState,
    scheduler: &Arc<UiStateScheduler>,
    autohide_task: &Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    runtime_handle: &tokio::runtime::Handle,
) {
    if scheduler.pending.swap(true, Ordering::AcqRel) {
        return;
    }
    let ui = ui.clone();
    let state = state.clone();
    let scheduler = scheduler.clone();
    let failed_scheduler = scheduler.clone();
    let autohide_task = autohide_task.clone();
    let runtime_handle = runtime_handle.clone();
    let result = slint::invoke_from_event_loop(move || {
        let applied_generation = scheduler.generation.load(Ordering::Acquire);
        let snapshot = read_state(&state).clone();
        if let Some(ui) = ui.upgrade() {
            let mut projection = scheduler
                .projection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            apply_state_to_ui(
                &ui,
                snapshot,
                &mut projection,
                &autohide_task,
                &runtime_handle,
            );
        }
        scheduler.pending.store(false, Ordering::Release);
        if scheduler.generation.load(Ordering::Acquire) != applied_generation {
            enqueue_ui_state(&ui, &state, &scheduler, &autohide_task, &runtime_handle);
        }
    });
    if let Err(error) = result {
        failed_scheduler.pending.store(false, Ordering::Release);
        tracing::error!(%error, "could not enqueue MPV state projection on the Slint event loop");
    }
}

fn apply_state_to_ui(
    ui: &MainWindow,
    state: Arc<PlaybackState>,
    projection: &mut PlayerUiProjectionCache,
    autohide_task: &Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    runtime_handle: &tokio::runtime::Handle,
) {
    let previous = projection.previous.as_deref();
    let was_paused = previous
        .map(|previous| previous.paused)
        .unwrap_or_else(|| ui.get_player_paused());
    let is_paused = state.paused;

    if previous.is_none_or(|previous| previous.loading != state.loading) {
        ui.set_player_loading(state.loading);
    }
    if previous.is_none_or(|previous| previous.buffering != state.buffering) {
        ui.set_player_buffering(state.buffering);
    }
    if previous
        .is_none_or(|previous| previous.cache_buffering_percent != state.cache_buffering_percent)
    {
        ui.set_player_buffering_percent(state.cache_buffering_percent as f32);
    }
    if previous.is_none_or(|previous| previous.paused != is_paused) {
        ui.set_player_paused(is_paused);
    }
    if previous.is_none_or(|previous| previous.volume != state.volume) {
        ui.set_player_volume(state.volume as f32);
    }
    if previous.is_none_or(|previous| previous.muted != state.muted) {
        ui.set_player_muted(state.muted);
    }
    if previous.is_none_or(|previous| previous.speed != state.speed) {
        ui.set_player_playback_speed(state.speed as f32);
    }

    let progress = if state.duration > 0.0 {
        (state.time / state.duration).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    let previous_progress = previous.map(|previous| {
        if previous.duration > 0.0 {
            (previous.time / previous.duration).clamp(0.0, 1.0) as f32
        } else {
            0.0
        }
    });
    if previous_progress != Some(progress) {
        ui.set_player_progress(progress);
    }

    let elapsed_second = state.time.round().max(0.0) as u64;
    if previous.is_none_or(|previous| previous.time.round().max(0.0) as u64 != elapsed_second) {
        ui.set_player_elapsed_time_str(format_time(state.time).into());
    }
    let duration_second = state.duration.round().max(0.0) as u64;
    if previous.is_none_or(|previous| previous.duration.round().max(0.0) as u64 != duration_second)
    {
        ui.set_player_total_time_str(format_time(state.duration).into());
        ui.set_player_duration_seconds(state.duration.max(0.0) as f32);
    }

    let audio_tracks_changed =
        previous.is_none_or(|previous| previous.audio_tracks != state.audio_tracks);
    if audio_tracks_changed {
        let audio_labels = state
            .audio_tracks
            .iter()
            .map(|track| track_label(&track.title, &track.language, &track.codec))
            .map(SharedString::from)
            .collect::<Vec<_>>();
        ui.set_player_audio_tracks(ModelRc::new(VecModel::from(audio_labels)));
        let audio_language_labels = state
            .audio_tracks
            .iter()
            .map(|track| language_label(track.language.as_deref()))
            .map(SharedString::from)
            .collect::<Vec<_>>();
        ui.set_player_audio_track_languages(ModelRc::new(VecModel::from(audio_language_labels)));
        let audio_detail_labels = state
            .audio_tracks
            .iter()
            .map(|track| {
                track
                    .title
                    .as_deref()
                    .or(track.codec.as_deref())
                    .unwrap_or("Audio track")
            })
            .map(SharedString::from)
            .collect::<Vec<_>>();
        ui.set_player_audio_track_labels(ModelRc::new(VecModel::from(audio_detail_labels)));
    }
    if audio_tracks_changed
        || previous.is_none_or(|previous| previous.active_audio_track != state.active_audio_track)
    {
        ui.set_player_active_audio_idx(
            state
                .active_audio_track
                .as_ref()
                .and_then(|active| {
                    state
                        .audio_tracks
                        .iter()
                        .position(|track| &track.id == active)
                })
                .and_then(|index| i32::try_from(index).ok())
                .unwrap_or(-1),
        );
    }

    let subtitle_tracks_changed =
        previous.is_none_or(|previous| previous.subtitle_tracks != state.subtitle_tracks);
    if subtitle_tracks_changed {
        let subtitle_labels = state
            .subtitle_tracks
            .iter()
            .map(subtitle_track_label)
            .map(SharedString::from)
            .collect::<Vec<_>>();
        ui.set_player_subtitles_tracks(ModelRc::new(VecModel::from(subtitle_labels)));
        let subtitle_track_languages = state
            .subtitle_tracks
            .iter()
            .map(|track| language_label(track.language.as_deref()))
            .collect::<Vec<_>>();
        ui.set_player_subtitle_track_languages(ModelRc::new(VecModel::from(
            subtitle_track_languages
                .iter()
                .map(|label| SharedString::from(label.as_str()))
                .collect::<Vec<_>>(),
        )));
        let subtitle_track_origins = state
            .subtitle_tracks
            .iter()
            .map(|track| {
                if !track.external {
                    return "Embedded".to_owned();
                }
                subtitle_origin(track.source_url.as_deref())
                    .unwrap_or_else(|| "External".to_owned())
            })
            .map(SharedString::from)
            .collect::<Vec<_>>();
        ui.set_player_subtitle_track_origins(ModelRc::new(VecModel::from(subtitle_track_origins)));
        let mut subtitle_languages = Vec::<SharedString>::new();
        let mut subtitle_language_track_indices = Vec::<i32>::new();
        for (index, language) in subtitle_track_languages.iter().enumerate() {
            if subtitle_languages
                .iter()
                .any(|existing| existing.as_str() == language)
            {
                continue;
            }
            subtitle_languages.push(language.as_str().into());
            if let Ok(index) = i32::try_from(index) {
                subtitle_language_track_indices.push(index);
            }
        }
        ui.set_player_subtitle_languages(ModelRc::new(VecModel::from(subtitle_languages)));
        ui.set_player_subtitle_language_track_indices(ModelRc::new(VecModel::from(
            subtitle_language_track_indices,
        )));
    }
    if subtitle_tracks_changed
        || previous
            .is_none_or(|previous| previous.active_subtitle_track != state.active_subtitle_track)
    {
        ui.set_player_active_subtitle_idx(
            state
                .active_subtitle_track
                .as_ref()
                .and_then(|active| {
                    state
                        .subtitle_tracks
                        .iter()
                        .position(|track| &track.id == active)
                })
                .and_then(|index| i32::try_from(index).ok())
                .unwrap_or(-1),
        );
    }
    if subtitle_tracks_changed
        || previous.is_none_or(|previous| {
            previous.active_secondary_subtitle_track != state.active_secondary_subtitle_track
        })
    {
        ui.set_player_active_secondary_subtitle_idx(
            state
                .active_secondary_subtitle_track
                .as_ref()
                .and_then(|active| {
                    state
                        .subtitle_tracks
                        .iter()
                        .position(|track| &track.id == active)
                })
                .and_then(|index| i32::try_from(index).ok())
                .unwrap_or(-1),
        );
    }

    if previous.is_none_or(|previous| previous.ab_loop_a != state.ab_loop_a) {
        ui.set_player_ab_loop_a(state.ab_loop_a.unwrap_or(-1.0) as f32);
    }
    if previous.is_none_or(|previous| previous.ab_loop_b != state.ab_loop_b) {
        ui.set_player_ab_loop_b(state.ab_loop_b.unwrap_or(-1.0) as f32);
    }
    if previous.is_none_or(|previous| previous.hdr_mode != state.hdr_mode) {
        ui.set_player_hdr_mode(hdr_mode_index(state.hdr_mode));
    }
    if previous.is_none_or(|previous| previous.hdr_content != state.hdr_content) {
        ui.set_player_hdr_content(state.hdr_content);
    }
    if previous.is_none_or(|previous| {
        previous.hdr_passthrough_available != state.hdr_passthrough_available
    }) {
        ui.set_player_hdr_passthrough_available(state.hdr_passthrough_available);
    }

    if previous.is_none_or(|previous| previous.video_format != state.video_format) {
        ui.set_player_video_format(state.video_format.as_deref().unwrap_or_default().into());
    }
    if previous.is_none_or(|previous| previous.audio_format != state.audio_format) {
        ui.set_player_audio_format(state.audio_format.as_deref().unwrap_or_default().into());
    }
    if previous.is_none_or(|previous| previous.file_format != state.file_format) {
        ui.set_player_file_format(state.file_format.as_deref().unwrap_or_default().into());
    }
    if previous.is_none_or(|previous| previous.hardware_decoder != state.hardware_decoder) {
        ui.set_player_hwdec(state.hardware_decoder.as_deref().unwrap_or_default().into());
    }

    let buffered_percent = if state.duration > 0.0 {
        ((state.buffered_until / state.duration) * 100.0).clamp(0.0, 100.0) as f32
    } else {
        0.0
    };
    let previous_buffered_percent = previous.map(|previous| {
        if previous.duration > 0.0 {
            ((previous.buffered_until / previous.duration) * 100.0).clamp(0.0, 100.0) as f32
        } else {
            0.0
        }
    });
    if previous_buffered_percent != Some(buffered_percent) {
        ui.set_player_buffered_percent(buffered_percent);
    }

    if was_paused && !is_paused {
        reset_autohide_timer(ui, autohide_task, runtime_handle);
    }
    projection.previous = Some(state);
}

fn install_renderer(
    ui: &MainWindow,
    source: RenderSource,
    playback_state: SharedPlaybackState,
    session: Arc<Mutex<SessionState>>,
    controller: PlaybackController,
    shader_coordinator: SharedShaderCoordinator,
) -> anyhow::Result<()> {
    tracing::info!(
        backend = "winit",
        renderer = "skia-opengl",
        "installing MPV renderer"
    );
    let window_weak = ui.as_weak();
    let mut context: Option<RenderContext> = None;
    let mut prewarm_spawned = false;
    let mut render_target_ready = false;
    let mut initial_surface_logged = false;
    let mut last_reported_load = None;
    let mut last_player_visible = None;
    let mut context_initialization_attempted = false;
    let mut missing_context_logged = false;
    let mut allocated_size: Option<(i32, i32)> = None;
    let mut pending_size: Option<(i32, i32)> = None;
    let mut pending_size_since = Instant::now();
    ui.window()
        .set_rendering_notifier(move |state, graphics_api| {
            let is_rendering_setup = matches!(&state, slint::RenderingState::RenderingSetup);
            let is_after_rendering = matches!(&state, slint::RenderingState::AfterRendering);

            if is_rendering_setup {
                tracing::info!(?graphics_api, "Slint rendering setup started");
                context_initialization_attempted = false;
            }

            // Slint's GL context is live once the notifier runs, so this is the
            // safe point to warm the driver on a background thread without
            // blocking the UI thread's own context creation. Creating the MPV
            // context below waits on `gpu_prewarm::is_ready()`.
            if !prewarm_spawned {
                prewarm_spawned = true;
                let redraw_weak = window_weak.clone();
                crate::gpu_prewarm::spawn(move || {
                    let redraw_weak = redraw_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = redraw_weak.upgrade() {
                            ui.window().request_redraw();
                        }
                    });
                });
            }

            // Fast startup deliberately installs MPV after the first window is
            // visible. In that case Slint's one-shot RenderingSetup notification
            // has already occurred, but its OpenGL context is equally current in
            // AfterRendering. Initialize there once, then request a new frame so
            // MPV draws in BeforeRendering and Slint composites the texture in
            // that same frame.
            if context.is_none()
                && !context_initialization_attempted
                && (is_rendering_setup || is_after_rendering)
                && crate::gpu_prewarm::is_ready()
            {
                context_initialization_attempted = true;
                if !is_rendering_setup {
                    tracing::info!(
                        ?graphics_api,
                        "initializing deferred MPV render context after Slint rendering"
                    );
                }
                if let Some(ui) = window_weak.upgrade() {
                    match create_render_context(&source, &window_weak, graphics_api) {
                        Ok(mut render_context) => {
                            let diagnostics = render_context.open_gl_diagnostics();
                            let capability = match diagnostics.video_shader_support() {
                                playback_mpv::VideoShaderSupport::Supported => {
                                    crate::shaders::ShaderContextCapability::Supported
                                }
                                playback_mpv::VideoShaderSupport::Unsupported(reason) => {
                                    crate::shaders::ShaderContextCapability::Unsupported(reason)
                                }
                            };
                            let shader_update = {
                                let mut coordinator = lock_shader_coordinator(&shader_coordinator);
                                coordinator.set_context_capability(capability)
                            };
                            tracing::info!(
                                backend = "winit",
                                renderer = "skia-opengl",
                                profile = ?diagnostics.profile,
                                context_profile = ?diagnostics.context_profile,
                                gl_major = diagnostics.major,
                                gl_minor = diagnostics.minor,
                                shader_support = ?diagnostics.video_shader_support(),
                                desired_preset = ?lock_shader_coordinator(&shader_coordinator)
                                    .desired_preset(),
                                effective_preset = ?shader_update.projection.active_preset,
                                "validated shared OpenGL shader capability"
                            );
                            dispatch_shader_update(&controller, &window_weak, shader_update);
                            match ensure_render_target(&ui, &mut render_context) {
                                Ok(ready) => render_target_ready = ready,
                                Err(error) => tracing::error!(
                                    %error,
                                    "MPV video render target creation failed"
                                ),
                            }
                            let size = ui.window().size();
                            allocated_size = Some((
                                i32::try_from(size.width).unwrap_or(i32::MAX),
                                i32::try_from(size.height).unwrap_or(i32::MAX),
                            ));
                            pending_size = allocated_size;
                            pending_size_since = Instant::now();
                            context = Some(render_context);
                            missing_context_logged = false;
                            tracing::info!("MPV OpenGL render context created");
                            if !is_rendering_setup {
                                ui.window().request_redraw();
                            }
                        }
                        Err(error) => {
                            tracing::error!(%error, "could not create MPV render context")
                        }
                    }
                } else {
                    context_initialization_attempted = false;
                }
            }

            match state {
                slint::RenderingState::RenderingSetup => {}
                slint::RenderingState::BeforeRendering => {
                    let Some(ui) = window_weak.upgrade() else {
                        return;
                    };
                    let player_visible = ui.get_show_player();
                    if last_player_visible != Some(player_visible) {
                        last_player_visible = Some(player_visible);
                        tracing::info!(player_visible, "player render visibility changed");
                    }
                    if context.is_none() && player_visible && !missing_context_logged {
                        missing_context_logged = true;
                        tracing::warn!(
                            "player is visible but the MPV render context is unavailable"
                        );
                    }
                    if !player_visible {
                        if let Some(context) = context.as_mut()
                            && let Err(error) = context.process_updates(false)
                        {
                            tracing::error!(%error, "MPV hidden-frame update processing failed");
                        }
                        return;
                    }

                    if let Some(context) = context.as_mut() {
                        let size = ui.window().size();
                        let requested_size = (
                            i32::try_from(size.width).unwrap_or(i32::MAX),
                            i32::try_from(size.height).unwrap_or(i32::MAX),
                        );
                        if pending_size != Some(requested_size) {
                            pending_size = Some(requested_size);
                            pending_size_since = Instant::now();
                        }
                        let resize_settled =
                            pending_size_since.elapsed() >= Duration::from_millis(100);
                        if !context.has_video_textures()
                            || (allocated_size != Some(requested_size) && resize_settled)
                        {
                            match ensure_render_target_size(&ui, context, requested_size) {
                                Ok(ready) => {
                                    render_target_ready = ready;
                                    allocated_size = Some(requested_size);
                                }
                                Err(error) => tracing::error!(
                                    %error,
                                    "MPV video render target creation failed"
                                ),
                            }
                        }
                    } else {
                        render_target_ready = false;
                    }
                    if !render_target_ready {
                        return;
                    }
                    let size = ui.window().size();
                    if let Some(context) = context.as_mut() {
                        let render_result = context.render();
                        if let Some(code) = context.take_gl_error() {
                            tracing::error!(
                                code = format_args!("{code:#x}"),
                                "OpenGL error after MPV render"
                            );
                        }
                        match render_result {
                            Ok(RenderOutcome::Rendered {
                                texture,
                                frame_ready,
                            }) => {
                                let image = unsafe {
                                    BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                                        texture.texture_id(),
                                        (texture.width(), texture.height()).into(),
                                    )
                                }
                                .origin(BorrowedOpenGLTextureOrigin::TopLeft)
                                .build();
                                ui.set_player_video_frame(image);
                                crate::performance::counters().record_mpv_frame_published();

                                if frame_ready {
                                    ui.set_player_has_video_frame(true);
                                }

                                let playable_frame =
                                    frame_ready && read_state(&playback_state).loaded;

                                if !initial_surface_logged {
                                    initial_surface_logged = true;
                                    tracing::info!(
                                        width = size.width,
                                        height = size.height,
                                        texture_id = texture.texture_id().get(),
                                        "initial MPV video surface submitted to Slint"
                                    );
                                }
                                let load_started_at = playable_frame
                                    .then(|| lock_session(&session).load_requested_at)
                                    .flatten();
                                if load_started_at.is_some()
                                    && load_started_at != last_reported_load
                                {
                                    last_reported_load = load_started_at;
                                    tracing::info!(
                                        width = size.width,
                                        height = size.height,
                                        texture_id = texture.texture_id().get(),
                                        load_to_first_frame_ms = load_started_at
                                            .map(|started_at| started_at.elapsed().as_millis()),
                                        "first post-load MPV video frame submitted to Slint"
                                    );
                                }
                            }
                            Ok(RenderOutcome::NoFrame) => {
                                tracing::trace!("MPV has no new frame to render");
                            }
                            Err(error) => {
                                tracing::error!(
                                    %error,
                                    width = size.width,
                                    height = size.height,
                                    "MPV frame rendering failed"
                                );
                            }
                        }
                    }
                }
                slint::RenderingState::AfterRendering => {}
                slint::RenderingState::RenderingTeardown => {
                    tracing::info!("Slint rendering teardown started");
                    let teardown_window = window_weak.clone();
                    if let Err(error) = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = teardown_window.upgrade() {
                            ui.set_player_video_frame(slint::Image::default());
                            ui.set_player_has_video_frame(false);
                        }
                    }) {
                        tracing::debug!(%error, "could not queue MPV surface teardown update");
                    }
                    context = None;
                    let shader_update = {
                        let mut coordinator = lock_shader_coordinator(&shader_coordinator);
                        coordinator.context_torn_down()
                    };
                    dispatch_shader_update(&controller, &window_weak, shader_update);
                    render_target_ready = false;
                    initial_surface_logged = false;
                    last_reported_load = None;
                    last_player_visible = None;
                    context_initialization_attempted = false;
                    missing_context_logged = false;
                    allocated_size = None;
                    pending_size = None;
                }
                _ => {}
            }
        })
        .map_err(|error| anyhow!("Slint renderer cannot host MPV: {error}"))?;
    // Installing the notifier after first paint must still produce a callback,
    // even when the loading page is otherwise static.
    ui.window().request_redraw();
    Ok(())
}

fn create_render_context(
    source: &RenderSource,
    window_weak: &slint::Weak<MainWindow>,
    graphics_api: &slint::GraphicsAPI<'_>,
) -> anyhow::Result<RenderContext> {
    let slint::GraphicsAPI::NativeOpenGL { get_proc_address } = graphics_api else {
        return Err(anyhow!(
            "MPV requires Slint's NativeOpenGL renderer, got {graphics_api:?}"
        ));
    };
    let redraw_weak = window_weak.clone();
    let render_context = source.create_context(get_proc_address, move || {
        crate::performance::counters().record_mpv_redraw_post();
        let redraw_weak = redraw_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = redraw_weak.upgrade()
                && ui.get_show_player()
            {
                ui.window().request_redraw();
            }
        });
    })?;
    let diagnostics = render_context.open_gl_diagnostics();
    tracing::info!(
        vendor = %diagnostics.vendor,
        renderer = %diagnostics.renderer,
        version = %diagnostics.version,
        gl_major = diagnostics.major,
        gl_minor = diagnostics.minor,
        profile = ?diagnostics.profile,
        context_profile = ?diagnostics.context_profile,
        glsl_version = %diagnostics.shading_language_version,
        shader_support = ?diagnostics.video_shader_support(),
        "MPV is sharing Slint's OpenGL context"
    );
    Ok(render_context)
}

fn ensure_render_target(ui: &MainWindow, context: &mut RenderContext) -> anyhow::Result<bool> {
    let size = ui.window().size();
    let width = i32::try_from(size.width)?;
    let height = i32::try_from(size.height)?;
    ensure_render_target_size(ui, context, (width, height))
}

fn ensure_render_target_size(
    ui: &MainWindow,
    context: &mut RenderContext,
    (width, height): (i32, i32),
) -> anyhow::Result<bool> {
    let start = std::time::Instant::now();
    if context.ensure_video_textures(width, height)? {
        // A previous borrowed image must not outlive targets discarded by a
        // resize. The next completed frame installs the new texture.
        ui.set_player_video_frame(slint::Image::default());
        tracing::info!(
            width,
            height,
            elapsed_ms = start.elapsed().as_millis(),
            "double-buffered MPV video targets created"
        );
    }
    Ok(context.has_video_textures())
}

fn resume_time(player: &Player) -> Option<f64> {
    let selected_video = player
        .selected
        .as_ref()?
        .stream_request
        .as_ref()?
        .path
        .id
        .as_str();
    let item = player.library_item.as_ref()?;
    (item.state.video_id.as_deref() == Some(selected_video))
        .then_some(item.state.time_offset as f64)
}

fn play_next(core: &Arc<Runtime<DesktopEnv, AppModel>>) -> bool {
    let selected = core.model().ok().and_then(|model| {
        let current = model.player.selected.as_ref()?;
        let next_video = model.player.next_video.as_ref()?;
        let next_stream = model.player.next_stream.clone()?;
        let mut stream_request = current.stream_request.clone();
        if let Some(request) = stream_request.as_mut() {
            request.path.id = next_video.id.clone();
        }
        let subtitles_path = current.subtitles_path.as_ref().map(|path| ResourcePath {
            id: next_video.id.clone(),
            ..path.clone()
        });
        Some(Selected {
            stream: next_stream,
            stream_request,
            meta_request: current.meta_request.clone(),
            subtitles_path,
        })
    });
    let Some(selected) = selected else {
        return false;
    };
    dispatch_player(core, ActionPlayer::NextVideo);
    core.dispatch(RuntimeAction {
        field: None,
        action: Action::Load(ActionLoad::Player(Box::new(selected))),
    });
    true
}

fn update_stream_state(
    core: &Arc<Runtime<DesktopEnv, AppModel>>,
    update: impl FnOnce(&mut StreamItemState),
) {
    let mut state = core
        .model()
        .ok()
        .and_then(|model| model.player.stream_state.clone())
        .unwrap_or_default();
    update(&mut state);
    dispatch_player(core, ActionPlayer::StreamStateChanged { state });
}

fn dispatch_player(core: &Arc<Runtime<DesktopEnv, AppModel>>, action: ActionPlayer) {
    core.dispatch(RuntimeAction {
        field: None,
        action: Action::Player(action),
    });
}

fn resolve_config_dir() -> PathBuf {
    crate::paths::get().mpv().to_path_buf()
}

/// Rewrites a YouTube selection back to its watch URL so MPV's `ytdl_hook`
/// resolves it instead of the streaming server.
///
/// The server can only redirect to a single progressive rendition, and YouTube
/// caps those at 360p; the hook merges the separate adaptive video and audio
/// tracks and so reaches the source quality.
fn youtube_watch_url(
    stream: &stremio_core::types::resource::Stream<
        stremio_core::types::streams::ConvertedStreamSource,
    >,
) -> Option<String> {
    match &stream.source {
        stremio_core::types::streams::ConvertedStreamSource::YouTube { yt_id, .. } => {
            Some(format!("https://www.youtube.com/watch?v={yt_id}"))
        }
        _ => None,
    }
}

/// Where MPV's `ytdl_hook` should look for `yt-dlp`.
///
/// The streaming server owns the binary — it downloads and refreshes it — so the
/// path is taken from there rather than guessed, and [`None`] simply means the
/// hook can find `yt-dlp` on `PATH` without help.
fn resolve_ytdl_path() -> Option<PathBuf> {
    stream_server::ytdlp::player_path(crate::paths::get().streaming_server())
}

fn resolve_spatial_audio_sofa_path(config_dir: &std::path::Path) -> Option<PathBuf> {
    std::env::var_os("STREMIO_SOFA_PATH")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(config_dir.join("hrtf").join("default.sofa")))
}

fn format_time(seconds: f64) -> String {
    let total = seconds.round().max(0.0) as u64;
    let hours = total / 3_600;
    let minutes = (total % 3_600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn track_label<'a>(
    title: &'a Option<String>,
    language: &'a Option<String>,
    codec: &'a Option<String>,
) -> &'a str {
    title
        .as_deref()
        .or(language.as_deref())
        .or(codec.as_deref())
        .unwrap_or("Unknown track")
}

/// stremio-web treats a subtitle label as usable only when it is non-empty and
/// is not a URL (`hasValidLabel`), otherwise it shows the language name. Add-on
/// subtitles carry their URL as the label, and that is what MPV reports back as
/// the track title, so without this the menu lists raw links.
fn subtitle_track_label(track: &playback_mpv::SubtitleTrack) -> String {
    track
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty() && !title.starts_with("http"))
        .map_or_else(
            || language_label(track.language.as_deref()),
            ToOwned::to_owned,
        )
}

fn language_label(language: Option<&str>) -> String {
    let raw = language.map(str::trim).filter(|value| !value.is_empty());
    let Some(raw) = raw else {
        return "Unknown".to_owned();
    };
    let normalized = raw
        .split(['-', '_'])
        .next()
        .unwrap_or(raw)
        .to_ascii_lowercase();
    match normalized.as_str() {
        "ar" | "ara" => "Arabic",
        "bg" | "bul" => "Bulgarian",
        "cs" | "ces" | "cze" => "Czech",
        "da" | "dan" => "Danish",
        "de" | "deu" | "ger" => "German",
        "el" | "ell" | "gre" => "Greek",
        "en" | "eng" => "English",
        "es" | "spa" => "Spanish",
        "et" | "est" => "Estonian",
        "fa" | "fas" | "per" => "Persian",
        "fi" | "fin" => "Finnish",
        "fr" | "fra" | "fre" => "French",
        "he" | "heb" => "Hebrew",
        "hi" | "hin" => "Hindi",
        "hr" | "hrv" => "Croatian",
        "hu" | "hun" => "Hungarian",
        "id" | "ind" => "Indonesian",
        "it" | "ita" => "Italian",
        "ja" | "jpn" => "Japanese",
        "ko" | "kor" => "Korean",
        "lt" | "lit" => "Lithuanian",
        "lv" | "lav" => "Latvian",
        "nl" | "nld" | "dut" => "Dutch",
        "no" | "nor" => "Norwegian",
        "pl" | "pol" => "Polish",
        "pt" | "por" => "Portuguese",
        "ro" | "ron" | "rum" => "Romanian",
        "ru" | "rus" => "Russian",
        "sk" | "slk" | "slo" => "Slovak",
        "sl" | "slv" => "Slovenian",
        "sr" | "srp" => "Serbian",
        "sv" | "swe" => "Swedish",
        "th" | "tha" => "Thai",
        "tr" | "tur" => "Turkish",
        "uk" | "ukr" => "Ukrainian",
        "vi" | "vie" => "Vietnamese",
        "zh" | "zho" | "chi" => "Chinese",
        "und" | "unknown" => "Unknown",
        _ if raw.chars().count() > 3 => raw,
        _ => return raw.to_ascii_uppercase(),
    }
    .to_owned()
}

fn send_or_show(
    controller: &PlaybackController,
    command: PlaybackCommand,
    ui: &slint::Weak<MainWindow>,
) {
    if let Err(error) = controller.send(command) {
        show_player_error(ui, error.to_string());
    }
}

fn log_command(result: Result<(), playback_mpv::MpvError>) {
    if let Err(error) = result {
        tracing::error!(%error, "MPV command failed");
    }
}

fn show_player_error(ui: &slint::Weak<MainWindow>, message: String) {
    tracing::error!(error = %message, "player error shown in UI");
    let ui = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui.upgrade() {
            ui.set_player_loading(false);
            ui.set_player_buffering(false);
            ui.set_player_has_video_frame(false);
            ui.set_player_error(message.into());
        }
    });
}

fn lock_session(session: &Mutex<SessionState>) -> std::sync::MutexGuard<'_, SessionState> {
    session
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn cancel_sleep_timer(session: &mut SessionState) {
    if let Some(timer) = session.sleep_timer.take() {
        timer.cancellation.cancel();
    }
    if let Some(task) = session.sleep_task.take() {
        task.abort();
    }
    session.preserve_end_timer_for_next_load = false;
}

fn read_state(
    state: &RwLock<Arc<PlaybackState>>,
) -> std::sync::RwLockReadGuard<'_, Arc<PlaybackState>> {
    state
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_statistics_poll(
    poll: &Mutex<Option<StatisticsPoll>>,
) -> std::sync::MutexGuard<'_, Option<StatisticsPoll>> {
    poll.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn cancel_statistics_poll(poll: &Mutex<Option<StatisticsPoll>>) {
    if let Some(poll) = lock_statistics_poll(poll).take() {
        poll.cancellation.cancel();
    }
}

fn unload_player(
    controller: &PlaybackController,
    core: &Arc<Runtime<DesktopEnv, AppModel>>,
    statistics_poll: &Mutex<Option<StatisticsPoll>>,
    session: &Mutex<SessionState>,
    discord_rpc: &Arc<crate::discord::DiscordRpc>,
    thumbnails: &crate::thumbnail_preview::ThumbnailPreview,
) {
    cancel_statistics_poll(statistics_poll);
    let mut current = lock_session(session);
    if let Some(task) = current.tidb_task.take() {
        task.abort();
    }
    if let Some(task) = current.recovery_task.take() {
        task.abort();
    }
    cancel_sleep_timer(&mut current);
    let next_generation = current.playback_generation.wrapping_add(1);
    *current = SessionState {
        playback_generation: next_generation,
        ..SessionState::default()
    };
    drop(current);
    thumbnails.unload(next_generation);
    let _ = discord_rpc.clear_activity();
    log_command(controller.send(PlaybackCommand::Stop));
    core.dispatch(RuntimeAction {
        field: Some(AppModelField::Player),
        action: Action::Unload,
    });
}

fn lock_autohide_task(
    task: &Mutex<Option<tokio::task::JoinHandle<()>>>,
) -> std::sync::MutexGuard<'_, Option<tokio::task::JoinHandle<()>>> {
    task.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether we have hidden the OS pointer for the player's idle state.
///
/// The cursor is global window state — one main window, one pointer — so a
/// single flag lets both the activity and the autohide-timer paths flip it
/// without threading extra state through the event plumbing. It also coalesces
/// calls: `set_player_cursor_hidden` only touches winit on an actual
/// transition, never on every mouse-move activity tick.
static PLAYER_CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);

/// Show or hide the OS pointer over the video surface.
///
/// Slint's declarative `mouse-cursor` (see `player.slint`) is only pushed to
/// the OS while a pointer event is being dispatched — the core recomputes the
/// cursor exclusively inside `process_mouse_input`. The inactivity timer fires
/// with no pointer event, so the `mouse-cursor: none` binding never reaches the
/// window on its own; we have to drive it here instead.
///
/// winit's `set_cursor_visible` is not free — on Windows it hops to the window
/// thread and blocks on the reply — so skip it whenever the state already
/// matches, and only remember the new state once the window actually applied it.
fn set_player_cursor_hidden(ui: &MainWindow, hidden: bool) {
    if PLAYER_CURSOR_HIDDEN.load(Ordering::Relaxed) == hidden {
        return;
    }
    let applied = ui
        .window()
        .with_winit_window(|window| window.set_cursor_visible(!hidden))
        .is_some();
    if applied {
        PLAYER_CURSOR_HIDDEN.store(hidden, Ordering::Relaxed);
    }
}

fn reset_autohide_timer(
    ui: &MainWindow,
    autohide_task: &Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    runtime_handle: &tokio::runtime::Handle,
) {
    let is_paused = ui.get_player_paused();

    // 1. Ensure controls and the pointer are visible when activity is triggered
    ui.set_player_controls_visible(true);
    set_player_cursor_hidden(ui, false);

    // 2. Abort the previous timer task if any
    if let Some(handle) = lock_autohide_task(autohide_task).take() {
        handle.abort();
    }

    // 3. If playing, spawn a new timer to auto-hide controls after 3 seconds
    if !is_paused {
        let weak_ui = ui.as_weak();
        *lock_autohide_task(autohide_task) = Some(runtime_handle.spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_ui.upgrade()
                    && !ui.get_player_paused()
                    && !ui.get_player_show_subtitles_menu()
                    && !ui.get_player_show_audio_menu()
                    && !ui.get_player_show_speed_menu()
                    && !ui.get_player_show_stats_menu()
                    && !ui.get_player_show_options_menu()
                    && !ui.get_player_show_playlist_drawer()
                    && !ui.get_player_show_context_menu()
                {
                    ui.set_player_controls_visible(false);
                    ui.invoke_player_seek_leave();
                    set_player_cursor_hidden(&ui, true);
                }
            });
        }));
    }
}

#[cfg(test)]
mod player_title_tests {
    use super::format_player_title;
    use stremio_core::{
        models::{
            common::{Loadable, ResourceLoadable},
            player::{Player, Selected},
        },
        types::{
            addon::{ResourcePath, ResourceRequest},
            library::{LibraryBucket, LibraryItem, LibraryItemState},
            resource::{
                MetaItem, MetaItemBehaviorHints, MetaItemPreview, SeriesInfo, Stream, StreamSource,
                Video,
            },
        },
    };
    use url::Url;

    fn mock_stream() -> Stream {
        Stream {
            source: StreamSource::Url {
                url: Url::parse("https://example.com/video.mp4").expect("valid stream URL"),
            },
            name: None,
            description: None,
            thumbnail: None,
            subtitles: vec![],
            behavior_hints: Default::default(),
        }
    }

    fn mock_request(resource: &str, r#type: &str, id: &str) -> ResourceRequest {
        ResourceRequest::new(
            Url::parse("https://example.com/manifest.json").expect("valid manifest URL"),
            ResourcePath::without_extra(resource, r#type, id),
        )
    }

    fn mock_player_with_meta(
        meta_name: &str,
        is_movie: bool,
        videos: Vec<Video>,
        video_id: &str,
    ) -> Player {
        let default_video_id = if is_movie {
            Some(video_id.to_string())
        } else {
            None
        };

        let meta_item = MetaItem {
            preview: MetaItemPreview {
                id: "tt12345".to_string(),
                r#type: if is_movie { "movie" } else { "series" }.to_string(),
                name: meta_name.to_string(),
                poster: None,
                background: None,
                logo: None,
                description: None,
                release_info: None,
                runtime: None,
                released: None,
                poster_shape: Default::default(),
                links: vec![],
                trailer_streams: vec![],
                behavior_hints: MetaItemBehaviorHints {
                    default_video_id,
                    ..Default::default()
                },
            },
            videos,
        };

        let media_type = if is_movie { "movie" } else { "series" };
        let stream_request = mock_request("stream", media_type, video_id);
        let meta_request = mock_request("meta", media_type, "tt12345");

        let selected = Selected {
            stream: mock_stream(),
            stream_request: Some(stream_request),
            meta_request: Some(meta_request.clone()),
            subtitles_path: None,
        };

        Player {
            selected: Some(selected),
            meta_item: Some(ResourceLoadable {
                request: meta_request,
                content: Some(Loadable::Ready(meta_item)),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn movie_title_returns_meta_name() {
        let player = mock_player_with_meta("Tenet", true, vec![], "tt6723592");
        let title = format_player_title(&player, None);
        assert_eq!(title, Some("Tenet".to_string()));
    }

    #[test]
    fn series_episode_formats_with_title_and_season_episode() {
        let video = Video {
            id: "tt12345:1:1".to_string(),
            title: "Pilot".to_string(),
            released: None,
            overview: None,
            thumbnail: None,
            streams: vec![],
            series_info: Some(SeriesInfo {
                season: 1,
                episode: 1,
            }),
            trailer_streams: vec![],
        };
        let player = mock_player_with_meta("Breaking Bad", false, vec![video], "tt12345:1:1");
        let title = format_player_title(&player, None);
        assert_eq!(title, Some("Breaking Bad - Pilot (1x1)".to_string()));
    }

    #[test]
    fn series_episode_without_unique_title_formats_season_episode() {
        let video = Video {
            id: "tt12345:1:2".to_string(),
            title: "Breaking Bad".to_string(),
            released: None,
            overview: None,
            thumbnail: None,
            streams: vec![],
            series_info: Some(SeriesInfo {
                season: 1,
                episode: 2,
            }),
            trailer_streams: vec![],
        };
        let player = mock_player_with_meta("Breaking Bad", false, vec![video], "tt12345:1:2");
        let title = format_player_title(&player, None);
        assert_eq!(title, Some("Breaking Bad (1x2)".to_string()));
    }

    #[test]
    fn fallback_to_library_item_name_when_meta_not_ready() {
        let stream_request = mock_request("stream", "series", "tt999:1:1");
        let selected = Selected {
            stream: mock_stream(),
            stream_request: Some(stream_request),
            meta_request: None,
            subtitles_path: None,
        };
        let player = Player {
            selected: Some(selected),
            ..Default::default()
        };
        let mut library = LibraryBucket::new(None, vec![]);
        library.items.insert(
            "tt999".to_string(),
            LibraryItem {
                id: "tt999".to_string(),
                r#type: "series".to_string(),
                name: "The Mandalorian".to_string(),
                poster: None,
                poster_shape: Default::default(),
                removed: false,
                temp: false,
                ctime: None,
                mtime: chrono::Utc::now(),
                state: LibraryItemState::default(),
                behavior_hints: Default::default(),
            },
        );

        let title = format_player_title(&player, Some(&library));
        assert_eq!(title, Some("The Mandalorian".to_string()));
    }
}
