//! Out-of-process crash capture and minidump reporting.
//!
//! The application process installs `crash-handler` and forwards the captured
//! context to a dedicated `minidumper` monitor. The monitor owns all filesystem
//! and dialog work so the in-process callback remains safe in a compromised
//! process.

use std::{
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, MutexGuard, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};
use chrono::Utc;
use minidumper::{Client, LoopAction, MinidumpBinary, Server, ServerHandler, SocketName};
use uuid::Uuid;

const REPORTER_MODE: &str = "--stremio-crash-reporter";
const METADATA_MESSAGE_KIND: u32 = 1;
const REPORTER_START_TIMEOUT: Duration = Duration::from_secs(5);
const REPORTER_STOP_TIMEOUT: Duration = Duration::from_millis(250);
const PARENT_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Eq, PartialEq)]
struct ReporterArgs {
    socket_name: String,
    log_dir: PathBuf,
    crashed_pid: u32,
}

/// Keeps the installed crash callback and its monitor process alive.
pub(crate) struct CrashReporter {
    handler: Option<::crash_handler::CrashHandler>,
    monitor: Child,
}

impl Drop for CrashReporter {
    fn drop(&mut self) {
        // Detaching drops the IPC client captured by the callback. The monitor
        // treats that clean disconnect as its normal shutdown signal.
        self.handler.take();

        let deadline = Instant::now() + REPORTER_STOP_TIMEOUT;
        while Instant::now() < deadline {
            match self.monitor.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }

        let _ = self.monitor.kill();
        let _ = self.monitor.wait();
    }
}

/// Runs the hidden monitor entry point when the current process was launched
/// as a crash reporter.
pub(crate) fn run_helper_from_args() -> Result<bool> {
    let Some(args) = reporter_args_from(std::env::args_os())? else {
        return Ok(false);
    };

    run_reporter(&args)?;
    Ok(true)
}

/// Starts the external minidump monitor and installs the process-wide crash
/// callback.
pub(crate) fn init_crash_handler(log_dir: &Path) -> Result<CrashReporter> {
    let crash_dir = log_dir.join("crashes");
    std::fs::create_dir_all(&crash_dir)
        .with_context(|| format!("failed to create {}", crash_dir.display()))?;

    let socket_name = make_socket_name()?;
    let executable = std::env::current_exe().context("failed to locate the application binary")?;
    let crashed_pid = std::process::id();
    let mut command = Command::new(executable);
    command
        .arg(REPORTER_MODE)
        .arg(&socket_name)
        .arg(log_dir)
        .arg(crashed_pid.to_string());
    configure_background_process(&mut command);

    let mut monitor = command
        .spawn()
        .context("failed to launch the crash reporter process")?;
    let client = match connect_to_monitor(&socket_name, &mut monitor) {
        Ok(client) => client,
        Err(error) => {
            stop_monitor(&mut monitor);
            return Err(error);
        }
    };

    let metadata = process_metadata(log_dir);
    if let Err(error) = client.send_message(METADATA_MESSAGE_KIND, metadata.as_bytes()) {
        stop_monitor(&mut monitor);
        return Err(anyhow!(error).context("failed to send crash reporter metadata"));
    }
    if let Err(error) = client.ping() {
        stop_monitor(&mut monitor);
        return Err(anyhow!(error).context("failed to synchronize with the crash reporter"));
    }

    // SAFETY: The callback performs only the IPC request designed by
    // `minidumper` for this compromised context. Allocation, disk I/O, logging,
    // and UI are all performed by the external monitor.
    let event = unsafe {
        ::crash_handler::make_crash_event(move |context| match client.request_dump(context) {
            Ok(()) => ::crash_handler::CrashEventResult::Handled(true),
            Err(_) => {
                ::crash_handler::write_stderr(
                    "stremio-native: crash monitor could not capture a minidump\n",
                );
                ::crash_handler::CrashEventResult::Handled(false)
            }
        })
    };

    let handler = match ::crash_handler::CrashHandler::attach(event) {
        Ok(handler) => handler,
        Err(error) => {
            stop_monitor(&mut monitor);
            return Err(anyhow!(error).context("failed to attach the native crash handler"));
        }
    };

    #[cfg(any(target_os = "linux", target_os = "android"))]
    handler.set_ptracer(Some(monitor.id()));

    tracing::info!(
        monitor_pid = monitor.id(),
        dump_directory = %crash_dir.display(),
        "out-of-process crash reporter initialized"
    );

    Ok(CrashReporter {
        handler: Some(handler),
        monitor,
    })
}

