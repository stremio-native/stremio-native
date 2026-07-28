#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

slint::include_modules!();

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context as _;
use stremio_core::{
    constants::{
        DISMISSED_EVENTS_STORAGE_KEY, LIBRARY_RECENT_STORAGE_KEY, LIBRARY_STORAGE_KEY,
        NOTIFICATIONS_STORAGE_KEY, PROFILE_STORAGE_KEY, SEARCH_HISTORY_STORAGE_KEY,
        STREAMING_SERVER_URLS_STORAGE_KEY, STREAMS_STORAGE_KEY,
    },
    models::{
        calendar::Calendar,
        catalog_with_filters::CatalogWithFilters,
        catalogs_with_extra::CatalogsWithExtra,
        continue_watching_preview::ContinueWatchingPreview,
        ctx::Ctx,
        installed_addons_with_filters::InstalledAddonsWithFilters,
        library_with_filters::{ContinueWatchingFilter, LibraryWithFilters, NotRemovedFilter},
        local_search::LocalSearch,
        player::Player,
        streaming_server::StreamingServer,
    },
    runtime::{Env, Runtime},
    types::{addon::Descriptor, resource::MetaItemPreview},
};

use core_env::DesktopEnv;

mod config;
pub mod db;
mod deep_link;
mod gpu_prewarm;
pub mod image_cache;
mod media_session;
mod models;
mod mpv_integration;
mod paths;
mod performance;
mod playback;
#[cfg(feature = "plugins")]
mod plugins;
mod shaders;
mod shortcuts;
mod single_instance;
mod taskbar_media;
mod thumbnail_preview;
mod tray;
mod window_events;
mod window_style;

// Modular sub-files
mod app_model;
mod callbacks;
mod discord;
mod event_loop;
mod logger;
mod navigation;
mod theintrodb;
mod updater;

// Re-exports/Usage
pub use app_model::{AppModel, AppModelField};
pub use discord::DiscordRpc;
pub use navigation::{DetailsPresentation, NavigationController, NavigationIntent, Tab};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let startup_started = Instant::now();
    // Core callbacks may originate on native threads (notably libmpv's actor),
    // so register the process runtime before any model or playback work starts.
    core_env::install_runtime_handle(tokio::runtime::Handle::current());

    let profile = performance::ProfileConfig::from_args(std::env::args());

    let primary_instance = match single_instance::acquire(std::env::args_os()).await? {
        single_instance::InstanceStartup::Primary(instance) => instance,
        single_instance::InstanceStartup::Forwarded => return Ok(()),
    };

    // A forwarded process exits before touching the primary process's log.
    let app_paths = paths::initialize()?;
    let _guards = logger::init_logger(&profile, app_paths)?;
    tracing::info!(data_root = %app_paths.root().display(), "Starting Stremio-Rust GUI client...");

    let res = run_app(&profile, primary_instance, startup_started).await;
    if let Err(ref e) = res {
        tracing::error!(error = ?e, "Stremio-Rust execution failed with error");
        let _ = db::insert_log("ERROR", &format!("Application crash: {:?}", e)).await;
    }
    res
}

