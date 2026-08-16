//! Native Windows crash handler (Structured Exception Handling).
//!
//! Intercepts unhandled native C/DLL exceptions (such as Access Violation `0xc0000005`)
//! in dynamic libraries (e.g. `libmpv-2.dll` or `orender.dll`) before process termination,
//! writing diagnostic details to `stremio.log` and `crash.log`.

#[cfg(target_os = "windows")]
use std::{fs::OpenOptions, io::Write, path::PathBuf, sync::OnceLock};

#[cfg(target_os = "windows")]
static CRASH_LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

#[cfg(target_os = "windows")]
pub fn init_crash_handler(log_dir: &std::path::Path) {
    let crash_log = log_dir.join("crash.log");
    let _ = CRASH_LOG_PATH.set(crash_log);

    unsafe {
        windows_sys::Win32::System::Diagnostics::Debug::SetUnhandledExceptionFilter(Some(
            unhandled_exception_filter,
        ));
    }
    tracing::info!("native Windows exception filter initialized");
}

#[cfg(not(target_os = "windows"))]
pub fn init_crash_handler(_log_dir: &std::path::Path) {}

#[cfg(target_os = "windows")]
unsafe extern "system" fn unhandled_exception_filter(
    exception_info: *const windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) -> i32 {
    if !exception_info.is_null() {
        let record = unsafe { (*exception_info).ExceptionRecord };
        if !record.is_null() {
            let code = unsafe { (*record).ExceptionCode };
            let address = unsafe { (*record).ExceptionAddress };

            let ucode = code as u32;
            let exception_name = match ucode {
                0xC0000005 => "STATUS_ACCESS_VIOLATION",
                0xC0000006 => "STATUS_IN_PAGE_ERROR",
                0xC000008E => "STATUS_FLOAT_DIVIDE_BY_ZERO",
                0xC0000094 => "STATUS_INTEGER_DIVIDE_BY_ZERO",
                0xC00000FD => "STATUS_STACK_OVERFLOW",
                0xC000001D => "STATUS_ILLEGAL_INSTRUCTION",
                _ => "UNKNOWN_EXCEPTION",
            };

            let (module, offset) = match faulting_module(address) {
                Some((path, offset)) => (path, format!("0x{offset:X}")),
                None => ("<unknown>".to_owned(), "<unknown>".to_owned()),
            };

            let log_entry = format!(
                "\n========================================\n\
                 FATAL NATIVE EXCEPTION AT {}\n\
                 Exception Code : 0x{:08X} ({})\n\
                 Fault Address  : {:p}\n\
                 Faulting Module: {}\n\
                 Module Offset  : {}\n\
                 Thread Id      : {}\n\
                 ========================================\n",
                chrono::Utc::now().to_rfc3339(),
                ucode,
                exception_name,
                address,
                module,
                offset,
                // SAFETY: Reading the current thread id has no preconditions.
                unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }
            );

            eprintln!("{log_entry}");

            if let Some(path) = CRASH_LOG_PATH.get()
                && let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path)
            {
                let _ = writeln!(file, "{log_entry}");
                let _ = file.flush();
            }
        }
    }
    windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_CONTINUE_SEARCH
}

/// Resolves a fault address to its owning module and relative offset, so a
/// crash report identifies the responsible binary without a memory dump.
#[cfg(target_os = "windows")]
fn faulting_module(address: *mut std::ffi::c_void) -> Option<(String, usize)> {
    use windows_sys::Win32::{
        Foundation::HMODULE,
        System::LibraryLoader::{
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            GetModuleFileNameW, GetModuleHandleExW,
        },
    };

    let mut module: HMODULE = std::ptr::null_mut();
    // SAFETY: The `FROM_ADDRESS` flag reinterprets the name argument as an
    // address to locate, and `UNCHANGED_REFCOUNT` returns a borrowed handle
    // that must not be released.
    let located = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            address.cast::<u16>(),
            &raw mut module,
        )
    };
    if located == 0 || module.is_null() {
        return None;
    }

    let mut buffer = [0_u16; 1024];
    // SAFETY: The borrowed handle is valid and the length matches the buffer.
    let length = unsafe { GetModuleFileNameW(module, buffer.as_mut_ptr(), buffer.len() as u32) };
    if length == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&buffer[..length as usize]);
    Some((path, (address as usize).saturating_sub(module as usize)))
}