fn reporter_args_from(args: impl IntoIterator<Item = OsString>) -> Result<Option<ReporterArgs>> {
    let mut args = args.into_iter();
    let _executable = args.next();
    let Some(mode) = args.next() else {
        return Ok(None);
    };
    if mode != OsStr::new(REPORTER_MODE) {
        return Ok(None);
    }

    let socket_name = required_utf8_arg(args.next(), "crash reporter socket")?;
    let log_dir = PathBuf::from(required_arg(args.next(), "crash reporter log directory")?);
    let crashed_pid = required_utf8_arg(args.next(), "crashed process id")?
        .parse::<u32>()
        .context("crashed process id is not a valid u32")?;

    Ok(Some(ReporterArgs {
        socket_name,
        log_dir,
        crashed_pid,
    }))
}

fn required_arg(value: Option<OsString>, name: &str) -> Result<OsString> {
    value.ok_or_else(|| anyhow!("missing {name}"))
}

fn required_utf8_arg(value: Option<OsString>, name: &str) -> Result<String> {
    required_arg(value, name)?
        .into_string()
        .map_err(|_| anyhow!("{name} is not valid UTF-8"))
}

fn make_socket_name() -> Result<String> {
    let nonce = Uuid::new_v4().as_u128() as u64;
    let filename = format!("snc-{}-{nonce:016x}.sock", std::process::id(),);
    std::env::temp_dir()
        .join(filename)
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow!("temporary directory is not valid UTF-8"))
}

fn configure_background_process(command: &mut Command) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;

        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
}

fn connect_to_monitor(socket_name: &str, monitor: &mut Child) -> Result<Client> {
    let started = Instant::now();

    loop {
        let error = match Client::with_name(SocketName::path(socket_name)) {
            Ok(client) => return Ok(client),
            Err(error) => error,
        };

        if let Some(status) = monitor
            .try_wait()
            .context("failed to query the crash reporter process")?
        {
            bail!("crash reporter exited during startup with {status}");
        }
        if started.elapsed() >= REPORTER_START_TIMEOUT {
            bail!("timed out connecting to crash reporter: {error}");
        }

        thread::sleep(Duration::from_millis(20));
    }
}

fn stop_monitor(monitor: &mut Child) {
    let _ = monitor.kill();
    let _ = monitor.wait();
}

fn process_metadata(log_dir: &Path) -> String {
    let executable = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|error| format!("<unavailable: {error}>"));
    format!(
        "version={}\nbuild={}\npid={}\nos={}\narch={}\nexecutable={}\nlog={}",
        env!("CARGO_PKG_VERSION"),
        env!("STREMIO_BUILD_VERSION"),
        std::process::id(),
        std::env::consts::OS,
        std::env::consts::ARCH,
        executable,
        log_dir.join("stremio.log").display(),
    )
}

#[derive(Debug)]
struct CrashReport {
    dump_path: Option<PathBuf>,
    error: Option<String>,
}

struct ReporterHandler {
    crash_dir: PathBuf,
    crashed_pid: u32,
    metadata: Arc<Mutex<Option<String>>>,
    pending_dump_path: Mutex<Option<PathBuf>>,
    report: Arc<Mutex<Option<CrashReport>>>,
}

