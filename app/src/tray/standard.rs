//! The non-Windows tray backend: Slint's native `SystemTrayIcon` menu.

use slint::{ComponentHandle, SharedString, Weak};

use super::TrayActions;
use crate::AppTray;

pub(super) struct Tray(AppTray);

#[derive(Clone)]
pub(super) struct TrayView(Weak<AppTray>);

pub(super) fn create(actions: TrayActions) -> anyhow::Result<Tray> {
    let tray = AppTray::new()?;
    tray.set_version(env!("CARGO_PKG_VERSION").into());
    tray.set_update_version(env!("CARGO_PKG_VERSION").into());

    let TrayActions {
        open_window,
        open_settings,
        open_logs,
        check_update,
        install_update,
        quit,
    } = actions;
    tray.on_open_window(move || open_window());
    tray.on_open_settings(move || open_settings());
    tray.on_open_logs(move || open_logs());
    tray.on_check_update(move || check_update());
    tray.on_install_update(move || install_update());
    tray.on_quit(move || quit());

    tray.show()?;
    Ok(Tray(tray))
}

impl Tray {
    pub(super) fn view(&self) -> TrayView {
        TrayView(self.0.as_weak())
    }
}

impl TrayView {
    pub(super) fn set_update_state(&self, state: i32) {
        if let Some(tray) = self.0.upgrade() {
            tray.set_update_state(state);
        }
    }

    pub(super) fn set_update_version(&self, version: SharedString) {
        if let Some(tray) = self.0.upgrade() {
            tray.set_update_version(version);
        }
    }

    pub(super) fn set_update_can_install(&self, can_install: bool) {
        if let Some(tray) = self.0.upgrade() {
            tray.set_update_can_install(can_install);
        }
    }

    pub(super) fn set_update_installing(&self, installing: bool) {
        if let Some(tray) = self.0.upgrade() {
            tray.set_update_installing(installing);
        }
    }
}
