#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(feature = "profiling"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

slint::include_modules!();

use std::{
    path::PathBuf,
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

pub mod backup;
mod color_picker;
pub mod community_addons;
mod config;
pub mod crash_handler;
pub mod customization;
pub mod db;
mod deep_link;
pub mod diagnostics;
pub mod downloads;
mod gpu_prewarm;
pub mod image_cache;
pub mod integrations;
pub mod local_library;
pub mod localization;
mod lock_handler;
mod media_session;
pub mod metadata_enrichment;
mod models;
mod mpv_integration;
pub mod network_tools;
mod paths;
mod performance;
mod playback;
mod player_features;
#[cfg(feature = "plugins")]
mod plugins;
mod preview_player;
pub mod profiles;
pub mod ratings;
pub mod runtime_host;
pub mod secure_settings;
mod shaders;
mod shortcuts;
mod single_instance;
mod taskbar_media;
mod theme_studio;
mod thumbnail_preview;
mod tray;
mod window_events;
mod window_hooks;
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

fn main() -> anyhow::Result<()> {
    if crash_handler::run_helper_from_args()? {
        return Ok(());
    }

    run_application()
}

#[tokio::main]
#[cfg_attr(feature = "profiling", hotpath::main)]
async fn run_application() -> anyhow::Result<()> {
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
    color_picker::install(&ui);
    theme_studio::install(&ui);

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
    tray: Option<tray::Tray>,
    updater: updater::UpdaterHandle,
    download_ui_task: tokio::task::JoinHandle<()>,
}

impl AppSession {
    fn shutdown(mut self) -> anyhow::Result<()> {
        self.updater.shutdown();
        // The event loop has ended, so changing the tray's visibility would
        // write to a finalized Slint property. Dropping it removes the native
        // icon without re-entering Slint's property system.
        drop(self.tray.take());
        self.command_task.abort();
        self.download_ui_task.abort();
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
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                db::close_and_checkpoint_db().await;
            });
        });
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
    if !config.theme.font_family.is_empty() {
        theme.set_font_family(config.theme.font_family.as_str().into());
    }
    theme.set_active_preset_idx(config.theme.active_preset_idx);
}

async fn finish_startup(
    ui: MainWindow,
    navigation: NavigationController,
    initial_command: Option<single_instance::AppCommand>,
    commands: tokio::sync::mpsc::UnboundedReceiver<single_instance::AppCommand>,
    tray: Option<tray::Tray>,
    updater: updater::UpdaterHandle,
    media_session: Arc<media_session::MediaSession>,
) -> anyhow::Result<AppSession> {
    let ui_weak = ui.as_weak();

    lock_handler::init_db_with_lock_handling(paths::get().database()).await?;
    let credential_store: Arc<dyn credential_store::CredentialStore> =
        Arc::new(credential_store::PlatformCredentialStore::default());
    core_env::install_credential_store(credential_store.clone());
    secure_settings::install(credential_store.clone());
    integrations::install(credential_store.clone());
    let download_manager = Arc::new(downloads::DownloadManager::new(credential_store, 2));
    let active_profile_id = profiles::active_profile_id()
        .await
        .context("could not load the active local profile")?;
    let local_profiles = profiles::list_profiles()
        .await
        .context("could not load local profiles")?;
    let active_profile_id = select_startup_profile(&ui, local_profiles, active_profile_id).await?;
    core_env::set_active_profile_scope(active_profile_id.as_str());
    config::init_config().await;
    let mut config = config::load_config();
    download_manager.set_bandwidth_limit(
        (config.download_bandwidth_limit_bps > 0).then_some(config.download_bandwidth_limit_bps),
    );
    let legacy_tidb_key = (!config.tidb_api_key.is_empty()).then(|| config.tidb_api_key.clone());
    secure_settings::activate_profile(active_profile_id.as_str(), legacy_tidb_key)
        .await
        .context("could not load the profile credential settings")?;
    if !config.tidb_api_key.is_empty() {
        config.tidb_api_key.clear();
        config::save_config_async(&config)
            .await
            .context("could not remove a migrated provider key from local storage")?;
    }
    apply_theme(&ui, &config);
    ui.set_settings_hardware_acceleration(config.hardware_acceleration);
    ui.set_settings_thumbnail_previews(config.thumbnail_previews_enabled);
    ui.set_settings_tidb_api_key(if secure_settings::has_tidb_api_key() {
        "••••••••••••".into()
    } else {
        "".into()
    });
    ui.set_settings_tidb_show_intro(config.tidb_show_intro);
    ui.set_settings_tidb_show_recap(config.tidb_show_recap);
    ui.set_settings_tidb_show_credits(config.tidb_show_credits);
    ui.set_settings_tidb_show_preview(config.tidb_show_preview);
    ui.set_operations_region(config.region.clone().into());
    ui.set_operations_bandwidth_mbps(
        if config.download_bandwidth_limit_bps == 0 {
            "0".to_owned()
        } else {
            format!(
                "{:.1}",
                config.download_bandwidth_limit_bps as f64 * 8.0 / 1_000_000.0
            )
        }
        .into(),
    );
    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    ui.set_operations_backup_path(
        paths::get()
            .root()
            .join("backups")
            .join(format!("stremio-native-{timestamp}.zip"))
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    ui.set_operations_diagnostic_path(
        paths::get()
            .root()
            .join("diagnostics")
            .join(format!("diagnostics-{timestamp}.zip"))
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    load_profile_customization(&ui, &active_profile_id).await;
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
                match selected_tab {
                    Tab::Movies => ui.invoke_discover_type_changed("movie".into()),
                    Tab::Shows => ui.invoke_discover_type_changed("series".into()),
                    Tab::Anime => ui.invoke_discover_type_changed("anime".into()),
                    Tab::Kids => {
                        ui.invoke_discover_type_changed("movie".into());
                        let weak = ui_weak_tab.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(150)).await;
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = weak.upgrade() {
                                    ui.invoke_discover_genre_changed("Family".into());
                                }
                            });
                        });
                    }
                    _ => {}
                }
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
    if let Ok(Some(mode)) = profiles::setting(&active_profile_id, "stream-ranking-mode").await {
        let ranking_mode = match mode.as_str() {
            "Quality" => stream_ranking::RankingMode::Quality,
            "Smallest" => stream_ranking::RankingMode::Smallest,
            "Seeders" => stream_ranking::RankingMode::Seeders,
            "Original" => stream_ranking::RankingMode::Original,
            _ => stream_ranking::RankingMode::Smart,
        };
        playback_selections.set_ranking_mode(ranking_mode);
        ui.set_detail_ranking_mode(mode.into());
    }
    if matches!(
        profiles::setting(&active_profile_id, "stream-show-filtered").await,
        Ok(Some(value)) if value == "true"
    ) {
        playback_selections.set_show_filtered(true);
        ui.set_detail_show_filtered(true);
    }
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

    // The hover popup is only offered when the secondary preview engine came
    // up with native playback; without it the card would never fill in.
    match native_playback_bridge.as_ref() {
        Some(bridge) => {
            let previews = bridge.previews();
            ui.set_hover_preview_enabled(previews.is_enabled());
            ui.set_hover_preview_delay_ms(
                i32::try_from(config.hover_trailer_preview_delay_ms)
                    .unwrap_or(crate::config::DEFAULT_HOVER_PREVIEW_DELAY_MS as i32),
            );
            event_loop::install_hover_preview_callbacks(
                &ui,
                runtime.clone(),
                playback_selections.clone(),
                previews,
                navigation.clone(),
            );
        }
        None => ui.set_hover_preview_enabled(false),
    }

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
        &config,
        navigation.clone(),
        download_manager.clone(),
    );
    downloads::setup_ui_callbacks(
        &ui,
        download_manager.clone(),
        native_playback_bridge.clone(),
        navigation.clone(),
    );
    local_library::setup(&ui, native_playback_bridge.clone(), navigation.clone());
    setup_live_profile_switch(
        &ui,
        runtime.clone(),
        native_playback_bridge.clone(),
        playback_selections.clone(),
        navigation.clone(),
        download_manager.clone(),
    );
    setup_profile_management(&ui);
    setup_integration_management(&ui);
    setup_operations(&ui, download_manager.clone());
    let mut download_progress = download_manager.subscribe();
    let download_ui_weak = ui.as_weak();
    let download_ui_task = tokio::spawn(async move {
        while download_progress.changed().await.is_ok() {
            tokio::time::sleep(Duration::from_millis(250)).await;
            downloads::project_active_profile(download_ui_weak.clone()).await;
        }
    });
    {
        let download_manager = download_manager.clone();
        let profile_id = active_profile_id.clone();
        tokio::spawn(async move {
            if let Err(error) = download_manager.resume_profile(profile_id.as_str()).await {
                tracing::warn!(%error, "could not resume persisted downloads");
            }
        });
    }
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
        download_ui_task,
    })
}