async fn run_app(
    profile_config: &performance::ProfileConfig,
    primary_instance: single_instance::PrimaryInstance,
    startup_started: Instant,
) -> anyhow::Result<()> {
    let _run_span = tracing::info_span!("run_app").entered();
    let single_instance::PrimaryInstance {
        initial_command,
        commands,
        start_hidden,
    } = primary_instance;

    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("skia-opengl".into())
        .require_opengl()
        .select()
        .context("could not select Slint's Skia OpenGL renderer")?;
    tracing::info!(
        backend = "winit",
        renderer = "skia-opengl",
        opengl_version_policy = "highest-available-desktop",
        "Slint backend selected"
    );
    let initial_config = config::AppConfig::default();

    // Icon fonts are registered/embedded at compile time via app.slint imports.
    tracing::info!("Icon fonts registered at compile time.");

    // Establish the process identity before the window (and thus the media
    // controls) exists, so the OS media overlay can attribute playback to us.
    window_style::set_app_user_model_id();

    // 5. Initialize Slint MainWindow UI
    let ui = MainWindow::new()?;
    tracing::info!("MainWindow created");

    // Apply Dynamic Theme to Slint Global Theme Singleton
    apply_theme(&ui, &initial_config);

    // Set initial configuration parameters
    let navigation = NavigationController::new(initial_config.active_tab);
    navigation.project(&ui);
    ui.set_settings_application_version(env!("CARGO_PKG_VERSION").into());
    ui.set_settings_build_version(env!("STREMIO_BUILD_VERSION").into());
    ui.set_settings_shell_version(env!("CARGO_PKG_VERSION").into());
    ui.set_settings_hardware_acceleration(initial_config.hardware_acceleration);
    ui.set_settings_thumbnail_previews(initial_config.thumbnail_previews_enabled);
    ui.set_settings_tidb_show_intro(initial_config.tidb_show_intro);
    ui.set_settings_tidb_show_recap(initial_config.tidb_show_recap);
    ui.set_settings_tidb_show_credits(initial_config.tidb_show_credits);
    ui.set_settings_tidb_show_preview(initial_config.tidb_show_preview);
    ui.set_loading(true);
    let os_media_session = Arc::new(media_session::MediaSession::for_window(&ui));
    shortcuts::install_platform_shortcuts(&ui, os_media_session.clone());

    // Request the native window before scheduling any optional shell service or
    // application-engine work. The event loop below owns first-paint priority.
    tracing::info!(
        start_hidden,
        shell_ready_ms = startup_started.elapsed().as_millis(),
        "Stremio client shell is ready"
    );
    if !start_hidden {
        ui.show()?;
    }

    let (installer_request, installer_launcher) = updater::installer_handoff();
    let startup_ui = ui.clone_strong();
    let startup_navigation = navigation.clone();
    let startup_handle = slint::spawn_local(async move {
        // Ensure the native event loop can paint the loading shell before even
        // small synchronous setup such as icon lookup or tray creation begins.
        tokio::time::sleep(Duration::from_millis(1)).await;
        let tray = match tray::setup(&startup_ui, &startup_navigation) {
            Ok(tray) => Some(tray),
            Err(error) => {
                tracing::warn!(%error, "system tray is unavailable; continuing with the GUI");
                None
            }
        };
        let updater = updater::setup(&startup_ui, tray.as_ref(), installer_request);
        let failure_ui = startup_ui.as_weak();
        let result = finish_startup(
            startup_ui,
            startup_navigation,
            initial_command,
            commands,
            tray,
            updater,
            os_media_session,
        )
        .await;
        if let Err(error) = &result {
            tracing::error!(%error, "application startup failed after opening the window");
            if let Some(ui) = failure_ui.upgrade() {
                ui.set_loading(false);
                ui.set_error_message(error.to_string().into());
            }
        }
        result
    })?;

    let performance_reporter = profile_config
        .mode
        .enabled()
        .then(performance::spawn_reporter)
        .flatten();

    tracing::info!("Stremio-Rust GUI loop starting...");
    let ui_result = tokio::task::block_in_place(slint::run_event_loop);
    if let Some(reporter) = performance_reporter {
        reporter.abort();
    }
    let startup_result = if startup_handle.is_finished() {
        Some(startup_handle.await)
    } else {
        startup_handle.abort();
        None
    };
    let hide_result = ui.hide();
    drop(ui);

    let session_result = match startup_result {
        Some(result) => result.and_then(AppSession::shutdown),
        None => Ok(()),
    };
    let installer_result = installer_launcher.launch_pending();

    ui_result?;
    hide_result?;
    session_result?;
    installer_result
}

