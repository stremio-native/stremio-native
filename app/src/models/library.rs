use crate::models::details::{load_meta_details_for_video, open_details_route};
use crate::models::pagination::{PaginationIdentity, PaginationScope, gate as pagination_gate};
use crate::models::{
    Fingerprint, SyncFingerprint, clear_sync_fingerprint, library_details_video_id,
    sync_fingerprint_changed,
};
use crate::{AppModel, AppModelField, MainWindow, NavigationController};
use crate::{LibraryRow, MediaCardItem};
use core_env::DesktopEnv;
use slint::ComponentHandle;
use std::sync::{Arc, Mutex, OnceLock};
use stremio_core::{
    models::library_with_filters::{
        LibraryFilter, LibraryRequest, LibraryWithFilters, Selected, Sort,
    },
    runtime::{
        Runtime, RuntimeAction,
        msg::{Action, ActionLibraryWithFilters, ActionLoad},
    },
    types::streams::StreamsItemKey,
};

static SEARCH_QUERY: OnceLock<Mutex<String>> = OnceLock::new();

static LAST_SYNC_STATE: OnceLock<Mutex<Option<SyncFingerprint>>> = OnceLock::new();

fn get_search_query() -> &'static Mutex<String> {
    SEARCH_QUERY.get_or_init(|| Mutex::new(String::new()))
}

fn sort_from_label(label: &str) -> Sort {
    match label {
        "A–Z" | "A-Z" => Sort::Name,
        "Z–A" | "Z-A" => Sort::NameReverse,
        "Most Watched" => Sort::TimesWatched,
        "Watched" => Sort::Watched,
        "Not Watched" => Sort::NotWatched,
        _ => Sort::LastWatched,
    }
}

fn sort_label(sort: &Sort) -> &'static str {
    match sort {
        Sort::LastWatched => "Last Watched",
        Sort::Name => "A–Z",
        Sort::NameReverse => "Z–A",
        Sort::TimesWatched => "Most Watched",
        Sort::Watched => "Watched",
        Sort::NotWatched => "Not Watched",
    }
}

fn type_from_label(label: &str) -> Option<String> {
    match label {
        "All" => None,
        "Movies" => Some("movie".to_owned()),
        "Series" => Some("series".to_owned()),
        "Others" => Some("other".to_owned()),
        value => Some(value.to_lowercase()),
    }
}

fn type_label(value: Option<&str>) -> &'static str {
    match value {
        Some("movie") => "Movies",
        Some("series") => "Series",
        Some("other") => "Others",
        _ => "All",
    }
}

pub fn clear_sync_state() {
    clear_sync_fingerprint(&LAST_SYNC_STATE);
    pagination_gate().reset(PaginationScope::Library);
    pagination_gate().reset(PaginationScope::ContinueWatching);
}

