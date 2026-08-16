use crate::models::details::{load_meta_details_for_video, open_details_route};
use crate::models::{
    CatalogScope, Fingerprint, SyncFingerprint, catalog_name_index, fingerprint_catalog_projection,
    library_details_video_id, project_catalog_section, queue_visible_catalog,
    spawn_catalog_visibility_loader, sync_fingerprint_changed,
};
use crate::{AppModel, MainWindow, NavigationController};
use crate::{BoardSection, MediaCardItem};
use core_env::DesktopEnv;
use slint::{ComponentHandle, Model};
use std::sync::{Arc, Mutex, OnceLock};

static SECTION_FINGERPRINTS: OnceLock<Mutex<Vec<SyncFingerprint>>> = OnceLock::new();
use stremio_core::{
    models::{
        catalogs_with_extra::CatalogsWithExtra, continue_watching_preview::ContinueWatchingPreview,
    },
    runtime::{
        Runtime, RuntimeAction,
        msg::{Action, ActionCtx, ActionLoad},
    },
    types::addon::Descriptor,
    types::streams::StreamsItemKey,
};
use url::Url;

static LAST_SYNC_STATE: OnceLock<Mutex<Option<SyncFingerprint>>> = OnceLock::new();

pub fn setup(
    ui: &MainWindow,
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
    navigation: &NavigationController,
) {
    let ui_weak = ui.as_weak();
    let catalog_loader = spawn_catalog_visibility_loader(runtime, CatalogScope::Board);

    ui.on_board_catalog_visible(move |index| {
        queue_visible_catalog(&catalog_loader, index);
    });

    ui.on_board_item_selected({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        let navigation = navigation.clone();
        move |id, media_type, video_id| {
            let id = id.to_string();
            if let Some(ui) = ui_weak.upgrade() {
                open_details_route(&ui, &runtime, &navigation, &id);
            }
            let media_type = media_type.to_string();
            let video_id = (!video_id.is_empty()).then(|| video_id.to_string());
            load_meta_details_for_video(&runtime, id, Some(media_type), video_id);
        }
    });

    ui.on_remove_continue_watching({
        let runtime = runtime.clone();
        move |id| {
            let rt = runtime.clone();
            let id = id.to_string();
            tokio::spawn(async move {
                rt.dispatch(RuntimeAction {
                    field: None,
                    action: Action::Ctx(ActionCtx::RewindLibraryItem(id.clone())),
                });
                rt.dispatch(RuntimeAction {
                    field: None,
                    action: Action::Ctx(ActionCtx::DismissNotificationItem(id)),
                });
            });
        }
    });

    ui.on_board_see_all_continue_watching({
        let ui_weak = ui_weak.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.invoke_tab_changed(2);
            }
        }
    });

    ui.on_board_see_all_clicked({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        move |catalog_index| {
            let Ok(catalog_index) = usize::try_from(catalog_index) else {
                return;
            };
            let request = runtime.model().ok().and_then(|model| {
                model
                    .board
                    .catalogs
                    .get(catalog_index)
                    .and_then(|catalog| catalog.first())
                    .map(|page| page.request.clone())
            });
            let Some(request) = request else {
                tracing::warn!(catalog_index, "cannot navigate to a missing Board catalog");
                return;
            };
            runtime.dispatch(RuntimeAction {
                field: None,
                action: Action::Load(ActionLoad::CatalogWithFilters(Some(
                    stremio_core::models::catalog_with_filters::Selected { request },
                ))),
            });
            if let Some(ui) = ui_weak.upgrade() {
                ui.invoke_tab_changed(1);
            }
        }
    });

    ui.on_board_play_clicked({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        let navigation = navigation.clone();
        move |id| {
            let id = id.to_string();
            // Extract needed data while holding the model read lock, then drop
            // the lock before dispatching (which acquires a write lock).
            let play_info = runtime.model().ok().and_then(|model| {
                model
                    .continue_watching_preview
                    .items
                    .iter()
                    .find(|i| i.library_item.id == id)
                    .map(|item| {
                        let video_id = library_details_video_id(
                            item.library_item.state.video_id.as_deref(),
                            item.library_item.state.time_offset,
                            item.library_item.behavior_hints.default_video_id.as_deref(),
                            false,
                        );
                        let saved_stream = item
                            .library_item
                            .state
                            .video_id
                            .as_ref()
                            .and_then(|video_id| {
                                model.ctx.streams.items.get(&StreamsItemKey {
                                    meta_id: item.library_item.id.clone(),
                                    video_id: video_id.clone(),
                                })
                            })
                            .cloned();
                        (
                            item.library_item.r#type.clone(),
                            video_id.map(String::from),
                            saved_stream,
                        )
                    })
            });
            // Read lock is now dropped — safe to dispatch and navigate.
            if let Some((_media_type, _video_id, Some(saved_stream))) = play_info.as_ref() {
                if let Some(ui) = ui_weak.upgrade() {
                    crate::deep_link::open_saved_stream(
                        &ui,
                        &runtime,
                        &navigation,
                        saved_stream.clone(),
                    );
                }
                return;
            }
            if let Some((media_type, video_id, _saved_stream)) = play_info {
                load_meta_details_for_video(&runtime, id.clone(), Some(media_type), video_id);
            }
            if let Some(ui) = ui_weak.upgrade() {
                open_details_route(&ui, &runtime, &navigation, &id);
            }
        }
    });

    ui.on_board_dismiss_server_warning({
        let ui_weak = ui_weak.clone();
        move || {
            if let Err(error) = open::that("https://www.stremio.com/download-service") {
                tracing::warn!(%error, "could not open the streaming-service download page");
            }
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_board_show_server_warning(false);
            }
        }
    });

    ui.on_board_reload_server({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        move || {
            let dismissed_until = chrono::Utc::now()
                .checked_add_months(chrono::Months::new(1))
                .unwrap_or_else(chrono::Utc::now);
            crate::models::settings::update_profile_settings(&runtime, |settings| {
                settings.streaming_server_warning_dismissed = Some(dismissed_until);
            });
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_board_show_server_warning(false);
            }
        }
    });

    ui.on_board_open_server_settings({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        move || {
            let dismissed_until = chrono::Utc::now()
                .checked_add_months(chrono::Months::new(600))
                .unwrap_or_else(chrono::Utc::now);
            crate::models::settings::update_profile_settings(&runtime, |settings| {
                settings.streaming_server_warning_dismissed = Some(dismissed_until);
            });
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_board_show_server_warning(false);
            }
        }
    });
}