struct AppSession {
    server_handle: stream_server::ServerHandle,
    native_playback: Option<mpv_integration::NativePlayback>,
    command_task: tokio::task::JoinHandle<()>,
    tray: Option<AppTray>,
    updater: updater::UpdaterHandle,
}

impl AppSession {
    fn shutdown(mut self) -> anyhow::Result<()> {
        self.updater.shutdown();
        // The event loop has ended, so changing the tray's visibility would
        // write to a finalized Slint property. Dropping it removes the native
        // icon without re-entering Slint's property system.
        drop(self.tray.take());
        self.command_task.abort();
        let playback_result = match self.native_playback.take() {
            Some(playback) => playback.shutdown(),
            None => Ok(()),
        };
        if let Err(error) = self.server_handle.shutdown() {
            tracing::warn!(%error, "stream-server was already stopped");
        }
        let server_result = self.server_handle.join();

        playback_result?;
        match server_result? {
            Some(source) => tracing::info!(?source, "stream-server stopped"),
            None => tracing::info!("stream-server stopped"),
        }
        Ok(())
    }
}

fn apply_theme(ui: &MainWindow, config: &config::AppConfig) {
    let theme = ui.global::<Theme>();
    macro_rules! set_color {
        ($value:expr, $setter:ident) => {
            if let Some(color) = config::parse_color($value) {
                theme.$setter(color);
            }
        };
    }

    set_color!(&config.theme.background, set_background);
    set_color!(&config.theme.secondary_background, set_secondary_background);
    set_color!(&config.theme.sidebar_background, set_sidebar_background);
    set_color!(&config.theme.modal_background, set_modal_background);
    set_color!(&config.theme.drawer_background, set_drawer_background);
    set_color!(&config.theme.control_background, set_control_background);
    set_color!(&config.theme.overlay, set_overlay);
    set_color!(&config.theme.overlay_hover, set_overlay_hover);
    set_color!(&config.theme.overlay_pressed, set_overlay_pressed);
    set_color!(&config.theme.divider, set_divider);
    set_color!(&config.theme.scrim, set_scrim);
    set_color!(&config.theme.accent, set_accent);
    set_color!(&config.theme.accent_hover, set_accent_hover);
    set_color!(&config.theme.success, set_success);
    set_color!(&config.theme.warning, set_warning);
    set_color!(&config.theme.info, set_info);
    set_color!(&config.theme.danger, set_danger);
    set_color!(&config.theme.focus, set_focus);
    set_color!(&config.theme.title_bar, set_title_bar);
    set_color!(&config.theme.card_background, set_card_background);
    set_color!(&config.theme.card_border, set_card_border);
    set_color!(&config.theme.text_primary, set_text_primary);
    set_color!(&config.theme.text_secondary, set_text_secondary);
    set_color!(&config.theme.text_muted, set_text_muted);
    set_color!(&config.theme.skeleton_base, set_skeleton_base);
    set_color!(&config.theme.skeleton_shimmer, set_skeleton_shimmer);
}

