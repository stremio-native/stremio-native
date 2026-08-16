use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::Context;
use tracing::warn;

#[derive(Clone, Debug, Eq, PartialEq)]
struct LockingProcess {
    pid: u32,
    name: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UserPromptChoice {
    TerminateAndRetry,
    Retry,
    Exit,
}

/// Detects SQLite/Turso lock errors without treating unrelated "busy" errors
/// as permission to terminate another process.
fn is_db_lock_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        if matches!(
            cause.downcast_ref::<turso::Error>(),
            Some(turso::Error::Busy(_) | turso::Error::BusySnapshot(_))
        ) {
            return true;
        }

        let message = cause.to_string().to_ascii_lowercase();
        message.contains("sqlite_busy")
            || message.contains("database is locked")
            || message.contains("database table is locked")
            || message.contains("database is busy")
            || message.contains("databaselocked")
    })
}

fn database_resource_paths(database_path: &Path) -> Vec<PathBuf> {
    let mut paths = vec![database_path.to_owned()];
    for suffix in ["-wal", "-shm"] {
        let companion = database_companion_path(database_path, suffix);
        if companion.exists() {
            paths.push(companion);
        }
    }
    paths
}

fn database_companion_path(database_path: &Path, suffix: &str) -> PathBuf {
    let mut path = database_path.as_os_str().to_os_string();
    path.push(suffix);
    path.into()
}

#[cfg(windows)]
struct RestartManagerSession(u32);

#[cfg(windows)]
impl Drop for RestartManagerSession {
    fn drop(&mut self) {
        use windows::Win32::System::RestartManager::RmEndSession;

        // SAFETY: The handle was returned by RmStartSession and is ended once
        // by this guard.
        let _ = unsafe { RmEndSession(self.0) };
    }
}

/// Uses Windows Restart Manager to identify processes with handles to the
/// database or its live WAL/SHM files.
#[cfg(windows)]
fn find_database_lock_owners(database_path: &Path) -> Vec<LockingProcess> {
    use std::os::windows::ffi::OsStrExt;

    use windows::{
        Win32::{
            Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS},
            System::RestartManager::{
                CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmGetList, RmRegisterResources, RmStartSession,
            },
        },
        core::{PCWSTR, PWSTR},
    };

    let mut handle = 0;
    let mut session_key = [0_u16; CCH_RM_SESSION_KEY as usize + 1];
    // SAFETY: Both output buffers are writable for the documented lengths.
    if unsafe { RmStartSession(&mut handle, None, PWSTR(session_key.as_mut_ptr())) }
        != ERROR_SUCCESS
    {
        return Vec::new();
    }
    let session = RestartManagerSession(handle);

    let encoded_paths = database_resource_paths(database_path)
        .into_iter()
        .map(|path| {
            path.as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let resources = encoded_paths
        .iter()
        .map(|path| PCWSTR(path.as_ptr()))
        .collect::<Vec<_>>();
    // SAFETY: Every PCWSTR points into `encoded_paths`, which remains alive
    // for the synchronous registration call.
    if unsafe { RmRegisterResources(session.0, Some(&resources), None, None) } != ERROR_SUCCESS {
        return Vec::new();
    }

    let mut needed = 0;
    let mut count = 0;
    let mut reboot_reasons = 0;
    // SAFETY: The size-discovery call receives valid output pointers and no
    // process-info buffer.
    let status = unsafe {
        RmGetList(
            session.0,
            &mut needed,
            &mut count,
            None,
            &mut reboot_reasons,
        )
    };
    if status == ERROR_SUCCESS {
        return Vec::new();
    }
    if status != ERROR_MORE_DATA {
        return Vec::new();
    }

    let mut process_info = Vec::new();
    for _ in 0..3 {
        process_info.resize(needed as usize, RM_PROCESS_INFO::default());
        count = needed;
        // SAFETY: `process_info` has capacity for `count` entries and all
        // scalar output pointers are valid for the synchronous call.
        let status = unsafe {
            RmGetList(
                session.0,
                &mut needed,
                &mut count,
                Some(process_info.as_mut_ptr()),
                &mut reboot_reasons,
            )
        };
        if status == ERROR_SUCCESS {
            process_info.truncate(count as usize);
            break;
        }
        if status != ERROR_MORE_DATA {
            return Vec::new();
        }
    }

    let current_pid = std::process::id();
    let mut seen = HashSet::new();
    let mut processes = process_info
        .into_iter()
        .filter_map(|info| {
            let pid = info.Process.dwProcessId;
            if pid <= 4 || pid == current_pid || !seen.insert(pid) {
                return None;
            }
            let name_end = info
                .strAppName
                .iter()
                .position(|character| *character == 0)
                .unwrap_or(info.strAppName.len());
            let name = String::from_utf16_lossy(&info.strAppName[..name_end]);
            Some(LockingProcess {
                pid,
                name: (!name.is_empty()).then_some(name),
            })
        })
        .collect::<Vec<_>>();
    processes.sort_unstable_by_key(|process| process.pid);
    processes
}

/// Uses lsof on Unix so only processes with the database files open are
/// considered. An unavailable lsof command safely produces no candidates.
#[cfg(not(windows))]
fn find_database_lock_owners(database_path: &Path) -> Vec<LockingProcess> {
    let output = Command::new("lsof")
        .arg("-t")
        .arg("--")
        .args(database_resource_paths(database_path))
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };

    let current_pid = std::process::id();
    let mut seen = HashSet::new();
    let mut processes = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|value| value.parse::<u32>().ok())
        .filter(|pid| *pid > 1 && *pid != current_pid && seen.insert(*pid))
        .map(|pid| LockingProcess { pid, name: None })
        .collect::<Vec<_>>();
    processes.sort_unstable_by_key(|process| process.pid);
    processes
}