fn setup_live_profile_switch(
    ui: &MainWindow,
    runtime: Arc<Runtime<DesktopEnv, AppModel>>,
    playback: Option<mpv_integration::NativePlaybackBridge>,
    playback_selections: Arc<playback::PlaybackSelections>,
    navigation: NavigationController,
    download_manager: Arc<downloads::DownloadManager>,
) {
    let limiter = Arc::new(profiles::PinAttemptLimiter::default());
    let pending_confirmation = Arc::new(std::sync::Mutex::new(None::<String>));
    let ui_weak = ui.as_weak();
    ui.on_profile_picker_selected(move |profile_id, pin| {
        let Ok(target_id) = profiles::ProfileId::parse(profile_id.to_string()) else {
            return;
        };
        let target_id_string = target_id.as_str().to_owned();
        if navigation.is_player_visible() {
            let mut pending = pending_confirmation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if pending.as_deref() != Some(target_id.as_str()) {
                *pending = Some(target_id_string);
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_profile_picker_error(
                        "Playback will stop. Select this profile again to confirm.".into(),
                    );
                }
                return;
            }
            *pending = None;
        }
        let runtime = runtime.clone();
        let playback = playback.clone();
        let playback_selections = playback_selections.clone();
        let navigation = navigation.clone();
        let download_manager = download_manager.clone();
        let limiter = limiter.clone();
        let ui_weak = ui_weak.clone();
        let pin = pin.to_string();
        tokio::spawn(async move {
            let result = async {
                let previous_id = profiles::active_profile_id().await?;
                if previous_id == target_id {
                    return Ok::<_, anyhow::Error>((previous_id, None, false));
                }
                let profiles = profiles::list_profiles().await?;
                let target = profiles
                    .iter()
                    .find(|profile| profile.id == target_id)
                    .ok_or(profiles::ProfileError::NotFound)?;
                if target.has_pin {
                    profiles::verify_pin(&target_id, &pin, &limiter).await?;
                }
                let target_core_profile = core_env::load_profile_scope(target_id.as_str())
                    .await
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?
                    .unwrap_or_default();
                profiles::set_active_profile(&target_id).await?;
                core_env::set_active_profile_scope(target_id.as_str());
                if let Err(error) =
                    secure_settings::activate_profile(target_id.as_str(), None).await
                {
                    core_env::set_active_profile_scope(previous_id.as_str());
                    profiles::set_active_profile(&previous_id).await?;
                    let _ = secure_settings::activate_profile(previous_id.as_str(), None).await;
                    return Err(anyhow::anyhow!(error.to_string()));
                }
                if let Err(error) = download_manager.pause_profile(previous_id.as_str()).await {
                    tracing::warn!(%error, "could not pause every previous-profile download");
                }
                let token = target_core_profile.auth_key().cloned();
                Ok((previous_id, token, true))
            }
            .await;
            match result {
                Ok((_previous_id, _token, false)) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.set_show_profile_picker(false);
                            ui.set_show_profile_manager(false);
                            ui.set_show_integrations_manager(false);
                            ui.set_show_operations_manager(false);
                            ui.set_profile_manager_owner_pin("".into());
                            ui.set_profile_picker_error("".into());
                        }
                    });
                }
                Ok((_previous_id, token, true)) => {
                    let weak = ui_weak.clone();
                    let runtime_for_ui = runtime.clone();
                    let download_manager_for_ui = download_manager.clone();
                    let target_id_for_ui = target_id.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            if let Some(playback) = playback.as_ref() {
                                playback.stop_for_profile_switch();
                            }
                            playback_selections.clear();
                            metadata_enrichment::clear();
                            ui.set_stream_links(Default::default());
                            ui.set_detail_stream_providers(slint::ModelRc::new(
                                slint::VecModel::from(vec!["All".into()]),
                            ));
                            ui.set_show_profile_picker(false);
                            ui.set_profile_picker_error("".into());
                            ui.set_show_details(false);
                            ui.set_show_player(false);
                            ui.set_search_query("".into());
                            ui.set_detail_enrichment_id("".into());
                            ui.set_detail_enrichment_summary("".into());
                            ui.set_detail_enrichment_attribution_url("".into());
                            ui.set_addons_community_adult_unlocked(false);
                            ui.set_addons_community_owner_pin("".into());
                            navigation
                                .dispatch_and_project(&ui, NavigationIntent::SelectTab(Tab::Board));
                            runtime_for_ui.dispatch(stremio_core::runtime::RuntimeAction {
                                field: None,
                                action: match token {
                                    Some(token) => stremio_core::runtime::msg::Action::Ctx(
                                        stremio_core::runtime::msg::ActionCtx::PullUserFromAPI {
                                            token: Some(token),
                                        },
                                    ),
                                    None => stremio_core::runtime::msg::Action::Ctx(
                                        stremio_core::runtime::msg::ActionCtx::Logout,
                                    ),
                                },
                            });
                            callbacks::trigger_initial_load(&runtime_for_ui);
                            let download_ui_weak = ui.as_weak();
                            let integration_ui_weak = ui.as_weak();
                            tokio::spawn(async move {
                                let _ = download_manager_for_ui
                                    .resume_profile(target_id_for_ui.as_str())
                                    .await;
                                downloads::project_active_profile(download_ui_weak).await;
                                refresh_integration_projection(
                                    integration_ui_weak.clone(),
                                    &target_id_for_ui,
                                    None,
                                )
                                .await;
                                load_profile_customization_weak(
                                    integration_ui_weak,
                                    &target_id_for_ui,
                                )
                                .await;
                            });
                        }
                    });
                }
                Err(error) => {
                    let message = error.to_string();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.set_profile_picker_error(message.into());
                        }
                    });
                }
            }
        });
    });
}

