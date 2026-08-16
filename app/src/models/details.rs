use crate::models::{Fingerprint, SyncFingerprint};
use crate::{AppModel, DetailsPresentation, EpisodeItem, MainWindow, NavigationController};
use core_env::DesktopEnv;
use slint::ComponentHandle;
use std::{
    borrow::Cow,
    sync::{Arc, Mutex, OnceLock},
};
use stremio_core::{
    models::{
        common::Loadable,
        meta_details::{MetaDetails, Selected as DetailsSelected},
    },
    runtime::{
        Runtime, RuntimeAction,
        msg::{Action, ActionCtx, ActionLoad, ActionMetaDetails},
    },
    types::{
        addon::ResourcePath,
        resource::{MetaItem, Video},
    },
};

// Thread-safe caches to track season/episode indexes locally
static ACTIVE_SEASON: OnceLock<Mutex<i32>> = OnceLock::new();
static ACTIVE_EPISODE_IDX: OnceLock<Mutex<usize>> = OnceLock::new();
static EPISODE_SEARCH_QUERY: OnceLock<Mutex<String>> = OnceLock::new();
static LAST_LOADED_ID: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static PENDING_SEASON_SELECTION_ID: OnceLock<Mutex<Option<String>>> = OnceLock::new();
thread_local! {
    static LAST_SYNCED_EPISODES: std::cell::Cell<Option<SyncFingerprint>> = const { std::cell::Cell::new(None) };
}

fn get_active_season() -> &'static Mutex<i32> {
    ACTIVE_SEASON.get_or_init(|| Mutex::new(1))
}

fn get_active_episode_idx() -> &'static Mutex<usize> {
    ACTIVE_EPISODE_IDX.get_or_init(|| Mutex::new(0))
}

fn get_search_query() -> &'static Mutex<String> {
    EPISODE_SEARCH_QUERY.get_or_init(|| Mutex::new(String::new()))
}

fn selected_details_are_ready(model: &AppModel, id: &str) -> bool {
    model
        .meta_details
        .selected
        .as_ref()
        .is_some_and(|selected| selected.meta_path.id == id)
        && model.meta_details.meta_items.iter().any(|resource| {
            matches!(
                &resource.content,
                Some(Loadable::Ready(item)) if item.preview.id == id
            )
        })
}

/// Opens the full details route and immediately reprojects a matching cached
/// selection. Core may intentionally emit no state change for a cached reload,
/// so every navigation entry point must use this instead of waiting for an
/// event that may never arrive.
pub fn open_details_route(
    ui: &MainWindow,
    rt: &Arc<Runtime<DesktopEnv, AppModel>>,
    navigation: &NavigationController,
    id: &str,
) {
    *PENDING_SEASON_SELECTION_ID
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(id.to_owned());
    navigation.dispatch_and_project(
        ui,
        crate::NavigationIntent::OpenDetails {
            media_id: id.to_owned(),
        },
    );

    let ready = rt.model().ok().is_some_and(|model| {
        if !selected_details_are_ready(&model, id) {
            return false;
        }
        let is_in_library = model
            .ctx
            .library
            .items
            .get(id)
            .is_some_and(|item| !item.removed);
        sync(
            ui,
            &model.meta_details,
            is_in_library,
            &ui.as_weak(),
            rt,
            navigation,
        );
        true
    });
    ui.set_details_loading(!ready);
}