async fn finish_startup(
    ui: MainWindow,
    navigation: NavigationController,
    initial_command: Option<single_instance::AppCommand>,
    commands: tokio::sync::mpsc::UnboundedReceiver<single_instance::AppCommand>,
    tray: Option<AppTray>,
    updater: updater::UpdaterHandle,
    media_session: Arc<media_session::MediaSession>,
) -> anyhow::Result<AppSession> {
    let ui_weak = ui.as_weak();

    db::init_db(paths::get().database()).await?;
    config::init_config().await;
    let config = config::load_config();
    apply_theme(&ui, &config);
    ui.set_settings_hardware_acceleration(config.hardware_acceleration);
    ui.set_settings_thumbnail_previews(config.thumbnail_previews_enabled);
    ui.set_settings_tidb_api_key(config.tidb_api_key.clone().into());
    ui.set_settings_tidb_show_intro(config.tidb_show_intro);
    ui.set_settings_tidb_show_recap(config.tidb_show_recap);
    ui.set_settings_tidb_show_credits(config.tidb_show_credits);
    ui.set_settings_tidb_show_preview(config.tidb_show_preview);
    if let Ok(active_tab) = Tab::try_from(config.active_tab) {
        navigation.dispatch_and_project(&ui, NavigationIntent::SelectTab(active_tab));
    }

    let server_cfg = stream_server::ServerConfig {
        http_addr: std::net::SocketAddr::from(([127, 0, 0, 1], config.torrent_port)),
        config_dir: Some(paths::get().streaming_server().to_path_buf()),
        cache_dir: Some(paths::get().streaming_server_cache().to_path_buf()),
        print_startup: true,
        init_logging: false,
        ..stream_server::ServerConfig::embedded()
    };
    tracing::info!("launching stream-server engine");
    let server_task = tokio::task::spawn_blocking(move || stream_server::start(server_cfg));
    let storage_task = tokio::spawn(load_startup_storage());
    let playback_files_task = tokio::spawn(mpv_integration::prepare_playback_files());

    // Await these independent tasks from Slint's local executor so the event
    // loop keeps painting and processing input while the loading UI is visible.
    let server_handle = server_task
        .await
        .map_err(|error| anyhow::anyhow!("stream-server startup task failed: {error}"))?
        .map_err(|error| anyhow::anyhow!("failed to start stream-server: {error}"))?;
    let mut startup_storage = storage_task
        .await
        .map_err(|error| anyhow::anyhow!("storage startup task failed: {error}"))?;
    let server_url = format!("http://{}", server_handle.http_addr());
    startup_storage.profile.settings.streaming_server_url = url::Url::parse(&server_url)?;
    ui.set_server_url(server_url.into());
    ui.set_server_status("Online".into());

    let StartupStorage {
        profile,
        library,
        streams_bucket,
        server_urls,
        notifications,
        search_history,
        dismissed_events,
    } = startup_storage;

    let (continue_watching_preview, continue_watching_preview_effects) =
        ContinueWatchingPreview::new(&library, &notifications);
    let (discover, discover_effects) = CatalogWithFilters::<MetaItemPreview>::new(&profile);
    let (library_, library_effects) =
        LibraryWithFilters::<NotRemovedFilter>::new(&library, &notifications);
    let (continue_watching, continue_watching_effects) =
        LibraryWithFilters::<ContinueWatchingFilter>::new(&library, &notifications);
    let (remote_addons, remote_addons_effects) = CatalogWithFilters::<Descriptor>::new(&profile);
    let (installed_addons, installed_addons_effects) = InstalledAddonsWithFilters::new(&profile);
    let (streaming_server, streaming_server_effects) = StreamingServer::new::<DesktopEnv>(&profile);
    let (local_search, local_search_effects) = LocalSearch::new::<DesktopEnv>();

    let model = AppModel {
        ctx: Ctx::new(
            profile,
            library,
            streams_bucket,
            server_urls,
            notifications,
            search_history,
            dismissed_events,
        ),
        auth_link: Default::default(),
        data_export: Default::default(),
        continue_watching_preview,
        board: CatalogsWithExtra::default(),
        discover,
        library: library_,
        continue_watching,
        search: CatalogsWithExtra::default(),
        local_search,
        calendar: Calendar::default(),
        meta_details: Default::default(),
        player: Player {
            collect_seek_logs: true,
            ..Default::default()
        },
        remote_addons,
        installed_addons,
        addon_details: Default::default(),
        streaming_server,
    };

    let mut all_effects = Vec::new();
    all_effects.extend(continue_watching_preview_effects);
    all_effects.extend(discover_effects);
    all_effects.extend(library_effects);
    all_effects.extend(continue_watching_effects);
    all_effects.extend(remote_addons_effects);
    all_effects.extend(installed_addons_effects);
    all_effects.extend(streaming_server_effects);
    all_effects.extend(local_search_effects);

    let (runtime, rx) = Runtime::<DesktopEnv, _>::new(model, all_effects, 1000);
    let runtime = Arc::new(runtime);

    {
        let ui_weak_refresh = ui_weak.clone();
        image_cache::set_refresh_callback(move |completed_urls| {
            if let Some(ui) = ui_weak_refresh.upgrade() {
                models::refresh_cached_media_images(&ui, &completed_urls);
            }
        });
    }
    ui.on_request_poster(|url| image_cache::request_image(url.as_str()));

    {
        let rt = runtime.clone();
        let ui_weak_tab = ui_weak.clone();
        let navigation_tab = navigation.clone();
        ui.on_tab_changed(move |tab| {
            let _tab_span = tracing::info_span!("Tab_Changed", tab = tab).entered();
            tracing::info!(tab, "active tab changed by user");
            let Ok(selected_tab) = Tab::try_from(tab) else {
                tracing::warn!(tab, "ignoring invalid tab navigation");
                return;
            };
            if let Some(ui) = ui_weak_tab.upgrade() {
                navigation_tab.dispatch_and_project(&ui, NavigationIntent::SelectTab(selected_tab));
                ui.set_loading(false);
                sync_tab_from_model(selected_tab, &rt, &ui, &ui_weak_tab, &navigation_tab);
            }
            if selected_tab == Tab::Calendar {
                let loading = models::calendar::ensure_loaded(&rt);
                if loading && let Some(ui) = ui_weak_tab.upgrade() {
                    ui.set_calendar_loading(true);
                }
            }
        });
    }

    let discord_rpc = Arc::new(discord::DiscordRpc::new());
    let playback_selections = Arc::new(playback::PlaybackSelections::default());
    let hardware_decoding = runtime
        .model()
        .ok()
        .map(|model| model.ctx.profile.settings.hardware_decoding)
        .unwrap_or(config.hardware_acceleration);
    let prepared_playback_files = match playback_files_task.await {
        Ok(Ok(files)) => Some(files),
        Ok(Err(error)) => {
            tracing::error!(%error, "native MPV file preparation failed");
            None
        }
        Err(error) => {
            tracing::error!(%error, "native MPV file preparation task stopped");
            None
        }
    };
    let native_playback = prepared_playback_files.and_then(|prepared_files| {
        match mpv_integration::NativePlayback::start(
            &ui,
            &runtime,
            hardware_decoding,
            navigation.clone(),
            discord_rpc.clone(),
            media_session.clone(),
            tokio::runtime::Handle::current(),
            prepared_files,
        ) {
            Ok(playback) => Some(playback),
            Err(error) => {
                tracing::error!(%error, "native MPV playback is unavailable");
                None
            }
        }
    });
    let native_playback_bridge = native_playback
        .as_ref()
        .map(mpv_integration::NativePlayback::bridge);

    event_loop::start_event_loop(
        rx,
        runtime.clone(),
        ui_weak.clone(),
        playback_selections.clone(),
        native_playback_bridge.clone(),
        navigation.clone(),
        discord_rpc,
    );
    callbacks::setup_ui_callbacks(
        &ui,
        &runtime,
        &playback_selections,
        &native_playback_bridge,
        ui_weak.clone(),
        &config,
        navigation.clone(),
    );
    window_events::install(&ui, runtime.clone());

    // Plugin system (lazy: only starts if plugins directory has .lua files)
    #[cfg(feature = "plugins")]
    let _plugin_manager = {
        let plugin_dir = crate::paths::get().plugins().to_path_buf();
        let pm = plugins::PluginManager::new(ui_weak.clone(), plugin_dir);
        if let Some(ref pm) = pm {
            let tx = pm.sender();
            ui.on_plugin_run_action(move |action_id| {
                let _ = tx.try_send(plugins::LuaEvent::RunAction(action_id.to_string()));
            });
        }
        pm
    };

    if let Ok(initial_tab) = Tab::try_from(navigation.active_tab_index()) {
        sync_tab_from_model(initial_tab, &runtime, &ui, &ui_weak, &navigation);
        if initial_tab == Tab::Calendar && models::calendar::ensure_loaded(&runtime) {
            ui.set_calendar_loading(true);
        }
    }
    ui.set_loading(false);
    callbacks::trigger_initial_load(&runtime);
    let command_task = deep_link::start_command_receiver(
        commands,
        ui.as_weak(),
        runtime.clone(),
        navigation.clone(),
    );
    if let Some(command) = initial_command {
        deep_link::handle(command, &ui, &runtime, &navigation);
    }
    tracing::info!("background application startup completed");
    Ok(AppSession {
        server_handle,
        native_playback,
        command_task,
        tray,
        updater,
    })
}