fn setup_profile_management(ui: &MainWindow) {
    let limiter = Arc::new(profiles::PinAttemptLimiter::default());
    let ui_weak = ui.as_weak();
    ui.on_profile_manager_create({
        let limiter = limiter.clone();
        let ui_weak = ui_weak.clone();
        move |name, role, profile_pin, owner_pin| {
            let limiter = limiter.clone();
            let ui_weak = ui_weak.clone();
            let name = name.to_string();
            let profile_pin = profile_pin.to_string();
            let owner_pin = owner_pin.to_string();
            let role = match role.as_str() {
                "Owner" => profiles::ProfileRole::Owner,
                "Kids" => profiles::ProfileRole::Kids,
                _ => profiles::ProfileRole::Standard,
            };
            tokio::spawn(async move {
                let result: Result<(), profiles::ProfileError> = async {
                    profiles::authorize_owner_pin(&owner_pin, &limiter).await?;
                    let profile = profiles::create_profile(&name, role, None).await?;
                    if !profile_pin.is_empty()
                        && let Err(error) = profiles::set_pin(&profile.id, &profile_pin).await
                    {
                        let _ = profiles::delete_profile(&profile.id).await;
                        return Err(error);
                    }
                    Ok::<_, profiles::ProfileError>(())
                }
                .await;
                match result {
                    Ok(()) => {
                        refresh_profile_projection(ui_weak, Some("Profile created".to_owned()))
                            .await
                    }
                    Err(error) => set_profile_manager_error(ui_weak, error.to_string()),
                }
            });
        }
    });
    ui.on_profile_manager_delete({
        let limiter = limiter.clone();
        let ui_weak = ui_weak.clone();
        move |profile_id, owner_pin| {
            let limiter = limiter.clone();
            let ui_weak = ui_weak.clone();
            let profile_id = profile_id.to_string();
            let owner_pin = owner_pin.to_string();
            tokio::spawn(async move {
                let result: Result<(), profiles::ProfileError> = async {
                    profiles::authorize_owner_pin(&owner_pin, &limiter).await?;
                    let profile_id = profiles::ProfileId::parse(profile_id)?;
                    if profiles::active_profile_id().await? == profile_id {
                        return Err(profiles::ProfileError::ActiveProfile);
                    }
                    let all_profiles = profiles::list_profiles().await?;
                    let target = all_profiles
                        .iter()
                        .find(|profile| profile.id == profile_id)
                        .ok_or(profiles::ProfileError::NotFound)?;
                    if target.role == profiles::ProfileRole::Owner
                        && all_profiles
                            .iter()
                            .filter(|profile| profile.role == profiles::ProfileRole::Owner)
                            .count()
                            <= 1
                    {
                        return Err(profiles::ProfileError::LastOwner);
                    }
                    core_env::delete_profile_credential(profile_id.as_str())
                        .await
                        .map_err(|error| profiles::ProfileError::Database(error.to_string()))?;
                    secure_settings::delete_profile_credentials(profile_id.as_str())
                        .await
                        .map_err(|error| profiles::ProfileError::Database(error.to_string()))?;
                    integrations::delete_profile_credentials(profile_id.as_str())
                        .await
                        .map_err(|error| profiles::ProfileError::Database(error.to_string()))?;
                    profiles::delete_profile(&profile_id).await
                }
                .await;
                match result {
                    Ok(()) => {
                        refresh_profile_projection(ui_weak, Some("Profile deleted".to_owned()))
                            .await
                    }
                    Err(error) => set_profile_manager_error(ui_weak, error.to_string()),
                }
            });
        }
    });
}

