use std::sync::Arc;

use core_env::DesktopEnv;
use slint::ComponentHandle;
use stremio_core::runtime::{
    Runtime, RuntimeAction,
    msg::{Action, ActionCtx},
};

use crate::{AppModel, AppModelField, MainWindow};

fn dismiss(runtime: &Runtime<DesktopEnv, AppModel>, event_id: &str) {
    if event_id.is_empty() {
        return;
    }
    runtime.dispatch(RuntimeAction {
        field: Some(AppModelField::Ctx),
        action: Action::Ctx(ActionCtx::DismissEvent(event_id.to_owned())),
    });
}

pub fn setup(ui: &MainWindow, runtime: &Arc<Runtime<DesktopEnv, AppModel>>) {
    ui.on_event_dismiss({
        let runtime = runtime.clone();
        move |event_id| dismiss(&runtime, event_id.as_str())
    });

    ui.on_event_action({
        let runtime = runtime.clone();
        let ui_weak = ui.as_weak();
        move |event_id, url| {
            dismiss(&runtime, event_id.as_str());
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            if url == ui.get_event_modal_addon_manifest_url() {
                ui.invoke_tab_changed(3);
                ui.invoke_open_addon_details(url);
            } else if !url.is_empty()
                && let Err(error) = open::that(url.as_str())
            {
                tracing::warn!(%error, %url, "failed to open event URL");
            }
        }
    });

    runtime.dispatch(RuntimeAction {
        field: Some(AppModelField::Ctx),
        action: Action::Ctx(ActionCtx::GetEvents),
    });
}
