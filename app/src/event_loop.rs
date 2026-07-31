use crate::{
    CastDevice, MainWindow, NavigationController, StreamLink,
    app_model::{AppModel, AppModelField, format_rate},
    image_cache, models,
    mpv_integration::NativePlaybackBridge,
    playback::PlaybackSelections,
};
use core_env::DesktopEnv;
use futures::StreamExt;
use slint::{Model, ModelRc};
use std::sync::Arc;
use stremio_core::{
    models::common::Loadable,
    runtime::{Runtime, RuntimeEvent, msg::Event},
};

fn update_vec_model<T: Clone + 'static>(
    model: &slint::ModelRc<T>,
    values: Vec<T>,
) -> Result<(), Vec<T>> {
    let Some(model) = model.as_any().downcast_ref::<slint::VecModel<T>>() else {
        return Err(values);
    };
    if model.row_count() == values.len() {
        for (index, value) in values.into_iter().enumerate() {
            model.set_row_data(index, value);
        }
    } else {
        model.set_vec(values);
    }
    Ok(())
}

pub(crate) fn project_stream_selection_views(
    ui: &MainWindow,
    ui_weak: &slint::Weak<MainWindow>,
    stream_selection_views: Vec<crate::playback::StreamSelectionView>,
) {
    let stream_links: Vec<StreamLink> = stream_selection_views
        .into_iter()
        .map(|link| {
            let score_summary = link.score_reasons.join(", ");
            StreamLink {
                id: link.id.into(),
                name: link.name.into(),
                description: link.description.into(),
                provider: link.provider.into(),
                thumbnail: link
                    .thumbnail
                    .as_deref()
                    .and_then(|thumbnail| url::Url::parse(thumbnail).ok())
                    .map(|thumbnail| image_cache::get_poster_image(&Some(thumbnail), ui_weak))
                    .unwrap_or_default(),
                thumbnail_url: link.thumbnail.unwrap_or_default().into(),
                progress: link.progress,
                score: link.score,
                score_reasons: ModelRc::new(slint::VecModel::from(
                    link.score_reasons
                        .into_iter()
                        .map(Into::into)
                        .collect::<Vec<slint::SharedString>>(),
                )),
                score_summary: score_summary.into(),
                filtered: link.filtered,
            }
        })
        .collect();
    let mut providers = vec![slint::SharedString::from("All")];
    for stream in &stream_links {
        if !providers
            .iter()
            .any(|provider| provider == &stream.provider)
        {
            providers.push(stream.provider.clone());
        }
    }
    let current_streams = ui.get_stream_links();
    if let Err(stream_links) = update_vec_model(&current_streams, stream_links) {
        ui.set_stream_links(slint::ModelRc::new(slint::VecModel::from(stream_links)));
    }
    let current_providers = ui.get_detail_stream_providers();
    if let Err(providers) = update_vec_model(&current_providers, providers) {
        ui.set_detail_stream_providers(slint::ModelRc::new(slint::VecModel::from(providers)));
    }
}

fn schedule_debrid_annotations(
    runtime: Arc<Runtime<DesktopEnv, AppModel>>,
    playback_selections: Arc<PlaybackSelections>,
    ui_weak: slint::Weak<MainWindow>,
) {
    let (generation, candidates) = playback_selections.debrid_candidates();
    if candidates.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let Ok(profile_id) = crate::profiles::active_profile_id().await else {
            return;
        };
        let Ok(providers) =
            crate::integrations::enabled_debrid_providers(profile_id.as_str()).await
        else {
            return;
        };
        if providers.is_empty() {
            return;
        }
        let checks = candidates.into_iter().map(|(selection_id, hash)| {
            let providers = providers.clone();
            async move {
                let results = futures::future::join_all(
                    providers
                        .iter()
                        .map(|provider| provider.availability(&hash)),
                )
                .await;
                let available = if results
                    .iter()
                    .any(|result| matches!(result, Ok(debrid::DebridAvailability::Cached)))
                {
                    Some(stream_ranking::DebridAvailability::Cached)
                } else if results
                    .iter()
                    .any(|result| matches!(result, Ok(debrid::DebridAvailability::Uncached)))
                {
                    Some(stream_ranking::DebridAvailability::Uncached)
                } else {
                    None
                };
                available.map(|available| (selection_id, available))
            }
        });
        let availability = futures::future::join_all(checks)
            .await
            .into_iter()
            .flatten()
            .collect();
        if !playback_selections.apply_debrid_availability(generation, availability) {
            return;
        }
        let views = {
            let model = runtime.model().expect("model read failed");
            playback_selections.rebuild(&model.meta_details, &model.ctx.profile.addons)
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                project_stream_selection_views(&ui, &ui_weak, views);
            }
        });
    });
}

