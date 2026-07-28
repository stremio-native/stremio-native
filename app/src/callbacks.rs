use crate::{
    MainWindow, NavigationController, NavigationIntent,
    app_model::{AppModel, AppModelField},
    config::AppConfig,
    models,
    mpv_integration::NativePlaybackBridge,
    playback::PlaybackSelections,
};
use core_env::DesktopEnv;
use slint::ComponentHandle;
use std::sync::{Arc, Mutex};
use stremio_core::{
    models::{
        installed_addons_with_filters::InstalledAddonsRequest,
        library_with_filters::LibraryRequest, library_with_filters::Sort,
    },
    runtime::{
        Runtime, RuntimeAction,
        msg::{Action, ActionLoad},
    },
};

pub fn setup_ui_callbacks(
    ui: &MainWindow,
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
    playback_selections: &Arc<PlaybackSelections>,
    native_playback_bridge: &Option<NativePlaybackBridge>,
    ui_weak: slint::Weak<MainWindow>,
    config: &AppConfig,
    navigation: NavigationController,
) {
    // Hook up submodel setup functions
    models::auth::setup(ui, runtime);
    models::board::setup(ui, runtime, &navigation);
    models::calendar::setup(ui, runtime, &navigation);
    models::discover::setup(ui, runtime, &navigation);
    models::events::setup(ui, runtime);
    models::library::setup(ui, runtime, &navigation);
    models::search::setup(ui, runtime, &navigation);
    models::addons::setup(ui, runtime, &navigation);
    models::details::setup(ui, runtime, &navigation);
    models::settings::setup(ui, runtime, config, native_playback_bridge.as_ref());
    models::onboarding::setup(ui, config);

    // Play stream action
    ui.on_play_stream({
        let ui_weak = ui_weak.clone();
        let runtime = runtime.clone();
        let playback_selections = playback_selections.clone();
        let native_playback_bridge = native_playback_bridge.clone();
        let navigation = navigation.clone();
        move |selection_id| {
            tracing::info!(
                selection_id = %selection_id,
                native_playback_available = native_playback_bridge.is_some(),
                "playback selection requested"
            );
            let Some((selected, stream_name)) = playback_selections.resolve(selection_id.as_str())
            else {
                tracing::warn!(selection_id = %selection_id, "playback selection expired");
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_error_message(
                        "That stream is no longer available. Choose it again.".into(),
                    );
                }
                return;
            };

            if let Some(ui) = ui_weak.upgrade() {
                let detail_title = ui.get_detail_title().to_string();
                tracing::info!(title = %detail_title, "player page shown");

                ui.set_player_title(detail_title.into());
                ui.set_player_stream_name(stream_name.into());
                ui.set_player_poster_image(ui.get_detail_poster());
                ui.set_player_video_frame(slint::Image::default());
                ui.set_player_has_video_frame(false);
                ui.set_player_error("".into());
                ui.set_player_loading(true);
                ui.set_player_buffering(false);
                ui.set_player_buffering_percent(0.0);
                ui.set_player_controls_visible(true);

                let is_series = ui.get_detail_is_series();
                ui.set_player_is_series(is_series);
                if is_series {
                    ui.set_player_seasons(ui.get_detail_seasons());
                    ui.set_player_active_season(ui.get_detail_active_season());
                    ui.set_player_episodes(ui.get_detail_episodes());
                    ui.set_player_active_episode_idx(ui.get_detail_active_episode_idx());
                    ui.set_player_active_video_id(
                        selected
                            .stream_request
                            .as_ref()
                            .map(|request| request.path.id.as_str())
                            .unwrap_or_default()
                            .into(),
                    );
                } else {
                    ui.set_player_seasons(Default::default());
                    ui.set_player_episodes(Default::default());
                    ui.set_player_active_video_id("".into());
                    ui.set_player_active_episode_idx(0);
                }
                ui.set_player_has_next_episode(false);

                ui.invoke_close_player_menus();
                ui.invoke_focus_app_shortcuts();
                navigation.dispatch_and_project(&ui, NavigationIntent::OpenPlayer);
            }
            runtime.dispatch(RuntimeAction {
                field: None,
                action: Action::Load(ActionLoad::Player(Box::new(selected))),
            });
        }
    });

    let clipboard = Arc::new(Mutex::new(arboard::Clipboard::new().ok()));

    ui.on_play_url_magnet_from_clipboard({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let navigation = navigation.clone();
        let clipboard = clipboard.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let text = clipboard
                .lock()
                .ok()
                .and_then(|mut cb| cb.as_mut()?.get_text().ok())
                .unwrap_or_default();
            let trimmed = text.trim();
            if !trimmed.is_empty()
                && (trimmed.starts_with("magnet:")
                    || trimmed.starts_with("stremio:")
                    || trimmed.starts_with("http://")
                    || trimmed.starts_with("https://"))
            {
                crate::deep_link::handle(
                    crate::single_instance::AppCommand::Open(trimmed.to_owned()),
                    &ui,
                    &runtime,
                    &navigation,
                );
            } else {
                ui.set_error_message(
                    "Clipboard does not contain a valid URL or magnet link.".into(),
                );
            }
        }
    });

    ui.on_details_copy_stream_link({
        let runtime = runtime.clone();
        let playback_selections = playback_selections.clone();
        let clipboard = clipboard.clone();
        move |selection_id| {
            let rt = runtime.clone();
            let playback_selections = playback_selections.clone();
            let clipboard = clipboard.clone();
            let selection_id = selection_id.to_string();
            tokio::spawn(async move {
                let model = rt.model().expect("model read failed");
                let settings = model.ctx.profile.settings.clone();
                let streaming_server_url = model.streaming_server.base_url.clone();
                drop(model);

                if let Some((selected, _)) = playback_selections.resolve(&selection_id) {
                    let sdl = stremio_core::deep_links::StreamDeepLinks::from((
                        &selected.stream,
                        streaming_server_url.as_ref(),
                        &settings,
                    ));
                    let link = sdl.external_player.streaming.clone().unwrap_or_else(|| {
                        match &selected.stream.source {
                            stremio_core::types::resource::StreamSource::Url { url } => {
                                url.to_string()
                            }
                            stremio_core::types::resource::StreamSource::YouTube { yt_id } => {
                                format!("https://youtube.com/watch?v={}", yt_id)
                            }
                            stremio_core::types::resource::StreamSource::Torrent {
                                info_hash,
                                ..
                            } => format!("magnet:?xt=urn:btih:{}", hex::encode(info_hash)),
                            _ => String::new(),
                        }
                    });
                    if !link.is_empty()
                        && let Ok(mut cb) = clipboard.lock()
                        && let Some(cb) = cb.as_mut()
                    {
                        let _ = cb.set_text(link);
                    }
                }
            });
        }
    });

    ui.on_details_copy_magnet_link({
        let runtime = runtime.clone();
        let playback_selections = playback_selections.clone();
        let clipboard = clipboard.clone();
        let ui_weak = ui_weak.clone();
        move |selection_id| {
            let rt = runtime.clone();
            let playback_selections = playback_selections.clone();
            let clipboard = clipboard.clone();
            let ui_weak = ui_weak.clone();
            let selection_id = selection_id.to_string();
            tokio::spawn(async move {
                let model = rt.model().expect("model read failed");
                let settings = model.ctx.profile.settings.clone();
                let streaming_server_url = model.streaming_server.base_url.clone();
                drop(model);

                if let Some((selected, _)) = playback_selections.resolve(&selection_id) {
                    let sdl = stremio_core::deep_links::StreamDeepLinks::from((
                        &selected.stream,
                        streaming_server_url.as_ref(),
                        &settings,
                    ));
                    let link =
                        sdl.external_player.magnet.clone().unwrap_or_else(|| {
                            match &selected.stream.source {
                                stremio_core::types::resource::StreamSource::Torrent {
                                    info_hash,
                                    ..
                                } => format!("magnet:?xt=urn:btih:{}", hex::encode(info_hash)),
                                _ => String::new(),
                            }
                        });
                    if !link.is_empty() {
                        if let Ok(mut cb) = clipboard.lock()
                            && let Some(cb) = cb.as_mut()
                        {
                            let _ = cb.set_text(link);
                        }
                    } else {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                ui.set_error_message(
                                    "No magnet link available for this stream.".into(),
                                );
                            }
                        });
                    }
                }
            });
        }
    });

    ui.on_details_copy_download_link({
        let runtime = runtime.clone();
        let playback_selections = playback_selections.clone();
        let clipboard = clipboard.clone();
        let ui_weak = ui_weak.clone();
        move |selection_id| {
            let rt = runtime.clone();
            let playback_selections = playback_selections.clone();
            let clipboard = clipboard.clone();
            let ui_weak = ui_weak.clone();
            let selection_id = selection_id.to_string();
            tokio::spawn(async move {
                let model = rt.model().expect("model read failed");
                let settings = model.ctx.profile.settings.clone();
                let streaming_server_url = model.streaming_server.base_url.clone();
                drop(model);

                if let Some((selected, _)) = playback_selections.resolve(&selection_id) {
                    let sdl = stremio_core::deep_links::StreamDeepLinks::from((
                        &selected.stream,
                        streaming_server_url.as_ref(),
                        &settings,
                    ));
                    if let Some(link) = sdl.external_player.download {
                        if let Ok(mut cb) = clipboard.lock()
                            && let Some(cb) = cb.as_mut()
                        {
                            let _ = cb.set_text(link);
                        }
                    } else {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                ui.set_error_message(
                                    "No download link available for this stream.".into(),
                                );
                            }
                        });
                    }
                }
            });
        }
    });

    ui.on_open_external_url(|url| {
        let url = url.to_string();
        if let Err(error) = open::that(&url) {
            tracing::error!(%error, %url, "failed to open external url");
        }
    });

    ui.on_navigation_back({
        let ui_weak = ui_weak.clone();
        let navigation = navigation.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                navigation.dispatch_and_project(&ui, NavigationIntent::Back);
                ui.set_details_loading(false);
            }
        }
    });

    ui.on_navigation_forward({
        let ui_weak = ui_weak.clone();
        let navigation = navigation.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                navigation.dispatch_and_project(&ui, NavigationIntent::Forward);
                ui.set_details_loading(false);
            }
        }
    });

    ui.on_toggle_fullscreen({
        let ui_weak = ui_weak.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                let fs = !ui.window().is_fullscreen();
                ui.window().set_fullscreen(fs);
                ui.set_is_fullscreen(fs);
            }
        }
    });
}

