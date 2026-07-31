use slint::{CloseRequestResponse, ComponentHandle, Weak, winit_030::WinitWindowAccessor};

use crate::{AppTray, MainWindow, NavigationController, NavigationIntent, Tab};

pub fn setup(
    ui: &MainWindow,
    navigation: &NavigationController,
) -> Result<AppTray, slint::PlatformError> {
    let tray = AppTray::new()?;
    tray.set_version(env!("CARGO_PKG_VERSION").into());
    tray.set_update_version(env!("CARGO_PKG_VERSION").into());

    tray.on_open_window({
        let ui = ui.as_weak();
        move || queue_show_window(ui.clone())
    });
    tray.on_open_settings({
        let ui = ui.as_weak();
        let navigation = navigation.clone();
        move || queue_open_settings(ui.clone(), navigation.clone())
    });
    let log_directory = crate::paths::get().logs().to_path_buf();
    tray.on_open_logs(move || {
        let path = log_directory.clone();
        if let Err(error) = open::that(&path) {
            tracing::error!(%error, path = %path.display(), "failed to open the log folder");
        }
    });
    tray.on_check_update({
        let ui = ui.as_weak();
        let navigation = navigation.clone();
        move || queue_update_action(ui.clone(), navigation.clone(), false)
    });
    tray.on_install_update({
        let ui = ui.as_weak();
        let navigation = navigation.clone();
        move || queue_update_action(ui.clone(), navigation.clone(), true)
    });
    tray.on_quit(|| {
        tracing::info!("quit requested from the system tray");
        if let Err(error) = slint::quit_event_loop() {
            tracing::warn!(%error, "failed to request UI event-loop shutdown");
        }
    });

    let ui_weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        if let Some(ui) = ui_weak.upgrade()
            && ui.get_player_pip_active()
        {
            tracing::info!("close requested while picture-in-picture is active; restoring window");
            ui.invoke_player_toggle_pip();
            return CloseRequestResponse::KeepWindowShown;
        }
        let quit_on_close = ui_weak
            .upgrade()
            .is_some_and(|ui| ui.get_settings_quit_on_close());
        if quit_on_close {
            tracing::info!("main window close requested application shutdown");
            if let Err(error) = slint::quit_event_loop() {
                tracing::warn!(%error, "failed to request UI event-loop shutdown");
            }
        } else {
            tracing::info!("main window closed to the system tray");
        }
        CloseRequestResponse::HideWindow
    });

    tray.show()?;
    tracing::info!("system tray initialized");
    Ok(tray)
}

fn queue_show_window(ui: Weak<MainWindow>) {
    if let Err(error) = ui.upgrade_in_event_loop(|ui| show_window(&ui)) {
        tracing::warn!(%error, "failed to queue the tray window action");
    }
}

fn queue_open_settings(ui: Weak<MainWindow>, navigation: NavigationController) {
    if let Err(error) = ui.upgrade_in_event_loop(move |ui| {
        navigation.dispatch_and_project(&ui, NavigationIntent::SelectTab(Tab::Settings));
        show_window(&ui);
    }) {
        tracing::warn!(%error, "failed to queue the tray settings action");
    }
}

fn queue_update_action(ui: Weak<MainWindow>, navigation: NavigationController, install: bool) {
    if let Err(error) = ui.upgrade_in_event_loop(move |ui| {
        navigation.dispatch_and_project(&ui, NavigationIntent::SelectTab(Tab::Settings));
        show_window(&ui);
        if install {
            ui.invoke_update_install();
        } else {
            ui.invoke_settings_update_action();
        }
    }) {
        tracing::warn!(%error, "failed to queue the tray update action");
    }
}

pub(crate) fn show_window(ui: &MainWindow) {
    if let Err(error) = ui.show() {
        tracing::error!(%error, "failed to show the main window from the tray");
        return;
    }
    ui.window().with_winit_window(|window| {
        window.set_minimized(false);
        window.focus_window();
    });
}