impl ServerHandler for ReporterHandler {
    fn create_minidump_file(&self) -> std::io::Result<(File, PathBuf)> {
        let path = make_dump_path(&self.crash_dir, self.crashed_pid);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                *lock_recover(&self.pending_dump_path) = Some(path.clone());
                Ok((file, path))
            }
            Err(error) => {
                *lock_recover(&self.report) = Some(CrashReport {
                    dump_path: None,
                    error: Some(format!("failed to create minidump file: {error}")),
                });
                Err(error)
            }
        }
    }

    fn on_minidump_created(
        &self,
        result: std::result::Result<MinidumpBinary, minidumper::Error>,
    ) -> LoopAction {
        let report = match result {
            Ok(binary) => {
                let error = binary
                    .file
                    .sync_all()
                    .err()
                    .map(|error| format!("failed to flush minidump: {error}"));
                CrashReport {
                    dump_path: Some(binary.path),
                    error,
                }
            }
            Err(error) => CrashReport {
                dump_path: lock_recover(&self.pending_dump_path).clone(),
                error: Some(format!("failed to write minidump: {error}")),
            },
        };
        *lock_recover(&self.report) = Some(report);
        LoopAction::Exit
    }

    fn on_message(&self, kind: u32, buffer: Vec<u8>) {
        if kind == METADATA_MESSAGE_KIND {
            *lock_recover(&self.metadata) = Some(String::from_utf8_lossy(&buffer).into_owned());
        }
    }

    fn on_client_disconnected(&self, num_clients: usize) -> LoopAction {
        if num_clients == 0 {
            LoopAction::Exit
        } else {
            LoopAction::Continue
        }
    }
}

fn run_reporter(args: &ReporterArgs) -> Result<()> {
    let crash_dir = args.log_dir.join("crashes");
    std::fs::create_dir_all(&crash_dir)
        .with_context(|| format!("failed to create {}", crash_dir.display()))?;

    let mut server = Server::with_name(SocketName::path(&args.socket_name))
        .context("failed to create the crash reporter IPC server")?;
    let metadata = Arc::new(Mutex::new(None));
    let report = Arc::new(Mutex::new(None));
    let handler = ReporterHandler {
        crash_dir: crash_dir.clone(),
        crashed_pid: args.crashed_pid,
        metadata: Arc::clone(&metadata),
        pending_dump_path: Mutex::new(None),
        report: Arc::clone(&report),
    };
    let shutdown = AtomicBool::new(false);
    let server_result = server.run(Box::new(handler), &shutdown, None);

    if let Some(report) = lock_recover(&report).take() {
        let metadata = lock_recover(&metadata).clone();
        finish_crash_report(
            &args.log_dir,
            args.crashed_pid,
            metadata.as_deref(),
            &report,
        );
    }

    server_result.context("crash reporter IPC loop failed")
}

fn make_dump_path(crash_dir: &Path, crashed_pid: u32) -> PathBuf {
    let timestamp = Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
    crash_dir.join(format!(
        "stremio-native-{timestamp}-{crashed_pid}-{}.dmp",
        Uuid::new_v4().simple()
    ))
}

fn finish_crash_report(
    log_dir: &Path,
    crashed_pid: u32,
    metadata: Option<&str>,
    report: &CrashReport,
) {
    if let Err(error) = append_crash_log(log_dir, metadata, report) {
        eprintln!("failed to append crash log: {error}");
    }

    wait_for_process_exit(crashed_pid);
    let crash_dir = log_dir.join("crashes");
    show_crash_dialog(log_dir, &crash_dir, metadata, report);
}

fn append_crash_log(
    log_dir: &Path,
    metadata: Option<&str>,
    report: &CrashReport,
) -> std::io::Result<()> {
    let crash_log_path = log_dir.join("crash.log");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(crash_log_path)?;

    writeln!(file, "\n========================================")?;
    writeln!(file, "CRASH CAPTURED AT {}", Utc::now().to_rfc3339())?;
    if let Some(metadata) = metadata {
        writeln!(file, "{metadata}")?;
    }
    if let Some(path) = &report.dump_path {
        writeln!(file, "minidump={}", path.display())?;
    }
    match &report.error {
        Some(error) => writeln!(file, "status=failed\nerror={error}")?,
        None => writeln!(file, "status=written")?,
    }
    writeln!(file, "========================================")?;
    file.sync_all()
}

