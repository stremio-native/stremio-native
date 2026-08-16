//! The notification-area integration.
//!
//! Every platform exposes the same actions; only the surface that presents them
//! differs. Windows renders a custom Slint popup driven by a raw `Shell_NotifyIcon`
//! host window ([`windows`]), while the other platforms use Slint's native
//! [`SystemTrayIcon`](crate::AppTray) menu ([`standard`]).

#[cfg(not(target_os = "windows"))]
#[path = "tray/standard.rs"]
mod platform;
#[cfg(target_os = "windows")]
#[path = "tray/windows.rs"]
mod platform;

use slint::{
    CloseRequestResponse, ComponentHandle, SharedString, Weak, winit_030::WinitWindowAccessor,
};

use crate::{MainWindow, NavigationController, NavigationIntent, Tab};

/// The live tray. Dropping it removes the icon from the notification area.
pub struct Tray(platform::Tray);

/// A cheap, cloneable handle for pushing updater state into the tray surface.
/// Setters are no-ops unless called from the Slint event loop.
#[derive(Clone)]
pub struct TrayView(platform::TrayView);

/// The tray actions, resolved once so both backends bind identical behaviour.
pub(crate) struct TrayActions {
    open_window: Box<dyn Fn()>,
    open_settings: Box<dyn Fn()>,
    open_logs: Box<dyn Fn()>,
    check_update: Box<dyn Fn()>,
    install_update: Box<dyn Fn()>,
    quit: Box<dyn Fn()>,
}

pub fn setup(ui: &MainWindow, navigation: &NavigationController) -> anyhow::Result<Tray> {
    install_close_handler(ui);
    let tray = platform::create(TrayActions::new(ui, navigation))?;
    tracing::info!("system tray initialized");
    Ok(Tray(tray))
}

impl Tray {
    pub fn view(&self) -> TrayView {
        TrayView(self.0.view())
    }
}

impl TrayView {
    pub fn set_update_state(&self, state: i32) {
        self.0.set_update_state(state);
    }

    pub fn set_update_version(&self, version: SharedString) {
        self.0.set_update_version(version);
    }

    pub fn set_update_can_install(&self, can_install: bool) {
        self.0.set_update_can_install(can_install);
    }

    pub fn set_update_installing(&self, installing: bool) {
        self.0.set_update_installing(installing);
    }
}

impl TrayActions {
    fn new(ui: &MainWindow, navigation: &NavigationController) -> Self {
        let log_directory = crate::paths::get().logs().to_path_buf();
        Self {
            open_window: Box::new({
                let ui = ui.as_weak();
                move || queue_show_window(ui.clone())
            }),
            open_settings: Box::new({
                let ui = ui.as_weak();
                let navigation = navigation.clone();
                move || queue_open_settings(ui.clone(), navigation.clone())
            }),
            open_logs: Box::new(move || {
                let path = log_directory.clone();
                if let Err(error) = open::that(&path) {
                    tracing::error!(%error, path = %path.display(), "failed to open the log folder");
                }
            }),
            check_update: Box::new({
                let ui = ui.as_weak();
                let navigation = navigation.clone();
                move || queue_update_action(ui.clone(), navigation.clone(), false)
            }),
            install_update: Box::new({
                let ui = ui.as_weak();
                let navigation = navigation.clone();
                move || queue_update_action(ui.clone(), navigation.clone(), true)
            }),
            quit: Box::new(|| {
                tracing::info!("quit requested from the system tray");
                if let Err(error) = slint::quit_event_loop() {
                    tracing::warn!(%error, "failed to request UI event-loop shutdown");
                }
            }),
        }
    }
}

fn install_close_handler(ui: &MainWindow) {
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
