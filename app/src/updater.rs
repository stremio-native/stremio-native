use std::{
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
#[cfg(target_os = "windows")]
use self_update::backends::github::Update;
use self_update::{backends::github::ReleaseList, update::Release};
use slint::{ComponentHandle, Weak};
use tokio::task::JoinHandle;

use crate::{MainWindow, tray};

const REPOSITORY_OWNER: &str = "perpetus";
const REPOSITORY_NAME: &str = "stremio-native";
#[cfg(target_os = "windows")]
const WINDOWS_ASSET_IDENTIFIER: &str = "stremio-native";
#[cfg(target_os = "windows")]
const WINDOWS_INSTALLER_NAME: &str = "stremio-installer";

#[derive(Clone)]
struct AvailableUpdate {
    release: Release,
    can_install: bool,
}

#[derive(Clone, Copy)]
enum CheckKind {
    Automatic,
    Manual,
}

#[derive(Clone, Copy)]
#[repr(i32)]
enum SettingsUpdateState {
    Current,
    Checking,
    Available,
    UpToDate,
    Failed,
}

#[derive(Clone, Copy)]
#[repr(i32)]
enum SettingsUpdateAction {
    CheckNow,
    Checking,
    ViewUpdate,
    CheckAgain,
}

#[derive(Clone, Copy)]
#[repr(i32)]
enum DialogStatus {
    None,
    Downloading,
    OpenReleaseFailed,
    InstallFailed,
}

struct Updater {
    available: RwLock<Option<AvailableUpdate>>,
    busy: Arc<AtomicBool>,
    installer_request: InstallerRequest,
}

struct BusyGuard(Arc<AtomicBool>);

pub(crate) struct InstallerRequest(mpsc::SyncSender<PathBuf>);

pub(crate) struct InstallerLauncher(mpsc::Receiver<PathBuf>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub struct UpdaterHandle {
    scheduled_check: JoinHandle<()>,
}

#[derive(Clone)]
struct UpdateViews {
    ui: Weak<MainWindow>,
    tray: Option<tray::TrayView>,
}

impl InstallerRequest {
    fn queue(&self, installer: PathBuf) -> Result<()> {
        self.0
            .try_send(installer)
            .context("failed to queue the staged installer for shutdown")
    }
}

impl InstallerLauncher {
    fn take_pending(self) -> Option<PathBuf> {
        self.0.try_recv().ok()
    }

    pub(crate) fn launch_pending(self) -> Result<()> {
        let Some(installer) = self.take_pending() else {
            return Ok(());
        };

        tracing::info!(
            path = %installer.display(),
            "launching staged application installer after shutdown"
        );
        launch_installer(&installer)
    }
}

impl UpdaterHandle {
    pub fn shutdown(&self) {
        self.scheduled_check.abort();
    }
}

impl Drop for UpdaterHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(crate) fn installer_handoff() -> (InstallerRequest, InstallerLauncher) {
    let (sender, receiver) = mpsc::sync_channel(1);
    (InstallerRequest(sender), InstallerLauncher(receiver))
}

pub(crate) fn setup(
    ui: &MainWindow,
    tray: Option<&tray::Tray>,
    installer_request: InstallerRequest,
) -> UpdaterHandle {
    ui.set_settings_update_state(SettingsUpdateState::Current as i32);
    ui.set_settings_update_version(env!("CARGO_PKG_VERSION").into());
    ui.set_settings_update_action_kind(SettingsUpdateAction::CheckNow as i32);
    ui.set_settings_update_action_enabled(true);
    let tray_view = tray.map(tray::Tray::view);
    if let Some(tray) = tray_view.as_ref() {
        tray.set_update_state(SettingsUpdateState::Current as i32);
        tray.set_update_version(env!("CARGO_PKG_VERSION").into());
        tray.set_update_can_install(false);
        tray.set_update_installing(false);
    }

    let updater = Arc::new(Updater {
        available: RwLock::new(None),
        busy: Arc::new(AtomicBool::new(false)),
        installer_request,
    });
    let views = UpdateViews {
        ui: ui.as_weak(),
        tray: tray_view,
    };

    let action_updater = updater.clone();
    let action_views = views.clone();
    ui.on_settings_update_action(move || {
        let updater = action_updater.clone();
        let views = action_views.clone();
        tokio::spawn(async move {
            if let Some(update) = updater.available_update() {
                project_available_update(views, update);
            } else {
                updater.check(views, CheckKind::Manual).await;
            }
        });
    });

    let install_updater = updater.clone();
    let install_views = views.clone();
    ui.on_update_install(move || {
        let updater = install_updater.clone();
        let views = install_views.clone();
        tokio::spawn(async move {
            updater.install(views).await;
        });
    });

    let dismiss_ui = ui.as_weak();
    ui.on_update_dismiss(move || {
        if let Some(ui) = dismiss_ui.upgrade() {
            ui.set_update_dialog_open(false);
        }
    });

    let scheduled_updater = updater;
    let scheduled_views = views;
    let scheduled_check = tokio::spawn(async move {
        // Avoid adding network work to first paint and stream-server startup.
        tokio::time::sleep(Duration::from_secs(20)).await;
        scheduled_updater
            .check(scheduled_views, CheckKind::Automatic)
            .await;
    });

    UpdaterHandle { scheduled_check }
}

impl Updater {
    fn try_begin(&self) -> Option<BusyGuard> {
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| BusyGuard(self.busy.clone()))
    }

    fn available_update(&self) -> Option<AvailableUpdate> {
        self.available
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set_available(&self, update: Option<AvailableUpdate>) {
        *self
            .available
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = update;
    }

    async fn check(&self, views: UpdateViews, kind: CheckKind) {
        let Some(_busy) = self.try_begin() else {
            return;
        };

        project_settings(
            views.clone(),
            SettingsUpdateState::Checking,
            "",
            SettingsUpdateAction::Checking,
            false,
        );

        let result = tokio::task::spawn_blocking(fetch_latest_stable)
            .await
            .context("update-check task failed")
            .and_then(|result| result);

        match result {
            Ok(Some(update)) => {
                tracing::info!(
                    version = %update.release.version,
                    can_install = update.can_install,
                    "GitHub application update is available"
                );
                self.set_available(Some(update.clone()));
                project_settings(
                    views.clone(),
                    SettingsUpdateState::Available,
                    display_version(&update.release.version),
                    SettingsUpdateAction::ViewUpdate,
                    true,
                );
                project_available_update(views, update);
            }
            Ok(None) => {
                self.set_available(None);
                project_settings(
                    views,
                    SettingsUpdateState::UpToDate,
                    env!("CARGO_PKG_VERSION"),
                    SettingsUpdateAction::CheckAgain,
                    true,
                );
            }
            Err(error) => {
                tracing::warn!(%error, "GitHub application update check failed");
                let state = match kind {
                    CheckKind::Automatic => SettingsUpdateState::Current,
                    CheckKind::Manual => SettingsUpdateState::Failed,
                };
                project_settings(
                    views,
                    state,
                    env!("CARGO_PKG_VERSION"),
                    SettingsUpdateAction::CheckAgain,
                    true,
                );
            }
        }
    }

    async fn install(&self, views: UpdateViews) {
        let Some(_busy) = self.try_begin() else {
            return;
        };
        let Some(update) = self.available_update() else {
            return;
        };

        if !update.can_install {
            let version = update.release.version.clone();
            let result = tokio::task::spawn_blocking(move || {
                open::that(release_page_url(&version))
                    .context("failed to open the GitHub release page")
            })
            .await
            .context("release-page task failed")
            .and_then(|result| result);
            if let Err(error) = result {
                tracing::warn!(%error, "failed to open GitHub release page");
                project_install_error(views, DialogStatus::OpenReleaseFailed);
            } else {
                close_update_dialog(views);
            }
            return;
        }

        project_installing(views.clone(), &update.release.version);
        let version = update.release.version.clone();
        let result = tokio::task::spawn_blocking(move || stage_installer(&version))
            .await
            .context("updater task failed")
            .and_then(|result| result)
            .and_then(|installer| self.installer_request.queue(installer));

        match result {
            Ok(()) => {
                tracing::info!(
                    version = %update.release.version,
                    "application installer staged; shutting down before launch"
                );
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
            }
            Err(error) => {
                tracing::error!(%error, "application update failed");
                project_install_error(views, DialogStatus::InstallFailed);
            }
        }
    }
}

fn fetch_latest_stable() -> Result<Option<AvailableUpdate>> {
    let releases = ReleaseList::configure()
        .repo_owner(REPOSITORY_OWNER)
        .repo_name(REPOSITORY_NAME)
        .build()
        .context("invalid GitHub release configuration")?
        .fetch()
        .context("failed to fetch GitHub releases")?;

    let release = latest_stable_release(releases, env!("CARGO_PKG_VERSION"));

    Ok(release.map(|release| AvailableUpdate {
        can_install: has_native_installer(&release),
        release,
    }))
}

fn latest_stable_release(
    releases: impl IntoIterator<Item = Release>,
    current_version: &str,
) -> Option<Release> {
    releases
        .into_iter()
        .filter(|release| {
            is_stable(&release.version)
                && self_update::version::bump_is_greater(current_version, &release.version)
                    .unwrap_or(false)
        })
        .reduce(|latest, candidate| {
            if self_update::version::bump_is_greater(&latest.version, &candidate.version)
                .unwrap_or(false)
            {
                candidate
            } else {
                latest
            }
        })
}

fn is_stable(version: &str) -> bool {
    !version.trim().trim_start_matches(['v', 'V']).contains('-')
}

#[cfg(target_os = "windows")]
fn has_native_installer(release: &Release) -> bool {
    release
        .asset_for(self_update::get_target(), Some(WINDOWS_ASSET_IDENTIFIER))
        .is_some()
}

#[cfg(not(target_os = "windows"))]
fn has_native_installer(_release: &Release) -> bool {
    false
}

#[cfg(target_os = "windows")]
fn stage_installer(version: &str) -> Result<PathBuf> {
    let update_dir = crate::paths::get().updates().join(display_version(version));
    std::fs::create_dir_all(&update_dir)
        .with_context(|| format!("failed to create {}", update_dir.display()))?;
    let installer = update_dir.join("stremio-installer.exe");
    if installer.exists() {
        std::fs::remove_file(&installer)
            .with_context(|| format!("failed to remove stale {}", installer.display()))?;
    }
    let release_tag = canonical_release_tag(version);

    let status = Update::configure()
        .repo_owner(REPOSITORY_OWNER)
        .repo_name(REPOSITORY_NAME)
        .target(self_update::get_target())
        .identifier(WINDOWS_ASSET_IDENTIFIER)
        .target_version_tag(&release_tag)
        .current_version(env!("CARGO_PKG_VERSION"))
        .bin_name(WINDOWS_INSTALLER_NAME)
        .bin_path_in_archive("stremio-installer.exe")
        .bin_install_path(&installer)
        .show_download_progress(false)
        .show_output(false)
        .no_confirm(true)
        .build()
        .context("invalid GitHub updater configuration")?
        .update()
        .context("self_update failed to stage the installer")?;

    if !status.updated() && !installer.is_file() {
        bail!("self_update did not stage the installer");
    }

    Ok(installer)
}

#[cfg(target_os = "windows")]
fn launch_installer(installer: &std::path::Path) -> Result<()> {
    std::process::Command::new(installer)
        .args([
            "/SP-",
            "/SILENT",
            "/SUPPRESSMSGBOXES",
            "/CLOSEAPPLICATIONS",
            "/RESTARTAPPLICATIONS",
            "/LOG",
        ])
        .spawn()
        .with_context(|| format!("failed to launch {}", installer.display()))?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn stage_installer(_version: &str) -> Result<PathBuf> {
    bail!("automatic installation is not available on this platform")
}

#[cfg(not(target_os = "windows"))]
fn launch_installer(_installer: &std::path::Path) -> Result<()> {
    bail!("automatic installation is not available on this platform")
}

fn display_version(version: &str) -> &str {
    version.trim().trim_start_matches(['v', 'V'])
}

fn canonical_release_tag(version: &str) -> String {
    format!("v{}", display_version(version))
}

fn release_page_url(version: &str) -> String {
    let tag = canonical_release_tag(version);
    format!(
        "https://github.com/{REPOSITORY_OWNER}/{REPOSITORY_NAME}/releases/tag/{}",
        percent_encoding::utf8_percent_encode(&tag, percent_encoding::NON_ALPHANUMERIC)
    )
}

fn release_notes_text(release: &Release) -> String {
    let notes = release
        .body
        .as_deref()
        .map(str::trim)
        .filter(|notes| !notes.is_empty())
        .unwrap_or("This update includes fixes and improvements.");

    let mut chars = notes.chars();
    let shortened = chars.by_ref().take(6_000).collect::<String>();
    if chars.next().is_some() {
        format!("{shortened}\n\n…")
    } else {
        shortened
    }
}

fn release_notes(release: &Release) -> slint::StyledText {
    let notes = release_notes_text(release);

    slint::StyledText::from_markdown(&notes)
        .unwrap_or_else(|_| slint::StyledText::from_plain_text(&notes))
}

fn project_settings(
    views: UpdateViews,
    state: SettingsUpdateState,
    version: &str,
    action: SettingsUpdateAction,
    enabled: bool,
) {
    let version = version.to_owned();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = views.ui.upgrade() {
            ui.set_settings_update_state(state as i32);
            ui.set_settings_update_version(version.as_str().into());
            ui.set_settings_update_action_kind(action as i32);
            ui.set_settings_update_action_enabled(enabled);
        }
        if let Some(tray) = views.tray.as_ref() {
            tray.set_update_state(state as i32);
            tray.set_update_version(version.as_str().into());
            if !matches!(state, SettingsUpdateState::Available) {
                tray.set_update_can_install(false);
            }
        }
    });
}

fn project_available_update(views: UpdateViews, update: AvailableUpdate) {
    let version = display_version(&update.release.version).to_owned();
    let notes = release_notes(&update.release);
    let can_install = update.can_install;
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = views.ui.upgrade() {
            ui.set_update_dialog_version(version.into());
            ui.set_update_dialog_notes(notes);
            ui.set_update_dialog_can_install(can_install);
            ui.set_update_dialog_status_state(DialogStatus::None as i32);
            ui.set_update_installing(false);
            ui.set_update_dialog_open(true);
        }
        if let Some(tray) = views.tray.as_ref() {
            tray.set_update_can_install(can_install);
            tray.set_update_installing(false);
        }
    });
}