fn setup_integration_management(ui: &MainWindow) {
    let ui_weak = ui.as_weak();
    ui.on_integration_configure({
        let ui_weak = ui_weak.clone();
        move |provider, secret, endpoint, chat_id| {
            let ui_weak = ui_weak.clone();
            let provider = provider.to_string();
            let secret = secret.to_string();
            let endpoint = endpoint.to_string();
            let chat_id = chat_id.to_string();
            tokio::spawn(async move {
                let result = async {
                    let profile = profiles::active_profile_id().await?;
                    let status = match provider.as_str() {
                        "Real-Debrid" => format_debrid_status(
                            integrations::configure_debrid(
                                profile.as_str(),
                                debrid::ProviderKind::RealDebrid,
                                &secret,
                            )
                            .await?,
                        ),
                        "AllDebrid" => format_debrid_status(
                            integrations::configure_debrid(
                                profile.as_str(),
                                debrid::ProviderKind::AllDebrid,
                                &secret,
                            )
                            .await?,
                        ),
                        "Premiumize" => format_debrid_status(
                            integrations::configure_debrid(
                                profile.as_str(),
                                debrid::ProviderKind::Premiumize,
                                &secret,
                            )
                            .await?,
                        ),
                        "Debrid-Link" => format_debrid_status(
                            integrations::configure_debrid(
                                profile.as_str(),
                                debrid::ProviderKind::DebridLink,
                                &secret,
                            )
                            .await?,
                        ),
                        "TorBox" => format_debrid_status(
                            integrations::configure_debrid(
                                profile.as_str(),
                                debrid::ProviderKind::TorBox,
                                &secret,
                            )
                            .await?,
                        ),
                        "TMDB" | "OMDb" | "MDBList" | "Fanart.tv" | "RPDB" => {
                            integrations::configure_metadata_provider(
                                profile.as_str(),
                                metadata_provider_kind(&provider)
                                    .expect("matched metadata provider"),
                                &secret,
                            )
                            .await?;
                            format!("{provider} connected and tested")
                        }
                        "Webhook" | "Telegram" => {
                            let kind = if provider == "Telegram" {
                                integrations::NotificationKind::Telegram
                            } else {
                                integrations::NotificationKind::Webhook
                            };
                            let endpoint = if provider == "Telegram" && endpoint.trim().is_empty() {
                                "https://api.telegram.org"
                            } else {
                                endpoint.as_str()
                            };
                            integrations::configure_notification(
                                profile.as_str(),
                                kind,
                                endpoint,
                                (!secret.is_empty()).then_some(secret.as_str()),
                                (!chat_id.is_empty()).then_some(chat_id.as_str()),
                            )
                            .await?;
                            integrations::send_notification(
                                profile.as_str(),
                                kind,
                                "Stremio Native notification test succeeded.",
                            )
                            .await?;
                            format!("{provider} connected and tested")
                        }
                        _ => return Err(anyhow::anyhow!("unknown integration provider")),
                    };
                    Ok::<_, anyhow::Error>((profile, status))
                }
                .await;
                match result {
                    Ok((profile, status)) => {
                        refresh_integration_projection(ui_weak, &profile, Some(status)).await;
                    }
                    Err(error) => set_integrations_status(ui_weak, error.to_string()),
                }
            });
        }
    });
    ui.on_integration_disconnect({
        let ui_weak = ui_weak.clone();
        move |provider| {
            let ui_weak = ui_weak.clone();
            let provider = provider.to_string();
            tokio::spawn(async move {
                let result = async {
                    let profile = profiles::active_profile_id().await?;
                    match provider.as_str() {
                        "Real-Debrid" => {
                            integrations::disconnect_debrid(
                                profile.as_str(),
                                debrid::ProviderKind::RealDebrid,
                            )
                            .await?
                        }
                        "AllDebrid" => {
                            integrations::disconnect_debrid(
                                profile.as_str(),
                                debrid::ProviderKind::AllDebrid,
                            )
                            .await?
                        }
                        "Premiumize" => {
                            integrations::disconnect_debrid(
                                profile.as_str(),
                                debrid::ProviderKind::Premiumize,
                            )
                            .await?
                        }
                        "Debrid-Link" => {
                            integrations::disconnect_debrid(
                                profile.as_str(),
                                debrid::ProviderKind::DebridLink,
                            )
                            .await?
                        }
                        "TorBox" => {
                            integrations::disconnect_debrid(
                                profile.as_str(),
                                debrid::ProviderKind::TorBox,
                            )
                            .await?
                        }
                        "TMDB" | "OMDb" | "MDBList" | "Fanart.tv" | "RPDB" => {
                            integrations::disconnect_metadata_provider(
                                profile.as_str(),
                                metadata_provider_kind(&provider).expect("matched provider"),
                            )
                            .await?;
                        }
                        "Webhook" => {
                            integrations::disconnect_notification(
                                profile.as_str(),
                                integrations::NotificationKind::Webhook,
                            )
                            .await?
                        }
                        "Telegram" => {
                            integrations::disconnect_notification(
                                profile.as_str(),
                                integrations::NotificationKind::Telegram,
                            )
                            .await?
                        }
                        _ => return Err(anyhow::anyhow!("unknown integration provider")),
                    }
                    Ok::<_, anyhow::Error>(profile)
                }
                .await;
                match result {
                    Ok(profile) => {
                        refresh_integration_projection(
                            ui_weak,
                            &profile,
                            Some(format!("{provider} disconnected")),
                        )
                        .await
                    }
                    Err(error) => set_integrations_status(ui_weak, error.to_string()),
                }
            });
        }
    });
    let startup_weak = ui_weak;
    tokio::spawn(async move {
        if let Ok(profile) = profiles::active_profile_id().await {
            refresh_integration_projection(startup_weak, &profile, None).await;
        }
    });
}