struct StartupStorage {
    profile: stremio_core::types::profile::Profile,
    library: stremio_core::types::library::LibraryBucket,
    streams_bucket: stremio_core::types::streams::StreamsBucket,
    server_urls: stremio_core::types::server_urls::ServerUrlsBucket,
    notifications: stremio_core::types::notifications::NotificationsBucket,
    search_history: stremio_core::types::search_history::SearchHistoryBucket,
    dismissed_events: stremio_core::types::events::DismissedEventsBucket,
}

#[tracing::instrument]
async fn load_startup_storage() -> StartupStorage {
    let (
        profile_result,
        library_recent_result,
        library_result,
        streams_result,
        server_urls_result,
        legacy_server_urls_result,
        notifications_result,
        search_history_result,
        dismissed_events_result,
    ) = tokio::join!(
        DesktopEnv::get_storage::<stremio_core::types::profile::Profile>(PROFILE_STORAGE_KEY),
        DesktopEnv::get_storage::<stremio_core::types::library::LibraryBucket>(
            LIBRARY_RECENT_STORAGE_KEY
        ),
        DesktopEnv::get_storage::<stremio_core::types::library::LibraryBucket>(LIBRARY_STORAGE_KEY),
        DesktopEnv::get_storage::<stremio_core::types::streams::StreamsBucket>(STREAMS_STORAGE_KEY),
        DesktopEnv::get_storage::<stremio_core::types::server_urls::ServerUrlsBucket>(
            STREAMING_SERVER_URLS_STORAGE_KEY
        ),
        DesktopEnv::get_storage::<stremio_core::types::server_urls::ServerUrlsBucket>(
            "server_urls"
        ),
        DesktopEnv::get_storage::<stremio_core::types::notifications::NotificationsBucket>(
            NOTIFICATIONS_STORAGE_KEY
        ),
        DesktopEnv::get_storage::<stremio_core::types::search_history::SearchHistoryBucket>(
            SEARCH_HISTORY_STORAGE_KEY
        ),
        DesktopEnv::get_storage::<stremio_core::types::events::DismissedEventsBucket>(
            DISMISSED_EVENTS_STORAGE_KEY
        ),
    );

    let profile = storage_value(PROFILE_STORAGE_KEY, profile_result).unwrap_or_default();
    let mut library = stremio_core::types::library::LibraryBucket::new(profile.uid(), vec![]);
    if let Some(recent_bucket) = storage_value(LIBRARY_RECENT_STORAGE_KEY, library_recent_result) {
        library.merge_bucket(recent_bucket);
    }
    if let Some(other_bucket) = storage_value(LIBRARY_STORAGE_KEY, library_result) {
        library.merge_bucket(other_bucket);
    }
    let streams_bucket = storage_value(STREAMS_STORAGE_KEY, streams_result)
        .unwrap_or_else(|| stremio_core::types::streams::StreamsBucket::new(profile.uid()));
    let server_urls = storage_value(STREAMING_SERVER_URLS_STORAGE_KEY, server_urls_result)
        .or_else(|| storage_value("server_urls", legacy_server_urls_result))
        .unwrap_or_else(|| {
            stremio_core::types::server_urls::ServerUrlsBucket::new::<DesktopEnv>(profile.uid())
        });
    let notifications = storage_value(NOTIFICATIONS_STORAGE_KEY, notifications_result)
        .unwrap_or_else(|| {
            stremio_core::types::notifications::NotificationsBucket::new::<DesktopEnv>(
                profile.uid(),
                vec![],
            )
        });
    let search_history = storage_value(SEARCH_HISTORY_STORAGE_KEY, search_history_result)
        .unwrap_or_else(|| {
            stremio_core::types::search_history::SearchHistoryBucket::new(profile.uid())
        });
    let dismissed_events = storage_value(DISMISSED_EVENTS_STORAGE_KEY, dismissed_events_result)
        .unwrap_or_else(|| stremio_core::types::events::DismissedEventsBucket::new(profile.uid()));

    tracing::info!(
        addons_count = profile.addons.len(),
        library_items_count = library.items.len(),
        notifications_count = notifications.items.len(),
        search_history_count = search_history.items.len(),
        "startup storage hydrated"
    );

    StartupStorage {
        profile,
        library,
        streams_bucket,
        server_urls,
        notifications,
        search_history,
        dismissed_events,
    }
}

