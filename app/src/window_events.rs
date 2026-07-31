use crate::{AppModel, AppModelField, MainWindow};
use core_env::DesktopEnv;
use slint::winit_030::{EventResult, winit};
use std::{
    cell::Cell,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use stremio_core::runtime::{
    Runtime, RuntimeAction,
    msg::{Action, ActionCtx, ActionStreamingServer, CreateTorrentArgs},
};

const FOCUS_SYNC_DEBOUNCE: Duration = Duration::from_secs(2);

fn dispatch_focus_sync(runtime: &Runtime<DesktopEnv, AppModel>) {
    for action in [
        ActionCtx::PullAddonsFromAPI,
        ActionCtx::PullUserFromAPI { token: None },
        ActionCtx::SyncLibraryWithAPI,
        ActionCtx::PullNotifications,
    ] {
        runtime.dispatch(RuntimeAction {
            field: Some(AppModelField::Ctx),
            action: Action::Ctx(action),
        });
    }
}

fn is_torrent(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("torrent"))
}

fn import_torrent(runtime: Arc<Runtime<DesktopEnv, AppModel>>, path: PathBuf) {
    tokio::spawn(async move {
        let display_path = path.display().to_string();
        let read_result = tokio::task::spawn_blocking(move || std::fs::read(path)).await;
        match read_result {
            Ok(Ok(bytes)) => {
                runtime.dispatch(RuntimeAction {
                    field: Some(AppModelField::StreamingServer),
                    action: Action::StreamingServer(ActionStreamingServer::CreateTorrent(
                        CreateTorrentArgs::File(bytes),
                    )),
                });
                tracing::info!(path = %display_path, "torrent file imported from window drop");
            }
            Ok(Err(error)) => {
                tracing::warn!(path = %display_path, %error, "failed to read dropped torrent file");
            }
            Err(error) => {
                tracing::warn!(path = %display_path, %error, "torrent import task stopped");
            }
        }
    });
}

pub fn install(ui: &MainWindow, runtime: Arc<Runtime<DesktopEnv, AppModel>>) {
    let last_focus_sync = Cell::new(Instant::now());
    crate::window_hooks::register(ui, move |_window, event| {
        match event {
            winit::event::WindowEvent::Focused(true)
                if last_focus_sync.get().elapsed() >= FOCUS_SYNC_DEBOUNCE =>
            {
                last_focus_sync.set(Instant::now());
                dispatch_focus_sync(&runtime);
            }
            winit::event::WindowEvent::DroppedFile(path) if is_torrent(path) => {
                import_torrent(runtime.clone(), path.clone());
            }
            _ => {}
        }
        EventResult::Propagate
    });
}

#[cfg(test)]
mod tests {
    use super::is_torrent;
    use std::path::Path;

    #[test]
    fn recognizes_torrent_extension_case_insensitively() {
        assert!(is_torrent(Path::new("movie.torrent")));
        assert!(is_torrent(Path::new("show.TORRENT")));
        assert!(!is_torrent(Path::new("video.mp4")));
    }
}