fn metadata_provider_kind(name: &str) -> Option<media_integrations::ProviderKind> {
    match name {
        "TMDB" => Some(media_integrations::ProviderKind::Tmdb),
        "OMDb" => Some(media_integrations::ProviderKind::Omdb),
        "MDBList" => Some(media_integrations::ProviderKind::Mdblist),
        "Fanart.tv" => Some(media_integrations::ProviderKind::Fanart),
        "RPDB" => Some(media_integrations::ProviderKind::Rpdb),
        _ => None,
    }
}

fn format_debrid_status(status: debrid::AccountStatus) -> String {
    let account = status.username.unwrap_or_else(|| "account".to_owned());
    match status.expires_at {
        Some(expires) => format!("Connected {account} · expires {}", expires),
        None if status.premium => format!("Connected {account} · premium active"),
        None => format!("Connected {account}"),
    }
}

async fn refresh_integration_projection(
    ui_weak: slint::Weak<MainWindow>,
    profile: &profiles::ProfileId,
    status: Option<String>,
) {
    let names = integrations::configured_integration_names(profile.as_str()).await;
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        match names {
            Ok(names) => {
                ui.set_configured_integrations(slint::ModelRc::new(slint::VecModel::from(
                    names.into_iter().map(Into::into).collect::<Vec<_>>(),
                )));
                ui.set_integrations_status(status.unwrap_or_default().into());
            }
            Err(error) => ui.set_integrations_status(error.to_string().into()),
        }
    });
}

fn set_integrations_status(ui_weak: slint::Weak<MainWindow>, message: String) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_integrations_status(message.into());
        }
    });
}