fn force_terminate_process(pid: u32) -> bool {
    #[cfg(windows)]
    let status = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status();

    #[cfg(not(windows))]
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();

    matches!(status, Ok(status) if status.success())
}

/// Rechecks file ownership after the user responds, avoiding termination when
/// a process has already released the database during the dialog.
fn terminate_confirmed_lock_owners(database_path: &Path, confirmed: &[LockingProcess]) -> Vec<u32> {
    let confirmed = confirmed
        .iter()
        .map(|process| process.pid)
        .collect::<HashSet<_>>();
    find_database_lock_owners(database_path)
        .into_iter()
        .filter(|process| confirmed.contains(&process.pid))
        .filter_map(|process| force_terminate_process(process.pid).then_some(process.pid))
        .collect()
}

fn prompt_user_db_locked(processes: &[LockingProcess]) -> UserPromptChoice {
    let title = "Stremio - Database Locked";
    let (message, affirmative_choice) = if processes.is_empty() {
        (
            "The local database is locked, but its owner could not be identified.\n\n\
             Select Yes to retry database initialization or No to exit."
                .to_owned(),
            UserPromptChoice::Retry,
        )
    } else {
        let owners = processes
            .iter()
            .map(|process| match process.name.as_deref() {
                Some(name) => format!("{name} (PID {})", process.pid),
                None => format!("PID {}", process.pid),
            })
            .collect::<Vec<_>>()
            .join(", ");
        (
            format!(
                "The following process is holding the local database:\n\n{owners}\n\n\
                 Select Yes to terminate confirmed lock owners and continue, or No to exit."
            ),
            UserPromptChoice::TerminateAndRetry,
        )
    };

    #[cfg(windows)]
    {
        use windows::{
            Win32::UI::WindowsAndMessaging::{
                IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_YESNO, MessageBoxW,
            },
            core::PCWSTR,
        };

        let title_w = title
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let message_w = message
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();

        // SAFETY: Both strings are null-terminated and remain alive for the
        // synchronous dialog call. A null owner is supported by MessageBoxW.
        let result = unsafe {
            MessageBoxW(
                None,
                PCWSTR(message_w.as_ptr()),
                PCWSTR(title_w.as_ptr()),
                MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
            )
        };
        if result == IDYES {
            affirmative_choice
        } else {
            UserPromptChoice::Exit
        }
    }

    #[cfg(not(windows))]
    {
        warn!(%title, %message, "database startup is waiting for user input");
        eprintln!("\n[WARNING] {title}\n{message}\nContinue? [y/N]: ");
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_ok() && input.trim().eq_ignore_ascii_case("y")
        {
            affirmative_choice
        } else {
            UserPromptChoice::Exit
        }
    }
}

/// Initializes the database with bounded lock waits and performs blocking OS
/// discovery, prompting, and process termination outside Tokio worker threads.
pub async fn init_db_with_lock_handling(database_path: &Path) -> anyhow::Result<()> {
    const MAX_RETRIES: usize = 3;
    const RELEASE_SETTLE_DELAY: Duration = Duration::from_millis(250);

    let mut retries = 0;
    loop {
        match crate::db::init_db(database_path).await {
            Ok(()) => return Ok(()),
            Err(error) if is_db_lock_error(&error) && retries < MAX_RETRIES => {
                retries += 1;
                warn!(retries, %error, "database is locked; identifying the file owner");

                let owner_path = database_path.to_owned();
                let processes =
                    tokio::task::spawn_blocking(move || find_database_lock_owners(&owner_path))
                        .await
                        .context("database lock-owner discovery task failed")?;
                let prompt_processes = processes.clone();
                let choice =
                    tokio::task::spawn_blocking(move || prompt_user_db_locked(&prompt_processes))
                        .await
                        .context("database lock prompt task failed")?;

                match choice {
                    UserPromptChoice::TerminateAndRetry => {
                        let terminate_path = database_path.to_owned();
                        let terminated = tokio::task::spawn_blocking(move || {
                            terminate_confirmed_lock_owners(&terminate_path, &processes)
                        })
                        .await
                        .context("database lock-owner termination task failed")?;
                        if terminated.is_empty() {
                            warn!(
                                "no confirmed database lock owner remained to terminate; retrying"
                            );
                        } else {
                            warn!(?terminated, "terminated confirmed database lock owners");
                        }
                    }
                    UserPromptChoice::Retry => {}
                    UserPromptChoice::Exit => {
                        return Err(anyhow::anyhow!(
                            "startup aborted by user because the database is locked"
                        ));
                    }
                }

                tokio::time::sleep(RELEASE_SETTLE_DELAY).await;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{database_companion_path, is_db_lock_error};
    use std::path::{Path, PathBuf};

    #[test]
    fn turso_busy_error_should_be_classified_as_database_lock() {
        let error = anyhow::Error::new(turso::Error::Busy("database is locked".to_owned()));

        assert!(
            is_db_lock_error(&error),
            "error was not classified: {error}"
        );
    }

    #[test]
    fn unrelated_busy_error_should_not_be_classified_as_database_lock() {
        let error = anyhow::anyhow!("media service is busy");

        assert!(
            !is_db_lock_error(&error),
            "error was misclassified: {error}"
        );
    }

    #[test]
    fn companion_path_should_append_suffix_to_database_filename() {
        let path = database_companion_path(Path::new("app.db"), "-wal");

        assert_eq!(path, PathBuf::from("app.db-wal"));
    }
}