pub fn setup(
    ui: &MainWindow,
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
    navigation: &NavigationController,
) {
    let ui_weak = ui.as_weak();

    ui.on_library_load_next_page({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        move || {
            let continue_watching = ui_weak
                .upgrade()
                .is_some_and(|ui| ui.get_library_continue_watching_mode());
            let identity = runtime.model().ok().and_then(|model| {
                if continue_watching {
                    model
                        .continue_watching
                        .selectable
                        .next_page
                        .as_ref()
                        .and_then(|next_page| {
                            PaginationIdentity::new(
                                PaginationScope::ContinueWatching,
                                model
                                    .continue_watching
                                    .selected
                                    .as_ref()
                                    .map(|selected| &selected.request),
                                &next_page.request,
                            )
                        })
                } else {
                    model
                        .library
                        .selectable
                        .next_page
                        .as_ref()
                        .and_then(|next_page| {
                            PaginationIdentity::new(
                                PaginationScope::Library,
                                model
                                    .library
                                    .selected
                                    .as_ref()
                                    .map(|selected| &selected.request),
                                &next_page.request,
                            )
                        })
                }
            });
            if pagination_gate().try_begin(identity) {
                runtime.dispatch(RuntimeAction {
                    field: Some(if continue_watching {
                        AppModelField::ContinueWatching
                    } else {
                        AppModelField::Library
                    }),
                    action: Action::LibraryWithFilters(ActionLibraryWithFilters::LoadNextPage),
                });
            }
        }
    });

    // Type change callback
    ui.on_library_type_changed({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        move |t| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_library_has_next_page(false);
                ui.set_library_scroll_y(0.0);
            }
            if t.as_str() == "Local" {
                let ui_weak = ui_weak.clone();
                tokio::spawn(crate::local_library::project(ui_weak));
                return;
            }
            clear_sync_state();
            let rt = runtime.clone();
            let r_type = type_from_label(t.as_str());
            let sort = ui_weak
                .upgrade()
                .map(|ui| sort_from_label(ui.get_library_active_sort().as_str()))
                .unwrap_or_default();
            let continue_watching = ui_weak
                .upgrade()
                .is_some_and(|ui| ui.get_library_continue_watching_mode());

            tokio::spawn(async move {
                rt.dispatch(RuntimeAction {
                    field: Some(if continue_watching {
                        AppModelField::ContinueWatching
                    } else {
                        AppModelField::Library
                    }),
                    action: Action::Load(ActionLoad::LibraryWithFilters(Selected {
                        request: LibraryRequest {
                            r#type: r_type,
                            sort,
                            page: Default::default(),
                        },
                    })),
                });
            });
        }
    });

    ui.on_library_sort_changed({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        move |label| {
            let r_type = ui_weak
                .upgrade()
                .and_then(|ui| type_from_label(ui.get_library_active_type().as_str()));
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_library_has_next_page(false);
                ui.set_library_scroll_y(0.0);
            }
            clear_sync_state();
            let rt = runtime.clone();
            let sort = sort_from_label(label.as_str());
            let continue_watching = ui_weak
                .upgrade()
                .is_some_and(|ui| ui.get_library_continue_watching_mode());
            tokio::spawn(async move {
                rt.dispatch(RuntimeAction {
                    field: Some(if continue_watching {
                        AppModelField::ContinueWatching
                    } else {
                        AppModelField::Library
                    }),
                    action: Action::Load(ActionLoad::LibraryWithFilters(Selected {
                        request: LibraryRequest {
                            r#type: r_type,
                            sort,
                            page: Default::default(),
                        },
                    })),
                });
            });
        }
    });

    // Local Search changed callback
    ui.on_library_search_changed({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        move |query| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_library_has_next_page(false);
                ui.set_library_scroll_y(0.0);
            }
            clear_sync_state();
            if let Ok(mut q) = get_search_query().lock() {
                *q = query.to_string();
            }

            // Trigger refresh immediately
            if let Some(ui) = ui_weak.upgrade()
                && let Ok(model) = runtime.model()
            {
                let ui_sync = ui_weak.clone();
                let rt_sync = runtime.clone();
                if ui.get_library_continue_watching_mode() {
                    sync(&ui, &model.continue_watching, &ui_sync, &rt_sync, true);
                } else {
                    sync(&ui, &model.library, &ui_sync, &rt_sync, false);
                }
            }
        }
    });

    // Item selection callback
    ui.on_library_item_selected({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        let navigation = navigation.clone();
        move |id, media_type, video_id| {
            let id = id.to_string();
            let media_type = media_type.to_string();
            let video_id = (!video_id.is_empty()).then(|| video_id.to_string());
            if let Some(ui) = ui_weak.upgrade() {
                open_details_route(&ui, &runtime, &navigation, &id);
            }
            load_meta_details_for_video(&runtime, id, Some(media_type), video_id);
        }
    });

    ui.on_library_remove_item({
        let runtime = runtime.clone();
        move |id| {
            runtime.dispatch(RuntimeAction {
                field: None,
                action: Action::Ctx(stremio_core::runtime::msg::ActionCtx::RemoveFromLibrary(
                    id.to_string(),
                )),
            });
        }
    });

    ui.on_library_watched_changed({
        let runtime = runtime.clone();
        move |id, is_watched| {
            runtime.dispatch(RuntimeAction {
                field: None,
                action: Action::Ctx(
                    stremio_core::runtime::msg::ActionCtx::LibraryItemMarkAsWatched {
                        id: id.to_string(),
                        is_watched,
                    },
                ),
            });
        }
    });

    ui.on_library_dismiss_item({
        let runtime = runtime.clone();
        move |id| {
            let id = id.to_string();
            runtime.dispatch(RuntimeAction {
                field: None,
                action: Action::Ctx(stremio_core::runtime::msg::ActionCtx::RewindLibraryItem(
                    id.clone(),
                )),
            });
            runtime.dispatch(RuntimeAction {
                field: None,
                action: Action::Ctx(
                    stremio_core::runtime::msg::ActionCtx::DismissNotificationItem(id),
                ),
            });
        }
    });

    ui.on_library_play_item({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        let navigation = navigation.clone();
        move |id| {
            let id = id.to_string();
            let saved_stream = runtime.model().ok().and_then(|model| {
                let item = model.ctx.library.items.get(&id)?;
                let video_id = item.state.video_id.as_ref()?;
                model
                    .ctx
                    .streams
                    .items
                    .get(&StreamsItemKey {
                        meta_id: id.clone(),
                        video_id: video_id.clone(),
                    })
                    .cloned()
            });
            if let (Some(ui), Some(saved_stream)) = (ui_weak.upgrade(), saved_stream) {
                crate::deep_link::open_saved_stream(&ui, &runtime, &navigation, saved_stream);
            }
        }
    });
}