pub fn trigger_initial_load(runtime: &Arc<Runtime<DesktopEnv, AppModel>>) {
    let rt = runtime.clone();
    tokio::spawn(async move {
        rt.dispatch(RuntimeAction {
            field: None,
            action: Action::Load(ActionLoad::CatalogWithFilters(None)),
        });
        rt.dispatch(RuntimeAction {
            field: Some(AppModelField::Board),
            action: Action::Load(ActionLoad::CatalogsWithExtra(
                stremio_core::models::catalogs_with_extra::Selected {
                    r#type: None,
                    extra: vec![],
                },
            )),
        });
        rt.dispatch(RuntimeAction {
            field: Some(AppModelField::LocalSearch),
            action: Action::Load(ActionLoad::LocalSearch),
        });
        rt.dispatch(RuntimeAction {
            field: None,
            action: Action::Load(ActionLoad::LibraryWithFilters(
                stremio_core::models::library_with_filters::Selected {
                    request: LibraryRequest {
                        r#type: None,
                        sort: Sort::LastWatched,
                        page: Default::default(),
                    },
                },
            )),
        });
        rt.dispatch(RuntimeAction {
            field: None,
            action: Action::Load(ActionLoad::InstalledAddonsWithFilters(
                stremio_core::models::installed_addons_with_filters::Selected {
                    request: InstalledAddonsRequest { r#type: None },
                },
            )),
        });
    });
}