pub fn load_meta_details_for_video(
    rt: &Arc<Runtime<DesktopEnv, AppModel>>,
    id: String,
    media_type: Option<String>,
    video_id: Option<String>,
) {
    let r_type = match media_type.filter(|value| !value.is_empty()) {
        Some(media_type) => media_type,
        None => {
            let model = rt.model().expect("model read failed");
            model
                .discover
                .selected
                .as_ref()
                .map(|selected| selected.request.path.r#type.clone())
                .unwrap_or_else(|| "movie".to_string())
        }
    };

    let meta_path = ResourcePath {
        resource: "meta".to_string(),
        r#type: r_type.clone(),
        id: id.clone(),
        extra: vec![],
    };

    rt.dispatch(RuntimeAction {
        field: None,
        action: Action::Load(ActionLoad::MetaDetails(details_selection(
            meta_path, video_id,
        ))),
    });
}

/// Match the web adapter's details route contract: an explicit video opens
/// that video's streams, while a metadata-only route lets Core select the
/// appropriate video after the metadata response arrives.
fn details_selection(meta_path: ResourcePath, video_id: Option<String>) -> DetailsSelected {
    let stream_path = video_id
        .filter(|value| !value.is_empty())
        .map(|id| ResourcePath {
            resource: "stream".to_owned(),
            r#type: meta_path.r#type.clone(),
            id,
            extra: vec![],
        });

    DetailsSelected {
        meta_path,
        stream_path,
        guess_stream: true,
    }
}

pub fn setup(
    ui: &MainWindow,
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
    navigation: &NavigationController,
) {
    let ui_weak = ui.as_weak();

    // Toggle library callback (Add / Remove)
    ui.on_details_toggle_library({
        let runtime = runtime.clone();
        move || {
            let rt = runtime.clone();
            tokio::spawn(async move {
                let model = rt.model().expect("model read failed");
                if let Some(selected) = &model.meta_details.selected {
                    let id = selected.meta_path.id.clone();
                    // Find preview
                    let mut meta_preview = None;
                    for resource in &model.meta_details.meta_items {
                        if let Some(Loadable::Ready(meta_item)) = &resource.content
                            && meta_item.preview.id == id
                        {
                            meta_preview = Some(meta_item.preview.clone());
                            break;
                        }
                    }

                    let is_in_library = model
                        .ctx
                        .library
                        .items
                        .get(&id)
                        .map(|item| !item.removed)
                        .unwrap_or(false);
                    drop(model);

                    if is_in_library {
                        rt.dispatch(RuntimeAction {
                            field: None,
                            action: Action::Ctx(ActionCtx::RemoveFromLibrary(id)),
                        });
                    } else if let Some(preview) = meta_preview {
                        rt.dispatch(RuntimeAction {
                            field: None,
                            action: Action::Ctx(ActionCtx::AddToLibrary(preview)),
                        });
                    }
                }
            });
        }
    });

    ui.on_details_toggle_watched({
        let runtime = runtime.clone();
        move || {
            let is_watched = runtime
                .model()
                .ok()
                .and_then(|model| model.meta_details.library_item.clone())
                .map(|item| item.watched())
                .unwrap_or(false);
            runtime.dispatch(RuntimeAction {
                field: None,
                action: Action::MetaDetails(ActionMetaDetails::MarkAsWatched(!is_watched)),
            });
        }
    });

    ui.on_details_notifications_changed({
        let runtime = runtime.clone();
        move |enabled| {
            let item_id = runtime.model().ok().and_then(|model| {
                model
                    .meta_details
                    .library_item
                    .as_ref()
                    .map(|item| item.id.clone())
            });
            if let Some(item_id) = item_id {
                runtime.dispatch(RuntimeAction {
                    field: None,
                    action: Action::Ctx(ActionCtx::ToggleLibraryItemNotifications(
                        item_id, !enabled,
                    )),
                });
            }
        }
    });

    // Season changed callback
    ui.on_details_season_changed({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        let navigation = navigation.clone();
        move |season| {
            if let Ok(mut s) = get_active_season().lock() {
                *s = season;
            }
            if let Ok(mut ep) = get_active_episode_idx().lock() {
                *ep = 0;
            } // reset episode

            // Season selection only changes the videos list in the web client.
            // Reproject immediately so cached metadata still refreshes episode
            // thumbnails; streams are requested only after an episode click.
            if let Some(ui) = ui_weak.upgrade()
                && let Ok(model) = runtime.model()
            {
                let is_in_library = model
                    .meta_details
                    .selected
                    .as_ref()
                    .is_some_and(|selected| {
                        model
                            .ctx
                            .library
                            .items
                            .get(&selected.meta_path.id)
                            .is_some_and(|item| !item.removed)
                    });
                sync(
                    &ui,
                    &model.meta_details,
                    is_in_library,
                    &ui_weak,
                    &runtime,
                    &navigation,
                );
            }
        }
    });

    ui.on_details_season_step({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        move |direction| {
            let seasons =
                runtime.model().ok().and_then(|model| {
                    let selected_id = model
                        .meta_details
                        .selected
                        .as_ref()
                        .map(|selected| selected.meta_path.id.as_str())?;
                    model.meta_details.meta_items.iter().find_map(|resource| {
                        match &resource.content {
                            Some(Loadable::Ready(meta)) if meta.preview.id == selected_id => {
                                Some(ordered_series_seasons(meta))
                            }
                            _ => None,
                        }
                    })
                });
            let Some(seasons) = seasons else {
                return;
            };
            let current = *get_active_season()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let season = adjacent_series_season(&seasons, current, direction);
            if season != current
                && let Some(ui) = ui_weak.upgrade()
            {
                ui.set_detail_active_season(season);
                ui.invoke_details_season_changed(season);
            }
        }
    });

    // Episode changed callback
    ui.on_details_episode_changed({
        let runtime = runtime.clone();
        move |episode_idx, video_id| {
            if let Ok(mut ep) = get_active_episode_idx().lock() {
                *ep = episode_idx as usize;
            }

            let rt = runtime.clone();
            let video_id = video_id.to_string();
            tokio::spawn(async move {
                reload_stream_for_video(&rt, video_id);
            });
        }
    });

    ui.on_details_manual_episode_selected({
        let runtime = runtime.clone();
        move |season, episode| {
            if episode < 1 || season < 0 {
                return;
            }
            let video_id = runtime.model().ok().and_then(|model| {
                let selected = model.meta_details.selected.as_ref()?;
                let ready_meta = model.meta_details.meta_items.iter().find_map(|resource| {
                    match &resource.content {
                        Some(Loadable::Ready(meta)) if meta.preview.id == selected.meta_path.id => {
                            Some(meta)
                        }
                        _ => None,
                    }
                });
                ready_meta
                    .and_then(|meta| {
                        meta.videos.iter().find_map(|video| {
                            video.series_info.as_ref().and_then(|info| {
                                (info.season as i32 == season && info.episode as i32 == episode)
                                    .then(|| video.id.clone())
                            })
                        })
                    })
                    .or_else(|| Some(format!("{}:{season}:{episode}", selected.meta_path.id)))
            });
            if let Some(video_id) = video_id {
                reload_stream_for_video(&runtime, video_id);
            }
        }
    });

    // Search query changed callback
    ui.on_details_episode_search_changed({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        let navigation = navigation.clone();
        move |q| {
            if let Ok(mut query) = get_search_query().lock() {
                *query = q.to_string();
            }
            // Trigger sync to update the filtered list
            if let Some(ui) = ui_weak.upgrade()
                && let Ok(model) = runtime.model()
            {
                let ui_sync = ui_weak.clone();
                let rt_sync = runtime.clone();
                let is_in_library = model
                    .meta_details
                    .selected
                    .as_ref()
                    .is_some_and(|selected| {
                        model
                            .ctx
                            .library
                            .items
                            .get(&selected.meta_path.id)
                            .is_some_and(|item| !item.removed)
                    });
                sync(
                    &ui,
                    &model.meta_details,
                    is_in_library,
                    &ui_sync,
                    &rt_sync,
                    &navigation,
                );
            }
        }
    });

    // Toggle episode watched callback
    ui.on_details_toggle_episode_watched({
        let runtime = runtime.clone();
        move |video_id| {
            let rt = runtime.clone();
            let video_id = video_id.to_string();
            tokio::spawn(async move {
                let model = rt.model().expect("model read failed");

                // Find the video details
                let mut target_video = None;
                for resource in &model.meta_details.meta_items {
                    if let Some(Loadable::Ready(meta_item)) = &resource.content
                        && let Some(v) = meta_item.videos.iter().find(|video| video.id == video_id)
                    {
                        target_video = Some(v.clone());
                        break;
                    }
                }

                if let Some(video) = target_video {
                    let is_watched = model
                        .meta_details
                        .watched
                        .as_ref()
                        .map(|watched| watched.get_video(&video.id))
                        .unwrap_or(false);
                    drop(model);

                    // Dispatch toggle watched action
                    rt.dispatch(RuntimeAction {
                        field: None,
                        action: Action::MetaDetails(ActionMetaDetails::MarkVideoAsWatched(
                            video,
                            !is_watched,
                        )),
                    });
                }
            });
        }
    });

    // Toggle season watched callback
    ui.on_details_toggle_season_watched({
        let runtime = runtime.clone();
        move |season| {
            let rt = runtime.clone();
            tokio::spawn(async move {
                let model = rt.model().expect("model read failed");
                let all_watched = model.meta_details.selected.as_ref().and_then(|selected| {
                    let meta =
                        model.meta_details.meta_items.iter().find_map(
                            |resource| match &resource.content {
                                Some(Loadable::Ready(item))
                                    if item.preview.id == selected.meta_path.id =>
                                {
                                    Some(item)
                                }
                                _ => None,
                            },
                        )?;
                    let is_target_season = |video: &&Video| {
                        video
                            .series_info
                            .as_ref()
                            .is_some_and(|info| info.season as i32 == season)
                    };
                    let has_videos = meta.videos.iter().any(|video| is_target_season(&video));
                    has_videos.then(|| {
                        model.meta_details.watched.as_ref().is_some_and(|watched| {
                            meta.videos
                                .iter()
                                .filter(is_target_season)
                                .all(|video| watched.get_video(&video.id))
                        })
                    })
                });
                drop(model);

                if let Some(all_watched) = all_watched {
                    rt.dispatch(RuntimeAction {
                        field: None,
                        action: Action::MetaDetails(ActionMetaDetails::MarkSeasonAsWatched(
                            season as u32,
                            !all_watched,
                        )),
                    });
                }
            });
        }
    });

    ui.on_details_share_copy(|share_url| {
        if let Err(error) = arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(share_url.to_string()))
        {
            tracing::warn!(%error, "failed to copy details share link");
        }
    });

    ui.on_details_share_social(|platform, share_url| {
        let base = match platform.as_str() {
            "facebook" => "https://www.facebook.com/sharer/sharer.php",
            "x" => "https://twitter.com/intent/tweet",
            "reddit" => "https://www.reddit.com/submit",
            _ => return,
        };
        let Ok(mut url) = url::Url::parse(base) else {
            return;
        };
        url.query_pairs_mut().append_pair("url", share_url.as_str());
        if let Err(error) = open::that(url.as_str()) {
            tracing::warn!(%error, %platform, "failed to open details share URL");
        }
    });
}