fn setup_operations(ui: &MainWindow, downloads: Arc<downloads::DownloadManager>) {
    let restore_confirmation = Arc::new(std::sync::Mutex::new(None::<(PathBuf, String)>));
    let ui_weak = ui.as_weak();
    ui.on_operations_create_backup({
        let ui_weak = ui_weak.clone();
        move |path, include_secrets, passphrase| {
            let ui_weak = ui_weak.clone();
            let path = PathBuf::from(path.as_str());
            let passphrase = passphrase.to_string();
            tokio::spawn(async move {
                let result: anyhow::Result<backup::BackupManifestV1> = async {
                    let secrets = if include_secrets {
                        let entries = integrations::exportable_secrets()
                            .await?
                            .into_iter()
                            .map(|(credential_ref, kind, value)| backup::SecretExportEntry {
                                credential_ref,
                                kind,
                                value,
                            })
                            .collect();
                        Some(backup::SecretExport {
                            passphrase,
                            entries,
                        })
                    } else {
                        None
                    };
                    Ok(backup::create_backup(&path, secrets).await?)
                }
                .await;
                match result {
                    Ok(manifest) => set_operations_status(
                        ui_weak,
                        format!(
                            "Backup created · {} tables · secrets: {}",
                            manifest.table_rows.len(),
                            manifest.includes_secrets
                        ),
                        false,
                    ),
                    Err(error) => set_operations_status(ui_weak, error.to_string(), false),
                }
            });
        }
    });
    ui.on_operations_preview_restore({
        let confirmation = restore_confirmation.clone();
        let ui_weak = ui_weak.clone();
        move |path, passphrase| {
            let path = PathBuf::from(path.as_str());
            let passphrase = passphrase.to_string();
            let confirmation = confirmation.clone();
            let ui_weak = ui_weak.clone();
            tokio::spawn(async move {
                let result = async {
                    let preview = backup::preview_restore(&path).await?;
                    if preview.manifest.includes_secrets {
                        backup::read_secret_export(&path, &passphrase).await?;
                    }
                    Ok::<_, backup::BackupError>(preview)
                }
                .await;
                match result {
                    Ok(preview) => {
                        *confirmation
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((
                            path,
                            preview.manifest.payload_checksum.clone(),
                        ));
                        set_operations_status(
                            ui_weak,
                            format!(
                                "Validated restore preview · {} profiles · {} downloads · {} local items",
                                preview.profile_count, preview.download_count, preview.local_media_count
                            ),
                            true,
                        );
                    }
                    Err(error) => {
                        *confirmation
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
                        set_operations_status(ui_weak, error.to_string(), false);
                    }
                }
            });
        }
    });
    ui.on_operations_apply_restore({
        let confirmation = restore_confirmation;
        let ui_weak = ui_weak.clone();
        move |path, passphrase| {
            let path = PathBuf::from(path.as_str());
            let passphrase = passphrase.to_string();
            let expected = confirmation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            let ui_weak = ui_weak.clone();
            tokio::spawn(async move {
                let result = async {
                    let Some((confirmed_path, expected_checksum)) = expected else {
                        return Err(anyhow::anyhow!("restore confirmation expired; preview the archive again"));
                    };
                    if confirmed_path != path {
                        return Err(anyhow::anyhow!("restore confirmation expired; preview the archive again"));
                    }
                    let preview = backup::preview_restore(&path).await?;
                    let secrets = if preview.manifest.includes_secrets {
                        Some(backup::read_secret_export(&path, &passphrase).await?)
                    } else {
                        None
                    };
                    let safety = paths::get()
                        .root()
                        .join("backups")
                        .join(format!("pre-restore-{}.zip", chrono::Utc::now().format("%Y%m%d-%H%M%S")));
                    backup::create_backup(&safety, None).await?;
                    let restored = backup::restore(&path, &expected_checksum).await?;
                    if let Some(secrets) = secrets {
                        integrations::restore_secrets(secrets).await?;
                    }
                    Ok::<_, anyhow::Error>(restored)
                }
                .await;
                match result {
                    Ok(preview) => {
                        refresh_profile_projection(ui_weak.clone(), None).await;
                        set_operations_status(
                            ui_weak,
                            format!("Restore applied for {} profiles. Restart the app to rebuild every runtime.", preview.profile_count),
                            false,
                        );
                    }
                    Err(error) => set_operations_status(ui_weak, error.to_string(), false),
                }
            });
        }
    });
    ui.on_operations_create_diagnostics({
        let ui_weak = ui_weak.clone();
        move |path| {
            let ui_weak = ui_weak.clone();
            let path = PathBuf::from(path.as_str());
            tokio::spawn(async move {
                match diagnostics::create_redacted_zip(&path).await {
                    Ok(path) => set_operations_status(
                        ui_weak,
                        format!("Redacted diagnostics created at {}", path.display()),
                        false,
                    ),
                    Err(error) => set_operations_status(ui_weak, error.to_string(), false),
                }
            });
        }
    });
    ui.on_operations_run_speed_test({
        let ui_weak = ui_weak.clone();
        move |endpoint| {
            let ui_weak = ui_weak.clone();
            let disclosure = network_tools::SpeedTestDisclosure::accept(endpoint.as_str());
            tokio::spawn(async move {
                let result = match disclosure {
                    Ok(disclosure) => network_tools::run_speed_test(disclosure).await,
                    Err(error) => Err(error),
                };
                match result {
                    Ok(result) => set_operations_status(
                        ui_weak,
                        format!(
                            "Speed test: {:.1} Mbps ({} bytes)",
                            result.bits_per_second / 1_000_000.0,
                            result.bytes_received
                        ),
                        false,
                    ),
                    Err(error) => set_operations_status(ui_weak, error.to_string(), false),
                }
            });
        }
    });
    ui.on_operations_save_network({
        let downloads = downloads.clone();
        let ui_weak = ui_weak.clone();
        move |region, bandwidth| {
            let Ok(mbps) = bandwidth.as_str().trim().parse::<f64>() else {
                set_operations_status(
                    ui_weak.clone(),
                    "Bandwidth must be a non-negative number".to_owned(),
                    false,
                );
                return;
            };
            if !mbps.is_finite() || mbps < 0.0 {
                set_operations_status(
                    ui_weak.clone(),
                    "Bandwidth must be a non-negative number".to_owned(),
                    false,
                );
                return;
            }
            let mut config = config::load_config();
            config.region = region.as_str().trim().to_ascii_uppercase();
            config.download_bandwidth_limit_bps = (mbps * 1_000_000.0 / 8.0) as u64;
            downloads.set_bandwidth_limit(
                (config.download_bandwidth_limit_bps > 0)
                    .then_some(config.download_bandwidth_limit_bps),
            );
            config::save_config(&config);
            set_operations_status(
                ui_weak.clone(),
                "Network and region settings saved".to_owned(),
                false,
            );
        }
    });
    ui.on_operations_apply_layout({
        let ui_weak = ui_weak.clone();
        move |preset| {
            let ui_weak = ui_weak.clone();
            let preset = preset.to_string();
            tokio::spawn(async move {
                let result = async {
                    let profile = profiles::active_profile_id().await?;
                    profiles::set_setting(&profile, "home-layout-preset", &preset).await
                }
                .await;
                match result {
                    Ok(()) => {
                        let weak = ui_weak.clone();
                        let preset_for_ui = preset.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_home_layout_preset(preset_for_ui.into());
                            }
                        });
                        set_operations_status(ui_weak, format!("{preset} layout applied"), false);
                    }
                    Err(error) => set_operations_status(ui_weak, error.to_string(), false),
                }
            });
        }
    });
    install_customization_callbacks(ui, ui_weak);
}

fn set_operations_status(ui_weak: slint::Weak<MainWindow>, message: String, restore_ready: bool) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_operations_status(message.into());
            ui.set_operations_restore_ready(restore_ready);
        }
    });
}