fn crash_dialog_message(log_dir: &Path, report: &CrashReport) -> String {
    match (&report.dump_path, &report.error) {
        (Some(path), None) => format!(
            "Stremio closed unexpectedly.\n\nA diagnostic minidump was saved to:\n{}\n\nPlease include this file and stremio.log when reporting the issue.\n\nOpen the crash reports folder?",
            path.display()
        ),
        _ => format!(
            "Stremio closed unexpectedly.\n\nThe crash reporter could not finish writing a minidump. Details were saved to:\n{}\n\nOpen the crash reports folder?",
            log_dir.join("crash.log").display()
        ),
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(windows)]
fn wait_for_process_exit(pid: u32) {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };

    // SAFETY: `OpenProcess` is given a PID supplied by the parent process and a
    // non-inheritable synchronization-only handle. A successful handle is
    // closed exactly once after the bounded wait.
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if !process.is_null() {
        unsafe {
            WaitForSingleObject(process, PARENT_EXIT_TIMEOUT.as_millis() as u32);
            let _ = CloseHandle(process);
        }
    }
}

#[cfg(target_os = "linux")]
fn wait_for_process_exit(pid: u32) {
    let process_path = PathBuf::from("/proc").join(pid.to_string());
    let deadline = Instant::now() + PARENT_EXIT_TIMEOUT;
    while process_path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(target_os = "macos")]
fn wait_for_process_exit(_pid: u32) {
    thread::sleep(Duration::from_millis(350));
}

fn show_crash_dialog(
    log_dir: &Path,
    crash_dir: &Path,
    metadata: Option<&str>,
    report: &CrashReport,
) {
    // Try launching the themed Slint Crash Dialog first.
    if let Ok(dialog) = crate::CrashDialog::new() {
        use slint::ComponentHandle as _;

        dialog.set_app_version(env!("CARGO_PKG_VERSION").into());
        dialog.set_build_version(env!("STREMIO_BUILD_VERSION").into());
        let log_path_str = log_dir.join("stremio.log").display().to_string();
        dialog.set_log_path(log_path_str.clone().into());

        let dump_path_str = report
            .dump_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        dialog.set_dump_path(dump_path_str.clone().into());

        let error_detail = report
            .error
            .clone()
            .unwrap_or_else(|| "Native exception captured into minidump.".to_owned());
        dialog.set_error_message(error_detail.clone().into());

        // Callbacks
        dialog.on_open_folder({
            let dir = crash_dir.to_path_buf();
            move || {
                let _ = open::that(&dir);
            }
        });

        dialog.on_copy_report({
            let weak_dialog = dialog.as_weak();
            let metadata_str = metadata.unwrap_or_default().to_owned();
            let dump_str = dump_path_str;
            let log_str = log_path_str;
            let error_str = error_detail;
            move || {
                let report_text = format!(
                    "### Stremio Crash Report\n- **Version**: {}\n- **Build**: {}\n- **Minidump**: `{}`\n- **Log**: `{}`\n- **Capture status**: {}\n\n**Process metadata**:\n```\n{}\n```\n",
                    env!("CARGO_PKG_VERSION"),
                    env!("STREMIO_BUILD_VERSION"),
                    dump_str,
                    log_str,
                    error_str,
                    metadata_str
                );
                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                    let _ = clipboard.set_text(report_text);
                }
                if let Some(d) = weak_dialog.upgrade() {
                    d.set_copied_notification(true);
                }
            }
        });

        dialog.on_report_github({
            move || {
                let github_url = "https://github.com/stremio-native/stremio-native/issues/new?template=bug_report.md&title=%5BCrash%5D+Application+Crash+Report";
                let _ = open::that(github_url);
            }
        });

        dialog.on_restart_app(|| {
            if let Ok(exe) = std::env::current_exe() {
                let _ = Command::new(exe).spawn();
            }
            let _ = slint::quit_event_loop();
        });

        dialog.on_close_dialog(|| {
            let _ = slint::quit_event_loop();
        });

        if dialog.run().is_ok() {
            return;
        }
    }

    // Fallback: If Slint window creation fails (e.g. GPU device lost), display native OS dialog.
    let message = crash_dialog_message(log_dir, report);
    show_fallback_dialog(&message, crash_dir);
}