fn reload_stream_for_video(rt: &Arc<Runtime<DesktopEnv, AppModel>>, video_id: String) {
    let model = rt.model().expect("model read failed");
    let meta_path = model
        .meta_details
        .selected
        .as_ref()
        .map(|selected| selected.meta_path.clone())
        .or_else(|| {
            let selected = model.player.selected.as_ref()?;
            let meta_request = selected.meta_request.as_ref()?;
            Some(meta_request.path.clone())
        })
        .or_else(|| {
            let meta_item = model.player.meta_item.as_ref()?.content.as_ref()?.ready()?;
            Some(ResourcePath {
                resource: "meta".to_string(),
                r#type: meta_item.preview.r#type.clone(),
                id: meta_item.preview.id.clone(),
                extra: Vec::new(),
            })
        });
    drop(model);

    if let Some(meta_path) = meta_path {
        rt.dispatch(RuntimeAction {
            field: None,
            action: Action::Load(ActionLoad::MetaDetails(details_selection(
                meta_path,
                Some(video_id),
            ))),
        });
    }
}

pub(crate) fn ordered_series_seasons(meta: &MetaItem) -> Vec<i32> {
    let mut seasons = meta
        .videos
        .iter()
        .filter_map(|video| video.series_info.as_ref().map(|info| info.season as i32))
        .collect::<Vec<_>>();
    seasons.sort_unstable_by_key(|season| if *season == 0 { i32::MAX } else { *season });
    seasons.dedup();
    seasons
}