fn install_customization_callbacks(ui: &MainWindow, ui_weak: slint::Weak<MainWindow>) {
    ui.on_operations_import_theme({
        let ui_weak = ui_weak.clone();
        move |path| {
            let ui_weak = ui_weak.clone();
            let path = PathBuf::from(path.as_str());
            tokio::spawn(async move {
                let result = async {
                    let profile = profiles::active_profile_id().await?;
                    let managed = paths::get().root().join("themes").join(profile.as_str());
                    let manifest = customization::import_theme_manifest(&path, &managed)?;
                    profiles::set_setting(
                        &profile,
                        "theme-manifest-v1",
                        &serde_json::to_string(&manifest)?,
                    )
                    .await?;
                    Ok::<_, anyhow::Error>(manifest)
                }
                .await;
                match result {
                    Ok(manifest) => {
                        let weak = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                apply_theme_manifest(&ui, &manifest);
                            }
                        });
                        set_operations_status(
                            ui_weak,
                            "Theme imported and applied".to_owned(),
                            false,
                        );
                    }
                    Err(error) => set_operations_status(ui_weak, error.to_string(), false),
                }
            });
        }
    });
    ui.on_operations_export_theme({
        let ui_weak = ui_weak.clone();
        move |path| {
            let ui_weak = ui_weak.clone();
            let path = PathBuf::from(path.as_str());
            tokio::spawn(async move {
                let result = async {
                    let profile = profiles::active_profile_id().await?;
                    let preset = profiles::setting(&profile, "home-layout-preset")
                        .await?
                        .unwrap_or_else(|| "Side Rail".to_owned());
                    let manifest = profiles::setting(&profile, "theme-manifest-v1")
                        .await?
                        .and_then(|value| serde_json::from_str(&value).ok())
                        .unwrap_or_else(|| customization::default_theme(layout_preset(&preset)));
                    customization::export_theme_manifest(&path, &manifest)?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                match result {
                    Ok(()) => {
                        set_operations_status(ui_weak, "Theme manifest exported".to_owned(), false)
                    }
                    Err(error) => set_operations_status(ui_weak, error.to_string(), false),
                }
            });
        }
    });
    ui.on_operations_import_player_layout({
        let ui_weak = ui_weak.clone();
        move |path| {
            let ui_weak = ui_weak.clone();
            let path = PathBuf::from(path.as_str());
            tokio::spawn(async move {
                let result = async {
                    let profile = profiles::active_profile_id().await?;
                    let layout = customization::import_player_layout(&path)?;
                    profiles::set_setting(
                        &profile,
                        "player-layout-v1",
                        &serde_json::to_string(&layout)?,
                    )
                    .await?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                match result {
                    Ok(()) => {
                        set_operations_status(ui_weak, "Player layout imported".to_owned(), false)
                    }
                    Err(error) => set_operations_status(ui_weak, error.to_string(), false),
                }
            });
        }
    });
    ui.on_operations_export_player_layout({
        let ui_weak = ui_weak.clone();
        move |path| {
            let ui_weak = ui_weak.clone();
            let path = PathBuf::from(path.as_str());
            tokio::spawn(async move {
                let result = async {
                    let profile = profiles::active_profile_id().await?;
                    let layout = profiles::setting(&profile, "player-layout-v1")
                        .await?
                        .and_then(|value| serde_json::from_str(&value).ok())
                        .unwrap_or_else(customization::default_player_layout);
                    customization::export_player_layout(&path, &layout)?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                match result {
                    Ok(()) => {
                        set_operations_status(ui_weak, "Player layout exported".to_owned(), false)
                    }
                    Err(error) => set_operations_status(ui_weak, error.to_string(), false),
                }
            });
        }
    });
    ui.on_operations_reset_customization({
        let ui_weak = ui_weak.clone();
        move || {
            let ui_weak = ui_weak.clone();
            tokio::spawn(async move {
                let result = async {
                    let profile = profiles::active_profile_id().await?;
                    profiles::delete_setting(&profile, "theme-manifest-v1").await?;
                    profiles::delete_setting(&profile, "player-layout-v1").await?;
                    profiles::delete_setting(&profile, "home-layout-preset").await?;
                    Ok::<_, profiles::ProfileError>(())
                }
                .await;
                match result {
                    Ok(()) => {
                        let weak = ui_weak.clone();
                        let config = config::load_config();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                apply_theme(&ui, &config);
                                ui.set_home_layout_preset("Side Rail".into());
                            }
                        });
                        set_operations_status(
                            ui_weak,
                            "Customization reset safely".to_owned(),
                            false,
                        );
                    }
                    Err(error) => set_operations_status(ui_weak, error.to_string(), false),
                }
            });
        }
    });
}

async fn load_profile_customization(ui: &MainWindow, profile: &profiles::ProfileId) {
    let preset = profiles::setting(profile, "home-layout-preset")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "Side Rail".to_owned());
    ui.set_home_layout_preset(preset.into());
    if let Ok(Some(value)) = profiles::setting(profile, "theme-manifest-v1").await
        && let Ok(manifest) = serde_json::from_str::<customization::ThemeManifestV1>(&value)
        && manifest.validate().is_ok()
    {
        apply_theme_manifest(ui, &manifest);
    }
}

async fn load_profile_customization_weak(
    ui_weak: slint::Weak<MainWindow>,
    profile: &profiles::ProfileId,
) {
    let preset = profiles::setting(profile, "home-layout-preset")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "Side Rail".to_owned());
    let manifest = profiles::setting(profile, "theme-manifest-v1")
        .await
        .ok()
        .flatten()
        .and_then(|value| serde_json::from_str::<customization::ThemeManifestV1>(&value).ok())
        .filter(|manifest| manifest.validate().is_ok());
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            if let Some(manifest) = manifest {
                apply_theme_manifest(&ui, &manifest);
            } else {
                apply_theme(&ui, &config::load_config());
                ui.set_home_layout_preset(preset.into());
            }
        }
    });
}