#[cfg(windows)]
fn show_fallback_dialog(message: &str, crash_dir: &Path) {
    use windows::{
        Win32::UI::WindowsAndMessaging::{
            IDYES, MB_DEFBUTTON1, MB_ICONERROR, MB_SETFOREGROUND, MB_YESNO, MessageBoxW,
        },
        core::PCWSTR,
    };

    let title = "Stremio closed unexpectedly";
    let title_w = title
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let message_w = message
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();

    // SAFETY: Both UTF-16 strings are null-terminated and remain alive for the
    // synchronous call. A null owner is supported for an external crash dialog.
    let result = unsafe {
        MessageBoxW(
            None,
            PCWSTR(message_w.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            MB_YESNO | MB_ICONERROR | MB_DEFBUTTON1 | MB_SETFOREGROUND,
        )
    };
    if result == IDYES {
        let _ = open::that(crash_dir);
    }
}

#[cfg(target_os = "macos")]
fn show_fallback_dialog(message: &str, crash_dir: &Path) {
    const SCRIPT: &str = r#"on run argv
set dialogResult to display alert "Stremio closed unexpectedly" message (item 1 of argv) as critical buttons {"Close", "Open Reports"} default button "Open Reports" cancel button "Close"
return button returned of dialogResult
end run"#;

    let open_reports = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(SCRIPT)
        .arg("--")
        .arg(message)
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "Open Reports");
    if open_reports {
        let _ = open::that(crash_dir);
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn show_fallback_dialog(message: &str, crash_dir: &Path) {
    let zenity = Command::new("zenity")
        .arg("--question")
        .arg("--no-markup")
        .arg("--title=Stremio closed unexpectedly")
        .arg("--icon-name=dialog-error")
        .arg("--ok-label=Open Reports")
        .arg("--cancel-label=Close")
        .arg(format!("--text={message}"))
        .status();

    let open_reports = match zenity {
        Ok(status) => status.success(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Command::new("kdialog")
            .arg("--yesno")
            .arg(message)
            .arg("--title")
            .arg("Stremio closed unexpectedly")
            .status()
            .is_ok_and(|status| status.success()),
        Err(error) => {
            eprintln!("failed to display crash dialog: {error}");
            false
        }
    };

    if open_reports {
        let _ = open::that(crash_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CrashReport, REPORTER_MODE, ReporterArgs, crash_dialog_message, reporter_args_from,
    };
    use std::{ffi::OsString, path::PathBuf};

    #[test]
    fn reporter_args_should_ignore_regular_application_launch() {
        let args = [
            OsString::from("stremio-native"),
            OsString::from("stremio://movie"),
        ];

        let result = reporter_args_from(args).expect("regular arguments should parse");

        assert_eq!(result, None);
    }

    #[test]
    fn reporter_args_should_parse_hidden_monitor_launch() {
        let args = [
            OsString::from("stremio-native"),
            OsString::from(REPORTER_MODE),
            OsString::from("monitor.sock"),
            OsString::from("logs"),
            OsString::from("42"),
        ];
        let expected = ReporterArgs {
            socket_name: "monitor.sock".to_owned(),
            log_dir: PathBuf::from("logs"),
            crashed_pid: 42,
        };

        let result = reporter_args_from(args).expect("reporter arguments should parse");

        assert_eq!(result, Some(expected));
    }

    #[test]
    fn reporter_args_should_reject_missing_process_id() {
        let args = [
            OsString::from("stremio-native"),
            OsString::from(REPORTER_MODE),
            OsString::from("monitor.sock"),
            OsString::from("logs"),
        ];

        let error = reporter_args_from(args).expect_err("missing pid should fail");

        assert_eq!(error.to_string(), "missing crashed process id");
    }

    #[test]
    fn crash_dialog_should_name_successful_minidump() {
        let report = CrashReport {
            dump_path: Some(PathBuf::from("logs/crashes/report.dmp")),
            error: None,
        };

        let message = crash_dialog_message(PathBuf::from("logs").as_path(), &report);

        assert!(
            message.contains("report.dmp"),
            "unexpected dialog: {message}"
        );
    }

    #[test]
    fn crash_dialog_should_name_fallback_log_when_dump_failed() {
        let report = CrashReport {
            dump_path: None,
            error: Some("writer failed".to_owned()),
        };

        let message = crash_dialog_message(PathBuf::from("logs").as_path(), &report);

        assert!(
            message.contains("crash.log"),
            "unexpected dialog: {message}"
        );
    }
}