#[tracing::instrument(skip_all)]
pub fn sync<F: LibraryFilter>(
    ui: &MainWindow,
    library: &LibraryWithFilters<F>,
    _ui_weak: &slint::Weak<MainWindow>,
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
    continue_watching_mode: bool,
) {
    let pagination_scope = if continue_watching_mode {
        PaginationScope::ContinueWatching
    } else {
        PaginationScope::Library
    };
    let pagination_identity = library.selectable.next_page.as_ref().and_then(|next_page| {
        PaginationIdentity::new(
            pagination_scope,
            library.selected.as_ref().map(|selected| &selected.request),
            &next_page.request,
        )
    });
    pagination_gate().observe(pagination_scope, pagination_identity);
    ui.set_library_has_next_page(pagination_identity.is_some());

    let query = get_search_query()
        .lock()
        .map(|q| q.clone())
        .unwrap_or_default();
    let normalized_query = query.to_lowercase();
    let (authenticated, notification_counts, saved_stream_keys) = runtime
        .model()
        .ok()
        .map(|model| {
            let counts = model
                .ctx
                .notifications
                .items
                .iter()
                .map(|(id, items)| (id.clone(), items.len()))
                .collect::<std::collections::HashMap<_, _>>();
            let saved_stream_keys = model
                .ctx
                .streams
                .items
                .keys()
                .cloned()
                .collect::<std::collections::HashSet<_>>();
            (model.ctx.profile.auth.is_some(), counts, saved_stream_keys)
        })
        .unwrap_or_default();
    ui.set_library_authenticated(authenticated);

    // 1. Filter raw items based on query
    let (raw_items, columns) = {
        let _span = tracing::info_span!("filter_library_items").entered();
        let mut raw_items = Vec::with_capacity(library.catalog.len());
        for item in &library.catalog {
            // Apply search query match
            if !normalized_query.is_empty() && !item.name.to_lowercase().contains(&normalized_query)
            {
                continue;
            }
            raw_items.push(item);
        }

        let metrics = crate::models::media_grid_metrics(ui);
        let columns = metrics.columns;

        (raw_items, columns)
    };

    let mut fingerprint = Fingerprint::new();
    fingerprint.usize(columns);
    fingerprint.str(&normalized_query);
    fingerprint.bool(authenticated);
    fingerprint.bool(continue_watching_mode);
    if let Some(selected) = &library.selected {
        fingerprint.optional_str(selected.request.r#type.as_deref());
        fingerprint.str(sort_label(&selected.request.sort));
    }
    for item in &raw_items {
        fingerprint.str(&item.id);
        fingerprint.str(&item.r#type);
        fingerprint.optional_str(item.behavior_hints.default_video_id.as_deref());
        fingerprint.bool(item.watched());
        fingerprint.u64(item.progress().to_bits());
        fingerprint.str(&item.name);
        fingerprint.optional_str(item.poster.as_ref().map(url::Url::as_str));
        fingerprint.usize(
            notification_counts
                .get(&item.id)
                .copied()
                .unwrap_or_default(),
        );
    }
    if !sync_fingerprint_changed(&LAST_SYNC_STATE, fingerprint.finish()) {
        return;
    }

    if let Some(selected) = &library.selected {
        if ui.get_library_active_type().as_str() != "Local" {
            ui.set_library_active_type(type_label(selected.request.r#type.as_deref()).into());
        }
        ui.set_library_active_sort(sort_label(&selected.request.sort).into());
    }

    let visible_items = {
        let _span = tracing::info_span!("map_library_cards").entered();
        let mut visible_items = Vec::with_capacity(raw_items.len());
        for item in raw_items {
            let progress = item.progress();
            visible_items.push(MediaCardItem {
                id: item.id.as_str().into(),
                media_type: item.r#type.as_str().into(),
                video_id: library_details_video_id(
                    item.state.video_id.as_deref(),
                    item.state.time_offset,
                    item.behavior_hints.default_video_id.as_deref(),
                    !continue_watching_mode,
                )
                .unwrap_or_default()
                .into(),
                title: item.name.as_str().into(),
                poster_url: item
                    .poster
                    .as_ref()
                    .map(url::Url::as_str)
                    .unwrap_or_default()
                    .into(),
                poster: crate::image_cache::get_cached_image(&item.poster),
                description: item.r#type.as_str().into(),
                show_checkmark: item.watched(),
                show_progress: progress > 0.0,
                progress_value: (progress / 100.0).clamp(0.0, 1.0) as f32,
                new_videos: i32::try_from(
                    notification_counts
                        .get(&item.id)
                        .copied()
                        .unwrap_or_default()
                        .min(99),
                )
                .unwrap_or(99),
                can_play: item.state.video_id.as_ref().is_some_and(|video_id| {
                    saved_stream_keys.contains(&StreamsItemKey {
                        meta_id: item.id.clone(),
                        video_id: video_id.clone(),
                    })
                }),
            });
        }
        visible_items
    };

    ui.set_library_column_count(columns as i32);

    let rows_model = {
        let _span = tracing::info_span!("chunk_library_rows").entered();
        let chunked = crate::models::chunk_vector_owned(visible_items, columns);
        let mut slint_rows = Vec::with_capacity(chunked.len());
        for row_items in chunked {
            let row_model = slint::VecModel::from(row_items);
            slint_rows.push(LibraryRow {
                cols: slint::ModelRc::new(row_model),
            });
        }
        slint::VecModel::from(slint_rows)
    };

    ui.set_library_rows(slint::ModelRc::new(rows_model));
}