fn layout_preset(value: &str) -> customization::HomeLayoutPreset {
    match value {
        "Top Bar" => customization::HomeLayoutPreset::TopBar,
        "Minimal" => customization::HomeLayoutPreset::Minimal,
        "Classic Home" => customization::HomeLayoutPreset::Classic,
        _ => customization::HomeLayoutPreset::SideRail,
    }
}

fn apply_theme_manifest(ui: &MainWindow, manifest: &customization::ThemeManifestV1) {
    if manifest.validate().is_err() {
        return;
    }
    let theme = ui.global::<Theme>();
    if let Some(color) = config::parse_color(&manifest.colors.background) {
        theme.set_background(color);
    }
    if let Some(color) = config::parse_color(&manifest.colors.surface) {
        theme.set_card_background(color);
        theme.set_modal_background(color);
    }
    if let Some(color) = config::parse_color(&manifest.colors.text) {
        theme.set_text_primary(color);
        theme.set_primary_foreground(color);
    }
    if let Some(color) = config::parse_color(&manifest.colors.accent) {
        theme.set_accent(color);
        theme.set_primary_accent(color);
    }
    if let Some(color) = config::parse_color(&manifest.colors.focus) {
        theme.set_focus(color);
    }
    theme.set_root_font_size(16.0 * manifest.density);
    ui.set_home_layout_preset(
        match manifest.layout {
            customization::HomeLayoutPreset::SideRail => "Side Rail",
            customization::HomeLayoutPreset::TopBar => "Top Bar",
            customization::HomeLayoutPreset::Minimal => "Minimal",
            customization::HomeLayoutPreset::Classic => "Classic Home",
        }
        .into(),
    );
}

async fn refresh_profile_projection(ui_weak: slint::Weak<MainWindow>, status: Option<String>) {
    let profiles = profiles::list_profiles().await;
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        match profiles {
            Ok(profiles) => {
                ui.set_local_profiles(slint::ModelRc::new(slint::VecModel::from(profile_items(
                    &profiles,
                ))));
                ui.set_profile_manager_error(status.unwrap_or_default().into());
            }
            Err(error) => ui.set_profile_manager_error(error.to_string().into()),
        }
    });
}

fn set_profile_manager_error(ui_weak: slint::Weak<MainWindow>, message: String) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_profile_manager_error(message.into());
        }
    });
}

async fn select_startup_profile(
    ui: &MainWindow,
    profiles: Vec<profiles::LocalProfile>,
    active_profile: profiles::ProfileId,
) -> anyhow::Result<profiles::ProfileId> {
    let initial_preflight = preflight_profile(&active_profile).await;
    let items = profile_items(&profiles);
    ui.set_local_profiles(slint::ModelRc::new(slint::VecModel::from(items)));
    ui.set_profile_picker_selected_id(active_profile.as_str().into());
    if profiles.len() <= 1 && initial_preflight.is_ok() {
        return Ok(active_profile);
    }

    ui.set_profile_picker_error(
        initial_preflight
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default()
            .into(),
    );
    ui.set_show_profile_picker(true);

    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();
    ui.on_profile_picker_selected(move |profile_id, pin| {
        let _ = sender.send((profile_id.to_string(), pin.to_string()));
    });
    let limiter = profiles::PinAttemptLimiter::default();
    while let Some((profile_id, pin)) = receiver.recv().await {
        let Some(profile) = profiles
            .iter()
            .find(|profile| profile.id.as_str() == profile_id)
        else {
            ui.set_profile_picker_error("That profile no longer exists".into());
            continue;
        };
        if profile.has_pin
            && let Err(error) = profiles::verify_pin(&profile.id, &pin, &limiter).await
        {
            ui.set_profile_picker_error(error.to_string().into());
            continue;
        }
        match preflight_profile(&profile.id).await {
            Ok(()) => {
                profiles::set_active_profile(&profile.id)
                    .await
                    .context("could not persist the active local profile")?;
                ui.set_profile_picker_error("".into());
                ui.set_show_profile_picker(false);
                return Ok(profile.id.clone());
            }
            Err(error) => {
                core_env::set_active_profile_scope(active_profile.as_str());
                ui.set_profile_picker_error(
                    format!("{error}. Retry or select another profile.").into(),
                );
            }
        }
    }
    Err(anyhow::anyhow!(
        "profile picker closed before a profile was selected"
    ))
}

fn profile_items(profiles: &[profiles::LocalProfile]) -> Vec<ProfileItem> {
    profiles
        .iter()
        .map(|profile| ProfileItem {
            id: profile.id.as_str().into(),
            name: profile.name.as_str().into(),
            avatar: profile.avatar.as_deref().unwrap_or_default().into(),
            role: match profile.role {
                profiles::ProfileRole::Owner => "Owner",
                profiles::ProfileRole::Standard => "Standard",
                profiles::ProfileRole::Kids => "Kids",
            }
            .into(),
            has_pin: profile.has_pin,
        })
        .collect()
}

async fn preflight_profile(profile_id: &profiles::ProfileId) -> anyhow::Result<()> {
    core_env::set_active_profile_scope(profile_id.as_str());
    DesktopEnv::get_storage::<stremio_core::types::profile::Profile>(PROFILE_STORAGE_KEY)
        .await
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!(error.to_string()))
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
        Tab::Discover | Tab::Movies | Tab::Shows | Tab::Anime | Tab::Kids => {
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
        Tab::Downloads => {}
    }
}