pub fn start_event_loop(
    mut rx: futures::channel::mpsc::Receiver<RuntimeEvent<DesktopEnv, AppModel>>,
    runtime: Arc<Runtime<DesktopEnv, AppModel>>,
    ui_weak: slint::Weak<MainWindow>,
    playback_selections: Arc<PlaybackSelections>,
    native_playback_bridge: Option<NativePlaybackBridge>,
    navigation: NavigationController,
    discord_rpc: Arc<crate::discord::DiscordRpc>,
) {
    tokio::spawn(async move {
        let mut first_render = true;
        let mut last_discord_enabled: Option<bool> = None;
        let mut last_profile_catalogs_fingerprint = None;
        let mut last_profile_addons_fingerprint = None;
        let mut last_auth_projection: Option<(String, Option<String>)> = None;
        let mut last_calendar_fingerprint = None;
        let mut last_details_projection_fingerprint = None;
        let last_stream_fingerprint = Arc::new(std::sync::Mutex::new(None));
        let last_details_id = Arc::new(std::sync::Mutex::new(None));
        while let Some(event) = rx.next().await {
            match event {
                RuntimeEvent::NewState(mut fields, ..) => {
                    // Drain only updates that are already queued. Waiting a fixed window here
                    // added latency to every state projection, including isolated input updates.
                    loop {
                        match rx.try_recv() {
                            Ok(RuntimeEvent::NewState(next_fields, ..)) => {
                                for field in next_fields {
                                    if !fields.contains(&field) {
                                        fields.push(field);
                                    }
                                }
                            }
                            Ok(RuntimeEvent::CoreEvent(event)) => {
                                handle_core_event(event, &ui_weak);
                            }
                            Err(_) => break,
                        }
                    }
                    let _state_span = tracing::info_span!("NewState", ?fields).entered();
                    #[cfg(debug_assertions)]
                    let state_started = std::time::Instant::now();
                    #[cfg(debug_assertions)]
                    let lock_start = std::time::Instant::now();
                    let model = runtime.model().expect("model read failed");
                    let discord_enabled = model.ctx.profile.settings.discord_rpc_enabled;
                    if last_discord_enabled != Some(discord_enabled) {
                        last_discord_enabled = Some(discord_enabled);
                        if discord_enabled {
                            discord_rpc.connect().ok();
                        } else {
                            discord_rpc.disconnect().ok();
                        }
                    }
                    #[cfg(debug_assertions)]
                    {
                        let lock_elapsed = lock_start.elapsed().as_millis();
                        if lock_elapsed > 15 {
                            tracing::warn!(
                                elapsed_ms = lock_elapsed,
                                "Model read lock acquisition took too long in NewState"
                            );
                        }
                    }
                    let ui_weak_clone = ui_weak.clone();

                    let active_tab = navigation.active_tab_index();
                    let profile_sync_needed = first_render || fields.contains(&AppModelField::Ctx);
                    let profile_catalogs_changed = if profile_sync_needed {
                        let fingerprint =
                            models::profile_catalogs_fingerprint(&model.ctx.profile.addons);
                        let changed = last_profile_catalogs_fingerprint != Some(fingerprint);
                        last_profile_catalogs_fingerprint = Some(fingerprint);
                        changed
                    } else {
                        false
                    };
                    let profile_addons_changed = if profile_sync_needed {
                        let fingerprint =
                            models::profile_addons_fingerprint(&model.ctx.profile.addons);
                        let changed = last_profile_addons_fingerprint != Some(fingerprint);
                        last_profile_addons_fingerprint = Some(fingerprint);
                        changed
                    } else {
                        false
                    };
                    let auth_projection = profile_sync_needed.then(|| {
                        let email = model
                            .ctx
                            .profile
                            .auth
                            .as_ref()
                            .map(|auth| auth.user.email.clone())
                            .unwrap_or_default();
                        let avatar = model
                            .ctx
                            .profile
                            .auth
                            .as_ref()
                            .and_then(|auth| auth.user.avatar.clone());
                        (email, avatar)
                    });
                    let auth_update = auth_projection.and_then(|projection| {
                        if last_auth_projection.as_ref() == Some(&projection) {
                            None
                        } else {
                            last_auth_projection = Some(projection.clone());
                            Some(projection)
                        }
                    });
                    let event_modal = profile_sync_needed.then(|| match &model.ctx.events.modal {
                        Loadable::Ready(Some(event)) => Some((
                            event.id.clone(),
                            event.title.clone(),
                            event.message.clone(),
                            event.image_url.clone(),
                            event
                                .addon
                                .as_ref()
                                .map(|addon| addon.name.clone())
                                .unwrap_or_default(),
                            event
                                .addon
                                .as_ref()
                                .map(|addon| addon.manifest_url.to_string())
                                .unwrap_or_default(),
                            event
                                .external_url
                                .as_ref()
                                .map(ToString::to_string)
                                .unwrap_or_default(),
                        )),
                        _ => None,
                    });

                    let board_sync_needed = (first_render
                        || fields.contains(&AppModelField::ContinueWatching)
                        || fields.contains(&AppModelField::ContinueWatchingPreview)
                        || fields.contains(&AppModelField::Board)
                        || profile_catalogs_changed)
                        && active_tab == 0;
                    let discover_tab =
                        crate::Tab::try_from(active_tab).is_ok_and(crate::Tab::is_discovery);
                    let discover_sync_needed =
                        (first_render || fields.contains(&AppModelField::Discover)) && discover_tab;
                    let library_sync_needed = (first_render
                        || fields.contains(&AppModelField::Library)
                        || fields.contains(&AppModelField::ContinueWatching)
                        || fields.contains(&AppModelField::Ctx))
                        && active_tab == 2;
                    let addons_sync_needed = (first_render
                        || fields.contains(&AppModelField::RemoteAddons)
                        || fields.contains(&AppModelField::InstalledAddons)
                        || profile_addons_changed)
                        && active_tab == 3;
                    let addon_details_sync_needed = fields.contains(&AppModelField::AddonDetails);
                    let meta_details_changed = fields.contains(&AppModelField::MetaDetails);
                    let details_context_changed = fields.contains(&AppModelField::Ctx);
                    let details_route = (meta_details_changed || details_context_changed)
                        .then(|| {
                            model.meta_details.selected.as_ref().and_then(|selected| {
                                navigation.details_presentation(&selected.meta_path.id).map(
                                    |presentation| (selected.meta_path.id.clone(), presentation),
                                )
                            })
                        })
                        .flatten();
                    let details_route_id = details_route
                        .as_ref()
                        .map(|(media_id, _presentation)| media_id.clone());
                    let (details_is_in_library, details_is_watched, details_notifications_enabled) =
                        details_route_id
                            .as_deref()
                            .and_then(|id| model.ctx.library.items.get(id))
                            .map(|item| (!item.removed, item.watched(), !item.state.no_notif))
                            .unwrap_or((false, false, true));
                    let details_projection_changed = if meta_details_changed {
                        details_route
                            .as_ref()
                            .is_some_and(|(_media_id, presentation)| {
                                let fingerprint = models::details::projection_fingerprint(
                                    &model.meta_details,
                                    details_is_in_library,
                                    *presentation,
                                );
                                let changed =
                                    last_details_projection_fingerprint != Some(fingerprint);
                                last_details_projection_fingerprint = Some(fingerprint);
                                changed
                            })
                    } else {
                        false
                    };
                    let details_sync_needed = meta_details_changed && details_projection_changed;
                    let details_stream_sync_needed =
                        meta_details_changed && details_route_id.is_some();
                    let details_library_sync_needed =
                        details_context_changed && details_route_id.is_some();
                    let settings_sync_needed = first_render
                        || ((fields.contains(&AppModelField::Ctx)
                            || fields.contains(&AppModelField::StreamingServer))
                            && active_tab == 4);
                    let data_export_sync_needed =
                        fields.contains(&AppModelField::DataExport) && active_tab == 4;
                    let calendar_fingerprint_changed = if active_tab == 5
                        && (first_render
                            || fields.contains(&AppModelField::Calendar)
                            || fields.contains(&AppModelField::Ctx))
                    {
                        let fingerprint = models::calendar::state_fingerprint(&model.calendar);
                        let changed = last_calendar_fingerprint != Some(fingerprint);
                        last_calendar_fingerprint = Some(fingerprint);
                        changed
                    } else {
                        false
                    };
                    let calendar_sync_needed = active_tab == 5
                        && (first_render
                            || fields.contains(&AppModelField::Calendar)
                            || calendar_fingerprint_changed);
                    let calendar_context_refresh_needed =
                        active_tab == 5 && fields.contains(&AppModelField::Ctx);
                    let search_sync_needed = (first_render
                        || fields.contains(&AppModelField::Search)
                        || profile_catalogs_changed)
                        && active_tab == 6;
                    let local_search_sync_needed = fields.contains(&AppModelField::LocalSearch);
                    let player_sync_needed = fields.contains(&AppModelField::Player);
                    let streaming_stats_sync_needed =
                        first_render || fields.contains(&AppModelField::StreamingServer);

                    first_render = false;

                    let _clone_span = tracing::info_span!("clone_model_state").entered();
                    // Clone core submodels for thread-safe UI thread updates (only if needed)
                    let continue_watching_cloned = if board_sync_needed {
                        Some(model.continue_watching_preview.clone())
                    } else {
                        None
                    };
                    let board_cloned = if board_sync_needed {
                        Some(model.board.clone())
                    } else {
                        None
                    };
                    let addons_cloned = if board_sync_needed || addons_sync_needed {
                        Some(model.ctx.profile.addons.clone())
                    } else {
                        None
                    };
                    let discover_cloned = if discover_sync_needed {
                        Some(model.discover.clone())
                    } else {
                        None
                    };
                    let library_cloned = if library_sync_needed {
                        Some(model.library.clone())
                    } else {
                        None
                    };
                    let continue_watching_library_cloned = if library_sync_needed {
                        Some(model.continue_watching.clone())
                    } else {
                        None
                    };
                    let remote_addons_cloned = if addons_sync_needed {
                        Some(model.remote_addons.clone())
                    } else {
                        None
                    };
                    let addon_details_cloned =
                        addon_details_sync_needed.then(|| model.addon_details.clone());
                    let meta_details_cloned = if details_sync_needed {
                        Some(model.meta_details.clone())
                    } else {
                        None
                    };
                    let settings_cloned = if settings_sync_needed {
                        Some((
                            model.ctx.profile.settings.clone(),
                            model
                                .ctx
                                .profile
                                .auth
                                .as_ref()
                                .map(|auth| auth.user.id.0.clone())
                                .unwrap_or_default(),
                            model.ctx.profile.has_trakt::<DesktopEnv>(),
                            model.ctx.streaming_server_urls.clone(),
                            match &model.streaming_server.settings {
                                Loadable::Ready(settings) => Some(settings.clone()),
                                _ => None,
                            },
                        ))
                    } else {
                        None
                    };
                    let data_export_cloned =
                        data_export_sync_needed.then(|| model.data_export.clone());
                    let calendar_cloned = if calendar_sync_needed {
                        Some(model.calendar.clone())
                    } else {
                        None
                    };
                    let search_cloned = search_sync_needed.then(|| model.search.clone());
                    let search_addons_cloned =
                        search_sync_needed.then(|| model.ctx.profile.addons.clone());
                    let local_search_cloned =
                        local_search_sync_needed.then(|| model.local_search.clone());
                    let streaming_stats = streaming_stats_sync_needed
                        .then(|| model.streaming_server.statistics.clone())
                        .flatten()
                        .and_then(|statistics| match statistics {
                            Loadable::Ready(statistics) => Some((
                                format_rate(statistics.download_speed),
                                format_rate(statistics.upload_speed),
                                i32::try_from(statistics.peers).unwrap_or(i32::MAX),
                                (statistics.stream_progress * 100.0).clamp(0.0, 100.0) as f32,
                            )),
                            _ => None,
                        });
                    let cast_devices = streaming_stats_sync_needed.then(|| {
                        match &model.streaming_server.playback_devices {
                            Loadable::Ready(devices) => (
                                devices
                                    .iter()
                                    .map(|device| CastDevice {
                                        id: device.id.as_str().into(),
                                        name: device.name.as_str().into(),
                                    })
                                    .collect::<Vec<_>>(),
                                false,
                            ),
                            Loadable::Loading => (Vec::new(), true),
                            _ => (Vec::new(), false),
                        }
                    });

                    let stream_selection_views = if details_stream_sync_needed {
                        {
                            playback_selections
                                .rebuild(&model.meta_details, &model.ctx.profile.addons)
                        }
                    } else {
                        Default::default()
                    };
                    let stream_selection_fingerprint = details_stream_sync_needed.then(|| {
                        let mut fingerprint = models::Fingerprint::new();
                        for stream in &stream_selection_views {
                            fingerprint.str(&stream.id);
                            fingerprint.str(&stream.name);
                            fingerprint.str(&stream.description);
                            fingerprint.str(&stream.provider);
                            fingerprint.optional_str(stream.thumbnail.as_deref());
                            fingerprint.u64(stream.progress.to_bits().into());
                        }
                        fingerprint.finish()
                    });
                    let trailer_selection_id = details_stream_sync_needed
                        .then(|| playback_selections.trailer_id())
                        .flatten();
                    let detail_stream_loading_count = details_stream_sync_needed.then(|| {
                        i32::try_from(
                            model
                                .meta_details
                                .streams
                                .iter()
                                .filter(|resource| {
                                    matches!(resource.content, Some(Loadable::Loading))
                                })
                                .count(),
                        )
                        .unwrap_or(i32::MAX)
                    });
                    // The subtitles menu labels each add-on track with the
                    // add-on's name, so carry the installed transport URLs out
                    // with the player snapshot before the model guard drops.
                    let player_cloned = player_sync_needed.then(|| {
                        (
                            model.player.clone(),
                            model.ctx.streams.clone(),
                            model
                                .ctx
                                .profile
                                .addons
                                .iter()
                                .map(|addon| {
                                    (addon.transport_url.to_string(), addon.manifest.name.clone())
                                })
                                .collect::<Vec<_>>(),
                        )
                    });

                    drop(_clone_span);
                    // Drop the model guard before invoking event loop
                    drop(model);
                    let calendar_refresh_started = calendar_context_refresh_needed
                        && models::calendar::ensure_loaded(&runtime);
                    if let (Some(playback), Some((player, streams, addon_names))) =
                        (&native_playback_bridge, player_cloned.as_ref())
                        && navigation.is_player_visible()
                    {
                        playback.sync_player(
                            player,
                            streams,
                            addon_names,
                            &ui_weak_clone,
                            &navigation,
                        );
                    }
                    let ui_weak_for_sync = ui_weak_clone.clone();
                    let runtime_for_sync = runtime.clone();
                    let navigation_for_sync = navigation.clone();
                    let playback_selections_for_sync = playback_selections.clone();
                    let last_stream_fingerprint_clone = last_stream_fingerprint.clone();
                    let last_details_id_clone = last_details_id.clone();

                    #[cfg(debug_assertions)]
                    let dispatch_queued = std::time::Instant::now();
                    let _ = slint::invoke_from_event_loop(move || {
                        #[cfg(debug_assertions)]
                        {
                            let queue_delay = dispatch_queued.elapsed();
                            if queue_delay.as_millis() > 10 {
                                tracing::warn!(
                                    elapsed_ms = queue_delay.as_millis(),
                                    "Slint event loop dispatch delay took too long"
                                );
                            }
                        }
                        let patch_started = std::time::Instant::now();
                        #[cfg(debug_assertions)]
                        let _ui_span = tracing::info_span!(
                            "UI_Thread_Sync",
                            queue_delay_ms = dispatch_queued.elapsed().as_millis()
                        )
                        .entered();
                        #[cfg(not(debug_assertions))]
                        let _ui_span = tracing::info_span!("UI_Thread_Sync").entered();
                        if let Some(ui) = ui_weak_clone.upgrade() {
                            if let Some((email, avatar_url)) = auth_update.as_ref() {
                                let current_username = ui.get_username().to_string();
                                if !email.is_empty() {
                                    ui.set_username(email.as_str().into());
                                    let avatar_letter = email
                                        .chars()
                                        .next()
                                        .unwrap_or('U')
                                        .to_string()
                                        .to_uppercase();
                                    ui.set_avatar_letter(avatar_letter.into());

                                    if let Some(url_str) = avatar_url {
                                        if let Ok(url) = url::Url::parse(url_str) {
                                            let img = image_cache::get_poster_image(
                                                &Some(url),
                                                &ui_weak_for_sync,
                                            );
                                            ui.set_avatar_image(img);
                                            ui.set_has_avatar_image(true);
                                        } else {
                                            ui.set_has_avatar_image(false);
                                        }
                                    } else {
                                        ui.set_has_avatar_image(false);
                                    }
                                } else if current_username != "Guest" {
                                    ui.set_username("".into());
                                    ui.set_has_avatar_image(false);
                                }
                            }

                            if let Some(event) = event_modal {
                                if let Some((
                                    id,
                                    title,
                                    message,
                                    image_url,
                                    addon_name,
                                    addon_manifest_url,
                                    external_url,
                                )) = event
                                {
                                    ui.set_event_modal_id(id.into());
                                    ui.set_event_modal_title(title.into());
                                    ui.set_event_modal_message(message.into());
                                    ui.set_event_modal_artwork(image_cache::get_poster_image(
                                        &Some(image_url),
                                        &ui_weak_for_sync,
                                    ));
                                    ui.set_event_modal_addon_name(addon_name.into());
                                    ui.set_event_modal_addon_manifest_url(
                                        addon_manifest_url.into(),
                                    );
                                    ui.set_event_modal_external_url(external_url.into());
                                    ui.set_event_modal_open(true);
                                } else {
                                    ui.set_event_modal_open(false);
                                    ui.set_event_modal_id("".into());
                                    ui.set_event_modal_artwork(slint::Image::default());
                                }
                            }

                            if board_sync_needed
                                || discover_sync_needed
                                || library_sync_needed
                                || addons_sync_needed
                                || calendar_sync_needed
                                || search_sync_needed
                            {
                                ui.set_loading(false);
                            }

                            if streaming_stats_sync_needed {
                                if let Some((download, upload, peers, progress)) = &streaming_stats
                                {
                                    ui.set_player_download_speed(download.as_str().into());
                                    ui.set_player_upload_speed(upload.as_str().into());
                                    ui.set_player_peer_count(*peers);
                                    ui.set_player_stream_progress(*progress);
                                } else {
                                    ui.set_player_download_speed("".into());
                                    ui.set_player_upload_speed("".into());
                                    ui.set_player_peer_count(0);
                                    ui.set_player_stream_progress(0.0);
                                }
                                if let Some((devices, loading)) = cast_devices {
                                    ui.set_player_cast_devices(slint::ModelRc::new(
                                        slint::VecModel::from(devices),
                                    ));
                                    ui.set_player_cast_devices_loading(loading);
                                }
                            }

                            let details_patch_allowed =
                                details_route_id.as_ref().is_some_and(|id| {
                                    navigation_for_sync.details_presentation(id).is_some()
                                });

                            if details_library_sync_needed && details_patch_allowed {
                                ui.set_detail_is_in_library(details_is_in_library);
                                ui.set_detail_is_watched(details_is_watched);
                                ui.set_detail_notifications_enabled(details_notifications_enabled);
                                ui.set_discover_preview_is_in_library(details_is_in_library);
                                ui.set_discover_preview_is_watched(details_is_watched);
                            }

                            // Sync stream links only for the route that requested them.
                            if details_stream_sync_needed && details_patch_allowed {
                                ui.set_detail_trailer_selection_id(
                                    trailer_selection_id.as_deref().unwrap_or_default().into(),
                                );
                                if let Ok(mut last_id_guard) = last_details_id_clone.lock()
                                    && *last_id_guard != details_route_id
                                {
                                    *last_id_guard = details_route_id.clone();
                                    if let Ok(mut fingerprint) =
                                        last_stream_fingerprint_clone.lock()
                                    {
                                        *fingerprint = None;
                                    }
                                }

                                let links_changed = last_stream_fingerprint_clone
                                    .lock()
                                    .map(|last| *last != stream_selection_fingerprint)
                                    .unwrap_or(true);
                                if links_changed {
                                    if let Ok(mut last) = last_stream_fingerprint_clone.lock() {
                                        *last = stream_selection_fingerprint;
                                    }
                                    project_stream_selection_views(
                                        &ui,
                                        &ui_weak_for_sync,
                                        stream_selection_views,
                                    );
                                    schedule_debrid_annotations(
                                        runtime_for_sync.clone(),
                                        playback_selections_for_sync.clone(),
                                        ui_weak_for_sync.clone(),
                                    );
                                }
                                if let Some(loading_count) = detail_stream_loading_count {
                                    ui.set_detail_stream_loading_count(loading_count);
                                }
                            }

                            // Sync submodels (only when needed AND viewing the corresponding active tab)
                            let active_tab = navigation_for_sync.active_tab_index();

                            if local_search_sync_needed
                                && let Some(local_search) = &local_search_cloned
                            {
                                models::search::sync_local_search(
                                    &ui,
                                    local_search,
                                    &ui_weak_for_sync,
                                );
                            }

                            if addon_details_sync_needed
                                && ui.get_addon_details_open()
                                && let Some(addon_details) = &addon_details_cloned
                            {
                                models::addons::sync_details(&ui, addon_details, &ui_weak_for_sync);
                            }
                            if board_sync_needed && active_tab != 0 {
                                tracing::warn!(
                                    "Out-of-focus tab rendering warning: Board sync requested but active tab is {}",
                                    active_tab
                                );
                            }
                            if discover_sync_needed && !discover_tab {
                                tracing::warn!(
                                    "Out-of-focus tab rendering warning: Discover sync requested but active tab is {}",
                                    active_tab
                                );
                            }
                            if library_sync_needed && active_tab != 2 {
                                tracing::warn!(
                                    "Out-of-focus tab rendering warning: Library sync requested but active tab is {}",
                                    active_tab
                                );
                            }
                            if addons_sync_needed && active_tab != 3 {
                                tracing::warn!(
                                    "Out-of-focus tab rendering warning: Addons sync requested but active tab is {}",
                                    active_tab
                                );
                            }
                            if calendar_sync_needed && active_tab != 5 {
                                tracing::warn!(
                                    "Out-of-focus tab rendering warning: Calendar sync requested but active tab is {}",
                                    active_tab
                                );
                            }
                            if search_sync_needed && active_tab != 6 {
                                tracing::warn!(
                                    "Out-of-focus tab rendering warning: Search sync requested but active tab is {}",
                                    active_tab
                                );
                            }

                            if active_tab == 0 && board_sync_needed {
                                if let (Some(cw), Some(brd), Some(ads)) =
                                    (&continue_watching_cloned, &board_cloned, &addons_cloned)
                                {
                                    models::board::sync(
                                        &ui,
                                        cw,
                                        brd,
                                        ads,
                                        &ui_weak_for_sync,
                                        &runtime_for_sync,
                                    );
                                }
                            } else if discover_tab && discover_sync_needed {
                                if let Some(disc) = &discover_cloned {
                                    models::discover::sync(
                                        &ui,
                                        disc,
                                        &ui_weak_for_sync,
                                        &runtime_for_sync,
                                        &navigation_for_sync,
                                    );
                                }
                            } else if active_tab == 2 && library_sync_needed {
                                if ui.get_library_continue_watching_mode() {
                                    if let Some(lib) = &continue_watching_library_cloned {
                                        models::library::sync(
                                            &ui,
                                            lib,
                                            &ui_weak_for_sync,
                                            &runtime_for_sync,
                                            true,
                                        );
                                    }
                                } else if let Some(lib) = &library_cloned {
                                    models::library::sync(
                                        &ui,
                                        lib,
                                        &ui_weak_for_sync,
                                        &runtime_for_sync,
                                        false,
                                    );
                                }
                            } else if active_tab == 3 && addons_sync_needed {
                                if let (Some(rem), Some(ads)) =
                                    (&remote_addons_cloned, &addons_cloned)
                                {
                                    models::addons::sync(
                                        &ui,
                                        rem,
                                        ads,
                                        &ui_weak_for_sync,
                                        &runtime_for_sync,
                                    );
                                }
                            } else if active_tab == 5 && calendar_sync_needed {
                                if let Some(calendar) = &calendar_cloned {
                                    models::calendar::sync(&ui, calendar, &ui_weak_for_sync);
                                }
                            } else if active_tab == 6
                                && search_sync_needed
                                && let (Some(search), Some(addons)) =
                                    (&search_cloned, &search_addons_cloned)
                            {
                                models::search::sync_results(
                                    &ui,
                                    search,
                                    addons,
                                    &ui_weak_for_sync,
                                );
                            }

                            if calendar_refresh_started
                                && navigation_for_sync.active_tab_index() == 5
                            {
                                ui.set_calendar_loading(true);
                            }

                            if details_sync_needed
                                && details_patch_allowed
                                && let Some(det) = &meta_details_cloned
                            {
                                models::details::sync(
                                    &ui,
                                    det,
                                    details_is_in_library,
                                    &ui_weak_for_sync,
                                    &runtime_for_sync,
                                    &navigation_for_sync,
                                );
                            }

                            if settings_sync_needed
                                && let Some((
                                    settings,
                                    user_id,
                                    trakt_authenticated,
                                    server_urls,
                                    streaming_settings,
                                )) = &settings_cloned
                            {
                                models::settings::sync(
                                    &ui,
                                    settings,
                                    user_id,
                                    *trakt_authenticated,
                                    server_urls,
                                    streaming_settings.as_ref(),
                                );
                            }
                            if data_export_sync_needed
                                && let Some(data_export) = &data_export_cloned
                            {
                                models::settings::sync_data_export(
                                    &ui,
                                    data_export,
                                    &runtime_for_sync,
                                );
                            }
                        }
                        crate::performance::counters().record_ui_patch(patch_started.elapsed());
                    });
                    #[cfg(debug_assertions)]
                    tracing::trace!(
                        elapsed_micros = state_started.elapsed().as_micros(),
                        "core state projected and queued"
                    );
                }
                RuntimeEvent::CoreEvent(event) => handle_core_event(event, &ui_weak),
            }
        }
    });
}

fn handle_core_event(event: Event, ui_weak: &slint::Weak<MainWindow>) {
    if let Event::Error { error, .. } = &event {
        tracing::warn!("core error event received");
        let ui_weak = ui_weak.clone();
        let message = format!("{error:?}");
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_error_message(message.into());
                ui.set_loading(false);
                ui.set_details_loading(false);
            }
        });
    } else {
        // Core event payloads can contain credentials, profile identifiers,
        // and very large library ID lists. The state projections below provide
        // actionable diagnostics without serializing those payloads to disk.
        tracing::debug!("core event received");
    }
}