#[tracing::instrument(skip_all)]
#[cfg_attr(feature = "profiling", hotpath::measure)]
pub fn sync(
    ui: &MainWindow,
    continue_watching: &ContinueWatchingPreview,
    board: &CatalogsWithExtra,
    addons: &[Descriptor],
    _ui_weak: &slint::Weak<MainWindow>,
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
) {
    if let Ok(model) = runtime.model() {
        let show_warning = model
            .ctx
            .profile
            .settings
            .streaming_server_warning_dismissed
            .is_none_or(|dismissed_until| dismissed_until < chrono::Utc::now())
            && matches!(
                &model.streaming_server.settings,
                stremio_core::models::common::Loadable::Err(_)
            );
        ui.set_board_show_server_warning(show_warning);
    }

    let mut fingerprint = Fingerprint::new();
    for item in &continue_watching.items {
        let library_item = &item.library_item;
        fingerprint.str(&library_item.id);
        fingerprint.str(&library_item.r#type);
        fingerprint.optional_str(library_item.state.video_id.as_deref());
        fingerprint.optional_str(library_item.behavior_hints.default_video_id.as_deref());
        fingerprint.str(&library_item.name);
        fingerprint.optional_str(library_item.poster.as_ref().map(Url::as_str));
        fingerprint.u64(library_item.progress().to_bits());
        fingerprint.usize(item.notifications);
    }
    for addon in addons {
        fingerprint.str(addon.transport_url.as_str());
        for catalog in &addon.manifest.catalogs {
            fingerprint.str(&catalog.id);
            fingerprint.str(&catalog.r#type);
            fingerprint.optional_str(catalog.name.as_deref());
        }
    }
    fingerprint_catalog_projection(&mut fingerprint, board);
    if !sync_fingerprint_changed(&LAST_SYNC_STATE, fingerprint.finish()) {
        return;
    }

    // 1. Sync Continue Watching
    let continue_watching_items: Vec<MediaCardItem> = {
        let _span = tracing::info_span!("sync_continue_watching").entered();
        let saved_stream_keys = runtime
            .model()
            .ok()
            .map(|model| {
                model
                    .ctx
                    .streams
                    .items
                    .keys()
                    .cloned()
                    .collect::<std::collections::HashSet<_>>()
            })
            .unwrap_or_default();
        let mut items = Vec::with_capacity(continue_watching.items.len());
        for item in &continue_watching.items {
            let library_item = &item.library_item;
            let prog = library_item.progress();
            items.push(MediaCardItem {
                id: library_item.id.as_str().into(),
                media_type: library_item.r#type.as_str().into(),
                video_id: library_details_video_id(
                    library_item.state.video_id.as_deref(),
                    library_item.state.time_offset,
                    library_item.behavior_hints.default_video_id.as_deref(),
                    false,
                )
                .unwrap_or_default()
                .into(),
                title: library_item.name.as_str().into(),
                poster_url: library_item
                    .poster
                    .as_ref()
                    .map(Url::as_str)
                    .unwrap_or_default()
                    .into(),
                poster: crate::image_cache::get_cached_image(&library_item.poster),
                description: library_item.r#type.as_str().into(),
                show_checkmark: false,
                show_progress: prog > 0.0,
                progress_value: (prog / 100.0).clamp(0.0, 1.0) as f32,
                new_videos: i32::try_from(item.notifications.min(99)).unwrap_or(99),
                can_play: library_item
                    .state
                    .video_id
                    .as_ref()
                    .is_some_and(|video_id| {
                        saved_stream_keys.contains(&StreamsItemKey {
                            meta_id: library_item.id.clone(),
                            video_id: video_id.clone(),
                        })
                    }),
            });
        }
        items
    };
    let has_continue_watching = !continue_watching_items.is_empty();
    let continue_watching_model =
        slint::ModelRc::new(slint::VecModel::from(continue_watching_items));
    ui.set_board_continue_watching(continue_watching_model.clone());

    // 2. Sync Board Sections
    let board_sections = {
        let _span = tracing::info_span!("sync_board_sections").entered();
        let catalog_names = catalog_name_index(addons);
        let mut board_sections =
            Vec::with_capacity(board.catalogs.len() + if has_continue_watching { 1 } else { 0 });
        if has_continue_watching {
            board_sections.push(BoardSection {
                title: "Continue watching".into(),
                r_type: "".into(),
                catalog_id: "".into(),
                addon_base: "".into(),
                catalog_index: -1,
                loading: false,
                error_message: "".into(),
                items: continue_watching_model,
                is_continue_watching: true,
                poster_shape: "poster".into(),
            });
        }
        for (catalog_index, catalog) in board.catalogs.iter().enumerate() {
            let Some(page) = catalog.first() else {
                continue;
            };
            let catalog_name = catalog_names
                .get(&(
                    page.request.base.as_str(),
                    page.request.path.id.as_str(),
                    page.request.path.r#type.as_str(),
                ))
                .copied()
                .unwrap_or(page.request.path.id.as_str());
            if let Some(section) = project_catalog_section(catalog_index, catalog, catalog_name) {
                board_sections.push(section);
            }
        }
        board_sections
    };

    // Update rows in-place when the section count is stable so the ListView
    // keeps its current viewport-y. A full model replacement resets scroll
    // position, which causes a visible snap-back during lazy catalog loading.
    // Per-section fingerprints skip set_row_data for unchanged rows, avoiding
    // Slint re-instantiating all cards in rows whose data didn't change.
    let existing = ui.get_board_sections();
    let mut cached_fps = SECTION_FINGERPRINTS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if existing.row_count() == board_sections.len() {
        for (index, section) in board_sections.into_iter().enumerate() {
            let fp = section_data_fingerprint(&section);
            if cached_fps.get(index) != Some(&fp) {
                existing.set_row_data(index, section);
                if index < cached_fps.len() {
                    cached_fps[index] = fp;
                } else {
                    cached_fps.push(fp);
                }
            }
        }
    } else {
        *cached_fps = board_sections
            .iter()
            .map(section_data_fingerprint)
            .collect();
        let sections_model = slint::VecModel::from(board_sections);
        ui.set_board_sections(slint::ModelRc::new(sections_model));
    }
}

fn section_data_fingerprint(section: &BoardSection) -> SyncFingerprint {
    let mut fp = Fingerprint::new();
    fp.str(section.title.as_str());
    fp.bool(section.loading);
    fp.str(section.error_message.as_str());
    fp.bool(section.is_continue_watching);
    fp.str(section.poster_shape.as_str());
    let items = &section.items;
    fp.usize(items.row_count());
    for i in 0..items.row_count() {
        if let Some(item) = items.row_data(i) {
            fp.str(item.id.as_str());
            fp.str(item.media_type.as_str());
            fp.str(item.title.as_str());
            fp.str(item.poster_url.as_str());
            fp.bool(item.show_progress);
            fp.usize(item.new_videos.max(0) as usize);
        }
    }
    fp.finish()
}
