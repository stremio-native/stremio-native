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
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
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
    config: &AppConfig,
    navigation: NavigationController,
    download_manager: Arc<crate::downloads::DownloadManager>,
) {
    let ui_weak = ui.as_weak();
    // Hook up submodel setup functions
    models::auth::setup(ui, runtime);
    models::board::setup(ui, runtime, &navigation);
    models::calendar::setup(ui, runtime, &navigation);
    models::discover::setup(ui, runtime, &navigation);
    models::events::setup(ui, runtime);
    models::library::setup(ui, runtime, &navigation);
    models::search::setup(ui, runtime, &navigation);
    models::addons::setup(ui, runtime, &navigation);
    crate::community_addons::setup(ui);
    models::details::setup(ui, runtime, &navigation);
    models::settings::setup(ui, runtime, config, native_playback_bridge.as_ref());
    models::onboarding::setup(ui, config);

    ui.on_details_ranking_mode_changed({
        let runtime = runtime.clone();
        let playback_selections = playback_selections.clone();
        let ui_weak = ui_weak.clone();
        move |mode| {
            let mode = match mode.as_str() {
                "Quality" => stream_ranking::RankingMode::Quality,
                "Smallest" => stream_ranking::RankingMode::Smallest,
                "Seeders" => stream_ranking::RankingMode::Seeders,
                "Original" => stream_ranking::RankingMode::Original,
                _ => stream_ranking::RankingMode::Smart,
            };
            playback_selections.set_ranking_mode(mode);
            project_current_streams(&runtime, &playback_selections, &ui_weak);
            persist_profile_setting("stream-ranking-mode", ranking_mode_name(mode));
        }
    });

    ui.on_details_show_filtered_changed({
        let runtime = runtime.clone();
        let playback_selections = playback_selections.clone();
        let ui_weak = ui_weak.clone();
        move |show| {
            playback_selections.set_show_filtered(show);
            project_current_streams(&runtime, &playback_selections, &ui_weak);
            persist_profile_setting("stream-show-filtered", if show { "true" } else { "false" });
        }
    });

    // Play stream action
    ui.on_play_stream({
        let ui_weak = ui_weak.clone();
        let runtime = runtime.clone();
        let playback_selections = playback_selections.clone();
        let native_playback_bridge = native_playback_bridge.clone();
        let navigation = navigation.clone();
        move |selection_id| {
            let switching_in_player = navigation.is_player_visible();
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

            if switching_in_player && let Some(bridge) = native_playback_bridge.as_ref() {
                bridge.prepare_stream_switch();
            }

            let ui_weak = ui_weak.clone();
            let runtime = runtime.clone();
            let navigation = navigation.clone();
            tokio::spawn(async move {
                // Debrid unrestriction only happens after this explicit click.
                // Any provider failure leaves the original source untouched.
                let selected = crate::integrations::resolve_explicit_selection(selected).await;
                let _ = slint::invoke_from_event_loop(move || {
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
                });
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
        let download_manager = download_manager.clone();
        let ui_weak = ui_weak.clone();
        move |selection_id| {
            let rt = runtime.clone();
            let playback_selections = playback_selections.clone();
            let download_manager = download_manager.clone();
            let ui_weak = ui_weak.clone();
            let selection_id = selection_id.to_string();
            tokio::spawn(async move {
                let (settings, streaming_server_url, title) = {
                    let model = rt.model().expect("model read failed");
                    (
                        model.ctx.profile.settings.clone(),
                        model.streaming_server.base_url.clone(),
                        model
                            .meta_details
                            .selected
                            .as_ref()
                            .map(|selected| selected.meta_path.id.clone())
                            .unwrap_or_else(|| "Stremio download".to_owned()),
                    )
                };

                if let Some((selected, stream_name)) = playback_selections.resolve(&selection_id) {
                    let sdl = stremio_core::deep_links::StreamDeepLinks::from((
                        &selected.stream,
                        streaming_server_url.as_ref(),
                        &settings,
                    ));
                    if let Some(link) = sdl.external_player.download {
                        let profile_id = match crate::profiles::active_profile_id().await {
                            Ok(profile_id) => profile_id,
                            Err(error) => {
                                show_download_error(&ui_weak, error.to_string());
                                return;
                            }
                        };
                        let filename = download_filename(&title, &link);
                        let source = crate::downloads::DownloadSource {
                            url: link,
                            headers: Vec::new(),
                            kind: crate::downloads::DownloadSourceKind::DirectHttp,
                        };
                        match download_manager
                            .enqueue(
                                profile_id.as_str(),
                                &stream_name,
                                &filename,
                                crate::paths::get().downloads(),
                                source,
                            )
                            .await
                        {
                            Ok(_) => {
                                crate::downloads::project_active_profile(ui_weak.clone()).await;
                            }
                            Err(error) => show_download_error(&ui_weak, error.to_string()),
                        }
                    } else {
                        show_download_error(
                            &ui_weak,
                            "No download link is available for this stream.".to_owned(),
                        );
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

fn project_current_streams(
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
    playback_selections: &PlaybackSelections,
    ui_weak: &slint::Weak<MainWindow>,
) {
    let views = {
        let model = runtime.model().expect("model read failed");
        playback_selections.rebuild(&model.meta_details, &model.ctx.profile.addons)
    };
    if let Some(ui) = ui_weak.upgrade() {
        crate::event_loop::project_stream_selection_views(&ui, ui_weak, views);
    }
}

fn ranking_mode_name(mode: stream_ranking::RankingMode) -> &'static str {
    match mode {
        stream_ranking::RankingMode::Smart => "Smart",
        stream_ranking::RankingMode::Quality => "Quality",
        stream_ranking::RankingMode::Smallest => "Smallest",
        stream_ranking::RankingMode::Seeders => "Seeders",
        stream_ranking::RankingMode::Original => "Original",
    }
}

fn persist_profile_setting(key: &'static str, value: &'static str) {
    tokio::spawn(async move {
        match crate::profiles::active_profile_id().await {
            Ok(profile_id) => {
                if let Err(error) = crate::profiles::set_setting(&profile_id, key, value).await {
                    tracing::warn!(%error, setting = key, "failed to save profile setting");
                }
            }
            Err(error) => {
                tracing::warn!(%error, setting = key, "active profile is unavailable");
            }
        }
    });
}

fn show_download_error(ui_weak: &slint::Weak<MainWindow>, message: String) {
    let ui_weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_error_message(message.into());
        }
    });
}

fn download_filename(title: &str, link: &str) -> String {
    let extension = url::Url::parse(link)
        .ok()
        .and_then(|url| {
            Path::new(url.path())
                .extension()
                .and_then(|extension| extension.to_str())
                .map(ToOwned::to_owned)
        })
        .filter(|extension| extension.len() <= 8)
        .unwrap_or_else(|| "mp4".to_owned());
    format!("{title}.{extension}")
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