pub(crate) fn adjacent_series_season(seasons: &[i32], current: i32, direction: i32) -> i32 {
    let current_index = seasons
        .iter()
        .position(|season| *season == current)
        .unwrap_or_default();
    let next_index = if direction < 0 {
        current_index.saturating_sub(1)
    } else {
        (current_index + 1).min(seasons.len().saturating_sub(1))
    };
    seasons.get(next_index).copied().unwrap_or(current)
}

fn preferred_series_season(seasons: &[i32], resumed_season: Option<i32>) -> Option<i32> {
    resumed_season
        .filter(|season| *season != 0 && seasons.contains(season))
        .or_else(|| seasons.iter().copied().find(|season| *season != 0))
        .or_else(|| seasons.first().copied())
}

pub(crate) fn series_videos(meta: &MetaItem, season: i32) -> Vec<&Video> {
    let mut videos = meta
        .videos
        .iter()
        .filter(|video| {
            video
                .series_info
                .as_ref()
                .is_some_and(|info| info.season as i32 == season)
        })
        .collect::<Vec<_>>();
    videos.sort_unstable_by(|left, right| {
        left.series_info
            .as_ref()
            .map(|info| info.episode)
            .unwrap_or_default()
            .cmp(
                &right
                    .series_info
                    .as_ref()
                    .map(|info| info.episode)
                    .unwrap_or_default(),
            )
            .then_with(|| left.id.cmp(&right.id))
    });
    videos
}

fn sync_series_details(ui: &MainWindow, meta_item: Option<&MetaItem>, episodes: &[&Video]) {
    let is_series = ui.get_detail_is_series();
    if is_series && let Some(meta) = meta_item {
        // Get available seasons
        let seasons = ordered_series_seasons(meta);

        let slint_seasons: Vec<i32> = seasons.clone();
        let seasons_model = slint::VecModel::from(slint_seasons);
        ui.set_detail_seasons(slint::ModelRc::new(seasons_model));

        // Default to season 1 if active season not found in list
        let active_season = {
            let mut s = get_active_season()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !seasons.contains(&*s) && !seasons.is_empty() {
                *s = seasons[0];
            }
            *s
        };
        ui.set_detail_active_season(active_season);

        let episode_names: Vec<slint::SharedString> = episodes
            .iter()
            .map(|v| {
                let ep_num = v.series_info.as_ref().map(|info| info.episode).unwrap_or(0);
                slint::SharedString::from(format!("Episode {}: {}", ep_num, v.title))
            })
            .collect();

        let ep_names_model = slint::VecModel::from(episode_names);
        ui.set_detail_episode_names(slint::ModelRc::new(ep_names_model));

        let active_episode_idx = {
            let mut ep = get_active_episode_idx()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *ep >= episodes.len() {
                *ep = 0;
            }
            *ep
        };
        ui.set_detail_active_episode_idx(active_episode_idx as i32);
    }
}

fn category_matches(category: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| category.eq_ignore_ascii_case(candidate))
}

fn projected_links(meta: &MetaItem, categories: &[&str]) -> Vec<slint::SharedString> {
    meta.preview
        .links
        .iter()
        .filter(|link| category_matches(&link.category, categories))
        .map(|link| slint::SharedString::from(&link.name))
        .collect()
}