fn storage_value<T>(
    key: &str,
    result: Result<Option<T>, stremio_core::runtime::EnvError>,
) -> Option<T> {
    match result {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, key, "startup storage bucket could not be read");
            None
        }
    }
}

fn sync_tab_from_model(
    tab: Tab,
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
    ui: &MainWindow,
    ui_weak: &slint::Weak<MainWindow>,
    navigation: &NavigationController,
) {
    if navigation.active_tab_index() != tab.index() {
        return;
    }

    match tab {
        Tab::Board => {
            let snapshot = runtime.model().ok().map(|model| {
                (
                    model.continue_watching_preview.clone(),
                    model.board.clone(),
                    model.ctx.profile.addons.clone(),
                )
            });
            if let Some((continue_watching, board, addons)) = snapshot {
                models::board::sync(ui, &continue_watching, &board, &addons, ui_weak, runtime);
            }
        }
        Tab::Discover => {
            crate::models::discover::clear_sync_state();
            if let Some(discover) = runtime.model().ok().map(|model| model.discover.clone()) {
                models::discover::sync(ui, &discover, ui_weak, runtime, navigation);
            }
        }
        Tab::Library => {
            crate::models::library::clear_sync_state();
            let continue_watching = ui.get_library_continue_watching_mode();
            let snapshot = runtime
                .model()
                .ok()
                .map(|model| (model.library.clone(), model.continue_watching.clone()));
            if let Some((library, continue_watching_library)) = snapshot {
                if continue_watching {
                    models::library::sync(ui, &continue_watching_library, ui_weak, runtime, true);
                } else {
                    models::library::sync(ui, &library, ui_weak, runtime, false);
                }
            }
        }
        Tab::Addons => {
            let snapshot = runtime.model().ok().map(|model| {
                (
                    model.remote_addons.clone(),
                    model.ctx.profile.addons.clone(),
                )
            });
            if let Some((remote_addons, installed_addons)) = snapshot {
                models::addons::sync(ui, &remote_addons, &installed_addons, ui_weak, runtime);
            }
        }
        Tab::Calendar => {
            if let Some(calendar) = runtime.model().ok().map(|model| model.calendar.clone()) {
                models::calendar::sync(ui, &calendar, ui_weak);
            }
        }
        Tab::Settings => {
            let snapshot = runtime.model().ok().map(|model| {
                (
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
                        stremio_core::models::common::Loadable::Ready(settings) => {
                            Some(settings.clone())
                        }
                        _ => None,
                    },
                )
            });
            if let Some((settings, user_id, trakt, server_urls, streaming_settings)) = snapshot {
                models::settings::sync(
                    ui,
                    &settings,
                    &user_id,
                    trakt,
                    &server_urls,
                    streaming_settings.as_ref(),
                );
            }
        }
    }
}