fn project_installing(views: UpdateViews, version: &str) {
    let version = display_version(version).to_owned();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = views.ui.upgrade() {
            ui.set_update_dialog_version(version.into());
            ui.set_update_installing(true);
            ui.set_update_dialog_status_state(DialogStatus::Downloading as i32);
        }
        if let Some(tray) = views.tray.as_ref() {
            tray.set_update_installing(true);
        }
    });
}

fn project_install_error(views: UpdateViews, status: DialogStatus) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = views.ui.upgrade() {
            ui.set_update_installing(false);
            ui.set_update_dialog_status_state(status as i32);
        }
        if let Some(tray) = views.tray.as_ref() {
            tray.set_update_installing(false);
        }
    });
}

fn close_update_dialog(views: UpdateViews) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = views.ui.upgrade() {
            ui.set_update_dialog_open(false);
        }
        if let Some(tray) = views.tray.as_ref() {
            tray.set_update_installing(false);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_release_filter_rejects_prereleases() {
        assert!(is_stable("v1.2.3"));
        assert!(!is_stable("v1.2.3-beta.1"));
    }

    #[test]
    fn latest_stable_release_is_selected_independently_of_api_order() {
        let releases = ["1.1.0", "1.3.0-beta.1", "1.2.0"]
            .into_iter()
            .map(|version| Release {
                version: version.to_owned(),
                ..Release::default()
            });

        let release = latest_stable_release(releases, "1.0.0").expect("new release");
        assert_eq!(release.version, "1.2.0");
    }

    #[test]
    fn release_urls_and_api_lookups_use_the_versioned_tag() {
        assert_eq!(canonical_release_tag("1.2.3"), "v1.2.3");
        assert_eq!(canonical_release_tag("v1.2.3"), "v1.2.3");
        assert!(release_page_url("1.2.3").ends_with("/v1%2E2%2E3"));
    }

    #[test]
    fn updater_targets_the_published_repository() {
        assert_eq!(REPOSITORY_OWNER, "perpetus");
        assert_eq!(REPOSITORY_NAME, "stremio-native");
    }

    #[test]
    fn release_notes_are_bounded() {
        let release = Release {
            body: Some("x".repeat(7_000)),
            ..Release::default()
        };
        assert!(release_notes_text(&release).chars().count() < 6_010);
    }

    #[test]
    fn installer_handoff_preserves_the_staged_path() {
        let (request, launcher) = installer_handoff();
        request
            .queue(PathBuf::from("stremio-installer.exe"))
            .unwrap();

        assert_eq!(
            launcher.take_pending(),
            Some(PathBuf::from("stremio-installer.exe"))
        );
    }
}