/// Fingerprint only the details state projected by `sync()`. Stream addon
/// results are deliberately excluded: they update the stream selector, but do
/// not require cloning and rebuilding all metadata, chips, seasons, and
/// episodes for every individual addon response.
pub(crate) fn projection_fingerprint(
    meta_details: &MetaDetails,
    is_in_library: bool,
    presentation: DetailsPresentation,
) -> SyncFingerprint {
    let mut fingerprint = Fingerprint::new();
    fingerprint.bool(is_in_library);
    fingerprint.u64(match presentation {
        DetailsPresentation::Preview => 0,
        DetailsPresentation::Full => 1,
    });

    if let Some(selected) = &meta_details.selected {
        fingerprint.bool(true);
        fingerprint.str(&selected.meta_path.r#type);
        fingerprint.str(&selected.meta_path.id);
        fingerprint.optional_str(selected.stream_path.as_ref().map(|path| path.id.as_str()));
    } else {
        fingerprint.bool(false);
    }

    fingerprint.bool(
        meta_details
            .library_item
            .as_ref()
            .is_some_and(|item| item.watched()),
    );
    fingerprint.bool(
        meta_details
            .library_item
            .as_ref()
            .is_none_or(|item| !item.state.no_notif),
    );
    fingerprint.optional_str(
        meta_details
            .library_item
            .as_ref()
            .and_then(|item| item.state.video_id.as_deref()),
    );
    fingerprint.u64(
        meta_details
            .library_item
            .as_ref()
            .map(|item| item.progress().to_bits())
            .unwrap_or_default(),
    );
    fingerprint.usize(meta_details.meta_items.len());
    for resource in &meta_details.meta_items {
        fingerprint.str(resource.request.base.as_str());
        fingerprint.str(&resource.request.path.r#type);
        fingerprint.str(&resource.request.path.id);
        match &resource.content {
            None => fingerprint.u64(0),
            Some(Loadable::Loading) => fingerprint.u64(1),
            Some(Loadable::Err(_)) => fingerprint.u64(2),
            Some(Loadable::Ready(meta)) => {
                fingerprint.u64(3);
                fingerprint.str(&meta.preview.id);
                fingerprint.str(&meta.preview.name);
                fingerprint.optional_str(meta.preview.logo.as_ref().map(url::Url::as_str));
                fingerprint.optional_str(meta.preview.description.as_deref());
                fingerprint.optional_str(meta.preview.release_info.as_deref());
                fingerprint.optional_str(meta.preview.runtime.as_deref());
                fingerprint.optional_str(meta.preview.poster.as_ref().map(url::Url::as_str));
                fingerprint.optional_str(meta.preview.background.as_ref().map(url::Url::as_str));
                fingerprint.bool(!meta.preview.trailer_streams.is_empty());
                if let Some(released) = meta.preview.released {
                    fingerprint.bool(true);
                    fingerprint.u64(released.timestamp_millis() as u64);
                } else {
                    fingerprint.bool(false);
                }
                for link in &meta.preview.links {
                    fingerprint.str(&link.category);
                    fingerprint.str(&link.name);
                }
                fingerprint.usize(meta.videos.len());
                for video in &meta.videos {
                    fingerprint.str(&video.id);
                    fingerprint.str(&video.title);
                    fingerprint.optional_str(video.thumbnail.as_deref());
                    if let Some(released) = video.released {
                        fingerprint.bool(true);
                        fingerprint.u64(released.timestamp_millis() as u64);
                    } else {
                        fingerprint.bool(false);
                    }
                    if let Some(series_info) = &video.series_info {
                        fingerprint.bool(true);
                        fingerprint.u64(u64::from(series_info.season));
                        fingerprint.u64(u64::from(series_info.episode));
                    } else {
                        fingerprint.bool(false);
                    }
                    fingerprint.bool(
                        meta_details
                            .watched
                            .as_ref()
                            .is_some_and(|watched| watched.get_video(&video.id)),
                    );
                }
            }
        }
    }

    fingerprint.u64(
        *get_active_season()
            .lock()
            .unwrap_or_else(|p| p.into_inner()) as u64,
    );
    fingerprint.usize(
        *get_active_episode_idx()
            .lock()
            .unwrap_or_else(|p| p.into_inner()),
    );
    let query = get_search_query()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    fingerprint.str(&query);
    fingerprint.finish()
}

#[tracing::instrument(skip_all)]
pub fn sync(
    ui: &MainWindow,
    meta_details: &MetaDetails,
    is_in_library: bool,
    ui_weak: &slint::Weak<MainWindow>,
    _runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
    navigation: &NavigationController,
) {
    let Some(selected_id) = meta_details
        .selected
        .as_ref()
        .map(|selected| selected.meta_path.id.as_str())
    else {
        return;
    };
    if let Some(selected) = meta_details.selected.as_ref() {
        let encoded_type: String =
            url::form_urlencoded::byte_serialize(selected.meta_path.r#type.as_bytes()).collect();
        let encoded_id: String =
            url::form_urlencoded::byte_serialize(selected.meta_path.id.as_bytes()).collect();
        ui.set_detail_share_url(
            format!("https://web.stremio.com/#/detail/{encoded_type}/{encoded_id}").into(),
        );
    }
    let Some(presentation) = navigation.details_presentation(selected_id) else {
        tracing::debug!(%selected_id, "discarding metadata for an inactive route");
        return;
    };

    let _span = tracing::info_span!("extract_meta_resource").entered();
    let Some(meta_item) =
        meta_details
            .meta_items
            .iter()
            .find_map(|resource| match &resource.content {
                Some(Loadable::Ready(item)) if item.preview.id == selected_id => Some(item),
                _ => None,
            })
    else {
        return;
    };
    let detail_title = meta_item.preview.name.as_str();
    let detail_description = meta_item.preview.description.as_deref().unwrap_or_default();
    let detail_released_date = meta_item
        .preview
        .release_info
        .as_deref()
        .map(Cow::Borrowed)
        .or_else(|| {
            meta_item
                .preview
                .released
                .map(|date| Cow::Owned(date.format("%Y").to_string()))
        })
        .unwrap_or(Cow::Borrowed(""));
    let detail_runtime = meta_item.preview.runtime.as_deref().unwrap_or_default();
    let detail_rating = meta_item
        .preview
        .links
        .iter()
        .find(|link| category_matches(&link.category, &["imdb"]))
        .map(|link| link.name.as_str())
        .unwrap_or_default();
    drop(_span);
    ui.set_details_loading(false);

    // Keep the underlying details state current even while Discover renders
    // the compact preview. Trailer playback and the Show action both reuse
    // these bindings without forcing a second metadata request.
    ui.set_detail_title(detail_title.into());
    ui.set_detail_description(detail_description.into());
    ui.set_detail_year(detail_released_date.as_ref().into());
    ui.set_detail_runtime(detail_runtime.into());
    ui.set_detail_rating(detail_rating.into());
    ui.set_detail_poster_url(
        meta_item
            .preview
            .poster
            .as_ref()
            .map(url::Url::as_str)
            .unwrap_or_default()
            .into(),
    );
    ui.set_detail_poster(crate::image_cache::get_poster_image(
        &meta_item.preview.poster,
        ui_weak,
    ));
    let logo_url = meta_item
        .preview
        .logo
        .as_ref()
        .map(url::Url::as_str)
        .unwrap_or_default();
    ui.set_detail_logo_url(logo_url.into());
    ui.set_detail_logo(crate::image_cache::get_poster_image(
        &meta_item.preview.logo,
        ui_weak,
    ));
    let background = meta_item
        .preview
        .background
        .as_ref()
        .or(meta_item.preview.poster.as_ref());
    ui.set_detail_background_url(background.map(url::Url::as_str).unwrap_or_default().into());
    ui.set_detail_background(crate::image_cache::get_poster_image_ref(
        background, ui_weak,
    ));
    ui.set_detail_has_trailer(!meta_item.preview.trailer_streams.is_empty());

    if presentation == DetailsPresentation::Preview {
        let _span = tracing::info_span!("sync_preview_panel").entered();
        // User is browsing Discover page and has not clicked Play/Show yet:
        // Update the right-side split screen preview panel instead of navigating!
        ui.set_discover_preview_title(detail_title.into());
        ui.set_discover_preview_description(detail_description.into());
        ui.set_discover_preview_year(detail_released_date.as_ref().into());
        ui.set_discover_preview_rating(detail_rating.into());
        ui.set_discover_preview_runtime(detail_runtime.into());
        ui.set_discover_has_preview(true);

        ui.set_discover_preview_poster_url(
            meta_item
                .preview
                .poster
                .as_ref()
                .map(url::Url::as_str)
                .unwrap_or_default()
                .into(),
        );
        let poster = crate::image_cache::get_poster_image(&meta_item.preview.poster, ui_weak);
        ui.set_discover_preview_poster(poster);
        ui.set_discover_preview_logo_url(logo_url.into());
        let logo = crate::image_cache::get_poster_image(&meta_item.preview.logo, ui_weak);
        ui.set_discover_preview_logo(logo);

        // Sync genres from links
        let slint_genres = projected_links(meta_item, &["genre", "genres"]);
        let genres_model = slint::VecModel::from(slint_genres);
        ui.set_discover_preview_genres(slint::ModelRc::new(genres_model));

        // Sync cast from links
        let cast_names = projected_links(meta_item, &["actor", "actors", "cast"]);
        let cast_model = slint::VecModel::from(cast_names);
        ui.set_discover_preview_cast(slint::ModelRc::new(cast_model));

        let directors = projected_links(meta_item, &["director", "directors"]);
        ui.set_discover_preview_directors(slint::ModelRc::new(slint::VecModel::from(directors)));
        ui.set_discover_preview_has_trailer(!meta_item.preview.trailer_streams.is_empty());

        // Sync Library State for discover preview
        ui.set_discover_preview_is_in_library(is_in_library);
        ui.set_discover_preview_is_watched(
            meta_details
                .library_item
                .as_ref()
                .map(|item| item.watched())
                .unwrap_or(false),
        );
    } else {
        let _span = tracing::info_span!("sync_full_details").entered();
        // Otherwise (Board, Library, or if user clicked play):
        // Navigate to full details view overlay
        ui.set_detail_title(detail_title.into());
        ui.set_detail_description(detail_description.into());
        ui.set_detail_year(detail_released_date.as_ref().into());
        ui.set_detail_runtime(detail_runtime.into());
        ui.set_detail_rating(detail_rating.into());

        let poster = crate::image_cache::get_poster_image(&meta_item.preview.poster, ui_weak);
        ui.set_detail_poster(poster);
        let logo = crate::image_cache::get_poster_image(&meta_item.preview.logo, ui_weak);
        ui.set_detail_logo(logo);

        let genres = projected_links(meta_item, &["genre", "genres"]);
        ui.set_detail_genres(slint::ModelRc::new(slint::VecModel::from(genres)));

        let cast = projected_links(meta_item, &["actor", "actors", "cast"]);
        ui.set_detail_cast(slint::ModelRc::new(slint::VecModel::from(cast)));
        let directors = projected_links(meta_item, &["director", "directors"]);
        ui.set_detail_directors(slint::ModelRc::new(slint::VecModel::from(directors)));
        ui.set_detail_has_trailer(!meta_item.preview.trailer_streams.is_empty());

        // Sync library details
        ui.set_detail_is_in_library(is_in_library);
        ui.set_detail_is_watched(
            meta_details
                .library_item
                .as_ref()
                .map(|item| item.watched())
                .unwrap_or(false),
        );
        ui.set_detail_notifications_enabled(
            meta_details
                .library_item
                .as_ref()
                .is_none_or(|item| !item.state.no_notif),
        );

        let is_series = meta_details
            .selected
            .as_ref()
            .is_some_and(|selected| selected.meta_path.r#type == "series");

        // Reset route-local video state for a newly opened details route, even
        // when Core serves the same metadata from cache and emits no event.
        let id_changed = {
            let mut last_id_guard = LAST_LOADED_ID
                .get_or_init(|| Mutex::new(None))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if last_id_guard.as_deref() != Some(selected_id) {
                *last_id_guard = Some(selected_id.to_owned());
                true
            } else {
                false
            }
        };
        let route_opened = {
            let mut pending = PENDING_SEASON_SELECTION_ID
                .get_or_init(|| Mutex::new(None))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if pending.as_deref() == Some(selected_id) {
                *pending = None;
                true
            } else {
                false
            }
        };

        if id_changed {
            crate::metadata_enrichment::request(ui_weak.clone(), selected_id.to_owned());
            let is_anime = selected_id.starts_with("kitsu:")
                || meta_item.preview.r#type.eq_ignore_ascii_case("anime")
                || meta_item.preview.links.iter().any(|link| {
                    category_matches(&link.category, &["genre", "genres"])
                        && (link.name.eq_ignore_ascii_case("anime")
                            || link.name.eq_ignore_ascii_case("animation"))
                });
            let year = meta_item.preview.release_info.clone().or_else(|| {
                meta_item
                    .preview
                    .released
                    .map(|d| d.format("%Y").to_string())
            });
            crate::ratings::fetch_and_project(
                ui_weak.clone(),
                selected_id.to_owned(),
                meta_item.preview.name.clone(),
                year,
                is_series,
                is_anime,
            );
        }

        if id_changed || route_opened {
            let resumed_season = meta_details
                .library_item
                .as_ref()
                .and_then(|item| item.state.video_id.as_deref())
                .and_then(|video_id| meta_item.videos.iter().find(|video| video.id == video_id))
                .and_then(|video| video.series_info.as_ref())
                .map(|info| info.season as i32);
            let preferred_season = is_series
                .then(|| {
                    preferred_series_season(&ordered_series_seasons(meta_item), resumed_season)
                })
                .flatten()
                .unwrap_or(1);
            *get_active_season()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = preferred_season;
            *get_active_episode_idx()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = 0;
            if let Ok(mut query) = get_search_query().lock() {
                query.clear();
            }
            LAST_SYNCED_EPISODES.with(|cache| {
                cache.set(None);
            });
            ui.set_detail_episode_search_query("".into());
            ui.set_detail_in_stream_view(false);
        }

        // Map EpisodeItem list
        ui.set_detail_is_series(is_series);

        let mut slint_episodes = Vec::new();
        let mut episode_fingerprint = Fingerprint::new();
        let mut videos_for_active_season = Vec::new();
        if is_series {
            let seasons = ordered_series_seasons(meta_item);
            let active_season = {
                let mut season = get_active_season()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if !seasons.contains(&*season) {
                    *season = preferred_series_season(&seasons, None).unwrap_or(1);
                }
                *season
            };
            videos_for_active_season = series_videos(meta_item, active_season);

            let search_query = get_search_query()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .to_lowercase();
            let now = chrono::Utc::now();

            for video in &videos_for_active_season {
                let ep_num = video
                    .series_info
                    .as_ref()
                    .map(|info| info.episode)
                    .unwrap_or(0);
                let title = &video.title;
                let released_str = video
                    .released
                    .map(|dt| dt.format("%b %d, %Y").to_string())
                    .unwrap_or_default();

                if !search_query.is_empty() {
                    let matches_title = title.to_lowercase().contains(&search_query);
                    let matches_num = ep_num.to_string().contains(&search_query);
                    let matches_released = released_str.to_lowercase().contains(&search_query);
                    if !matches_title && !matches_num && !matches_released {
                        continue;
                    }
                }

                let is_watched = meta_details
                    .watched
                    .as_ref()
                    .map(|watched| watched.get_video(&video.id))
                    .unwrap_or(false);

                let is_scheduled = meta_item.preview.behavior_hints.has_scheduled_videos;
                let is_upcoming = is_scheduled
                    && video
                        .released
                        .map(|released| released > now)
                        .unwrap_or(false);
                let progress = meta_details
                    .library_item
                    .as_ref()
                    .filter(|item| item.state.video_id.as_deref() == Some(video.id.as_str()))
                    .map(|item| item.progress() as f32)
                    .unwrap_or_default();

                let thumb_url = video
                    .thumbnail
                    .as_ref()
                    .and_then(|url_str| url::Url::parse(url_str).ok());
                let thumb_img = crate::image_cache::get_poster_image(&thumb_url, ui_weak);

                episode_fingerprint.str(&video.id);
                episode_fingerprint.str(&video.title);
                episode_fingerprint.str(&released_str);
                episode_fingerprint.optional_str(thumb_url.as_ref().map(url::Url::as_str));
                episode_fingerprint.u64(active_season as u64);
                episode_fingerprint.u64(u64::from(ep_num));
                episode_fingerprint.bool(is_upcoming);
                episode_fingerprint.bool(is_watched);
                episode_fingerprint.bool(is_scheduled);
                episode_fingerprint.u64(u64::from(progress.to_bits()));

                slint_episodes.push(EpisodeItem {
                    id: video.id.as_str().into(),
                    title: video.title.as_str().into(),
                    released: released_str.into(),
                    thumbnail_url: thumb_url
                        .as_ref()
                        .map(url::Url::as_str)
                        .unwrap_or_default()
                        .into(),
                    thumbnail: thumb_img,
                    season: active_season,
                    episode_num: ep_num as i32,
                    is_upcoming,
                    is_watched,
                    is_scheduled,
                    progress,
                    can_play: false,
                });
            }
        }

        let episode_fingerprint = episode_fingerprint.finish();
        let episodes_changed = LAST_SYNCED_EPISODES.with(|cache| {
            let changed = cache.get() != Some(episode_fingerprint);
            if changed {
                cache.set(Some(episode_fingerprint));
            }
            changed
        });

        if episodes_changed {
            let episodes_model = slint::VecModel::from(slint_episodes);
            ui.set_detail_episodes(slint::ModelRc::new(episodes_model));
        }

        sync_series_details(ui, Some(meta_item), &videos_for_active_season);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_path() -> ResourcePath {
        ResourcePath {
            resource: "meta".to_owned(),
            r#type: "series".to_owned(),
            id: "series-id".to_owned(),
            extra: vec![],
        }
    }

    #[test]
    fn metadata_route_leaves_stream_selection_to_core() {
        let selected = details_selection(meta_path(), None);

        assert!(selected.stream_path.is_none());
        assert!(selected.guess_stream);
    }

    #[test]
    fn explicit_video_route_preserves_episode_identity() {
        let selected = details_selection(meta_path(), Some("series-id:2:4".to_owned()));
        let stream_path = selected.stream_path.expect("explicit stream path");

        assert_eq!(stream_path.r#type, "series");
        assert_eq!(stream_path.id, "series-id:2:4");
        assert!(selected.guess_stream);
    }

    #[test]
    fn preferred_season_uses_resume_then_first_non_special() {
        let seasons = [1, 2, 3, 0];

        assert_eq!(preferred_series_season(&seasons, Some(3)), Some(3));
        assert_eq!(preferred_series_season(&seasons, Some(9)), Some(1));
        assert_eq!(preferred_series_season(&[0], Some(0)), Some(0));
    }

    #[test]
    fn season_steps_follow_core_order_including_specials() {
        let seasons = [1, 3, 0];

        assert_eq!(adjacent_series_season(&seasons, 1, 1), 3);
        assert_eq!(adjacent_series_season(&seasons, 3, 1), 0);
        assert_eq!(adjacent_series_season(&seasons, 0, 1), 0);
        assert_eq!(adjacent_series_season(&seasons, 1, -1), 1);
    }
}
