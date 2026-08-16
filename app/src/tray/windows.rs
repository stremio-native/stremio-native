//! The Windows tray backend.
//!
//! Slint's `SystemTrayIcon` renders the stock Win32 menu, which cannot be
//! themed. Instead we own the notification-area icon directly through
//! `Shell_NotifyIconW` on a message-only host window and pop up
//! [`TrayPanel`](crate::TrayPanel) — a real Slint window — where the stock menu
//! would have appeared.
//!
//! Everything here runs on the Slint event-loop thread: the host window is
//! created there, so winit's own message pump dispatches our callback messages,
//! and the panel is a Slint component that may only be touched from that thread.
//! The thread-locals below exist because a `WNDPROC` receives no user context.

use std::cell::{Cell, RefCell};
use std::mem::size_of;
use std::sync::OnceLock;

use slint::winit_030::{WinitWindowAccessor, winit};
use slint::{ComponentHandle, SharedString, Weak};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, CreateWindowExW, DefWindowProcW, DestroyWindow, GWL_EXSTYLE, GWLP_WNDPROC,
    GetCursorPos, GetSystemMetrics, GetWindowLongPtrW, HICON, HWND_MESSAGE, HWND_TOPMOST,
    IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTCOLOR, LoadIconW, LoadImageW, RegisterClassExW,
    RegisterWindowMessageW, SM_CXSCREEN, SM_CXSMICON, SM_CYSCREEN, SM_CYSMICON, SW_HIDE, SW_SHOW,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, WM_ACTIVATE, WM_APP, WM_CONTEXTMENU, WM_KILLFOCUS, WM_LBUTTONUP, WM_RBUTTONUP,
    WM_USER, WNDCLASSEXW, WNDPROC, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};
use windows::core::PCWSTR;

use super::TrayActions;
use crate::TrayPanel;

/// Sent by the shell for every interaction with our notification-area icon.
const TRAY_CALLBACK_MESSAGE: u32 = WM_APP + 1;
const TRAY_ICON_ID: u32 = 1;
/// Keyboard activation of the icon; only delivered under version-4 semantics but
/// harmless to accept.
const NIN_SELECT: u32 = WM_USER;
const NIN_KEYSELECT: u32 = WM_USER + 1;
/// The gap kept between the popup and both the cursor and the monitor edges.
const EDGE_MARGIN: i32 = 8;
/// The icon resource embedded by `build.rs` through `winres`.
const ICON_RESOURCE: &str = "MAINICON";
const WINDOW_CLASS: &str = "StremioTrayHost";

thread_local! {
    /// The popup, reachable from the `WNDPROC` that has no user context.
    static PANEL: RefCell<Option<TrayPanel>> = const { RefCell::new(None) };
    /// Slint's own window procedure, retained while we subclass the popup so it
    /// dismisses itself when it loses focus.
    static PREVIOUS_PANEL_WNDPROC: Cell<WNDPROC> = const { Cell::new(None) };
    /// Kept so the icon can be re-created when Explorer restarts.
    static TRAY_ICON: Cell<Option<HICON>> = const { Cell::new(None) };
}

pub(super) struct Tray {
    panel: TrayPanel,
    host: HWND,
}

#[derive(Clone)]
pub(super) struct TrayView(Weak<TrayPanel>);

pub(super) fn create(actions: TrayActions) -> anyhow::Result<Tray> {
    let panel = TrayPanel::new()?;
    panel.set_version(env!("CARGO_PKG_VERSION").into());
    panel.set_update_version(env!("CARGO_PKG_VERSION").into());

    let TrayActions {
        open_window,
        open_settings,
        open_logs,
        check_update,
        install_update,
        quit,
    } = actions;
    // Every activation dismisses the popup first, the way a menu would.
    panel.on_open_window(dismissing(open_window));
    panel.on_open_settings(dismissing(open_settings));
    panel.on_open_logs(dismissing(open_logs));
    panel.on_check_update(dismissing(check_update));
    panel.on_install_update(dismissing(install_update));
    panel.on_quit(dismissing(quit));
    panel.on_dismiss(hide_panel);

    // SAFETY: called on the event-loop thread; the host window outlives the icon
    // because `Tray::drop` removes the icon before destroying the window.
    let host = unsafe { create_host_window() }?;
    let icon = unsafe { load_tray_icon() };
    TRAY_ICON.with(|slot| slot.set(icon));
    if let Err(error) = unsafe { add_notification_icon(host, icon) } {
        unsafe { destroy_host_window(host) };
        return Err(error);
    }

    PANEL.with(|slot| *slot.borrow_mut() = Some(panel.clone_strong()));
    Ok(Tray { panel, host })
}

impl Tray {
    pub(super) fn view(&self) -> TrayView {
        TrayView(self.panel.as_weak())
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            remove_notification_icon(self.host);
            destroy_host_window(self.host);
        }
        TRAY_ICON.with(|slot| slot.set(None));
        PANEL.with(|slot| *slot.borrow_mut() = None);
    }
}

impl TrayView {
    pub(super) fn set_update_state(&self, state: i32) {
        if let Some(panel) = self.0.upgrade() {
            panel.set_update_state(state);
        }
    }

    pub(super) fn set_update_version(&self, version: SharedString) {
        if let Some(panel) = self.0.upgrade() {
            panel.set_update_version(version);
        }
    }

    pub(super) fn set_update_can_install(&self, can_install: bool) {
        if let Some(panel) = self.0.upgrade() {
            panel.set_update_can_install(can_install);
        }
    }

    pub(super) fn set_update_installing(&self, installing: bool) {
        if let Some(panel) = self.0.upgrade() {
            panel.set_update_installing(installing);
        }
    }
}

fn dismissing(action: Box<dyn Fn()>) -> impl FnMut() + 'static {
    move || {
        hide_panel();
        action();
    }
}

// ─── Notification-area plumbing ───

/// Explorer broadcasts this after it restarts, and every tray icon has to be
/// added again.
fn taskbar_created_message() -> u32 {
    static MESSAGE: OnceLock<u32> = OnceLock::new();
    *MESSAGE.get_or_init(|| {
        let name = wide("TaskbarCreated");
        // SAFETY: `name` is a valid NUL-terminated wide string for the call.
        unsafe { RegisterWindowMessageW(PCWSTR(name.as_ptr())) }
    })
}

unsafe fn create_host_window() -> anyhow::Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class_name = wide(WINDOW_CLASS);

        // Registering twice fails harmlessly; the tray is created once per run.
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(tray_wnd_proc),
            hInstance: instance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        RegisterClassExW(&class);

        // A message-only window receives the shell callbacks without ever being
        // visible or appearing in the task switcher.
        let host = CreateWindowExW(
            Default::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(class_name.as_ptr()),
            Default::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        )?;
        Ok(host)
    }
}

unsafe fn destroy_host_window(host: HWND) {
    if let Err(error) = unsafe { DestroyWindow(host) } {
        tracing::warn!(%error, "failed to destroy the tray host window");
    }
}

unsafe fn load_tray_icon() -> Option<HICON> {
    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        let name = wide(ICON_RESOURCE);
        // Ask for the notification-area size so Windows picks the matching frame
        // from the .ico instead of downscaling the largest one.
        let loaded = LoadImageW(
            Some(instance.into()),
            PCWSTR(name.as_ptr()),
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_DEFAULTCOLOR,
        );
        match loaded {
            Ok(handle) if !handle.is_invalid() => Some(HICON(handle.0)),
            _ => {
                tracing::warn!("the embedded application icon is unavailable for the system tray");
                LoadIconW(None, IDI_APPLICATION).ok()
            }
        }
    }
}

unsafe fn notification_icon_data(host: HWND, icon: Option<HICON>) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: host,
        uID: TRAY_ICON_ID,
        uFlags: NIF_MESSAGE | NIF_TIP | NIF_ICON,
        uCallbackMessage: TRAY_CALLBACK_MESSAGE,
        hIcon: icon.unwrap_or_default(),
        ..Default::default()
    };
    let tip = wide("Stremio");
    data.szTip[..tip.len()].copy_from_slice(&tip);
    data
}

unsafe fn add_notification_icon(host: HWND, icon: Option<HICON>) -> anyhow::Result<()> {
    let data = unsafe { notification_icon_data(host, icon) };
    // SAFETY: `data` is fully initialized and outlives the call.
    if unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "the shell rejected the notification-area icon"
        ))
    }
}

unsafe fn remove_notification_icon(host: HWND) {
    let data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: host,
        uID: TRAY_ICON_ID,
        ..Default::default()
    };
    // SAFETY: `data` is fully initialized and outlives the call.
    if !unsafe { Shell_NotifyIconW(NIM_DELETE, &data) }.as_bool() {
        tracing::warn!("failed to remove the notification-area icon");
    }
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == TRAY_CALLBACK_MESSAGE {
        let event = lparam.0 as u32;
        if matches!(
            event,
            WM_LBUTTONUP | WM_RBUTTONUP | WM_CONTEXTMENU | NIN_SELECT | NIN_KEYSELECT
        ) {
            show_panel();
            return LRESULT(0);
        }
    } else if msg == taskbar_created_message() {
        let icon = TRAY_ICON.with(|slot| slot.get());
        // SAFETY: `hwnd` is our live host window.
        if let Err(error) = unsafe { add_notification_icon(hwnd, icon) } {
            tracing::warn!(%error, "failed to restore the tray icon after an Explorer restart");
        }
        return LRESULT(0);
    }
    // SAFETY: forwarding the untouched message parameters to the default handler.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Subclasses the popup so clicking away dismisses it, the way a menu does.
unsafe extern "system" fn panel_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let dismiss = match msg {
        WM_KILLFOCUS => true,
        // WA_INACTIVE
        WM_ACTIVATE => wparam.0 == 0,
        _ => false,
    };
    if dismiss {
        // SAFETY: `hwnd` is the live popup window.
        let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
    }
    let previous = PREVIOUS_PANEL_WNDPROC.with(|slot| slot.get());
    // SAFETY: `previous` is Slint's own procedure for this window.
    unsafe { CallWindowProcW(previous, hwnd, msg, wparam, lparam) }
}

// ─── Popup placement ───

fn hide_panel() {
    with_panel_hwnd(|hwnd| {
        // SAFETY: `hwnd` belongs to the live popup window.
        let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
    });
}

fn with_panel_hwnd(action: impl FnOnce(HWND)) {
    let Some(panel) = PANEL.with(|slot| slot.borrow().as_ref().map(ComponentHandle::clone_strong))
    else {
        return;
    };
    let hwnd = panel
        .window()
        .with_winit_window(panel_hwnd)
        .flatten()
        .map(|hwnd| HWND(hwnd as *mut _));
    if let Some(hwnd) = hwnd {
        action(hwnd);
    }
}

fn panel_hwnd(window: &winit::window::Window) -> Option<usize> {
    crate::window_style::window_hwnd(window)
}

fn show_panel() {
    // Cloning out of the thread-local first keeps the borrow from spanning the
    // Slint calls below, which can re-enter this module.
    let Some(panel) = PANEL.with(|slot| slot.borrow().as_ref().map(ComponentHandle::clone_strong))
    else {
        return;
    };

    let mut cursor = POINT::default();
    // SAFETY: `cursor` is a valid out-parameter.
    if unsafe { GetCursorPos(&mut cursor) }.is_err() {
        return;
    }

    let monitor = monitor_bounds(cursor);
    let dpi = monitor_dpi(cursor);
    let mut width = scale_for_dpi(panel.get_panel_width(), dpi);
    let mut height = scale_for_dpi(panel.get_panel_height(), dpi);
    let (mut x, mut y) = place_popup(cursor, monitor, width, height);

    // Positioning before the first `show` keeps the popup from appearing at
    // winit's default location for a frame.
    panel
        .window()
        .set_position(slint::PhysicalPosition::new(x, y));
    if let Err(error) = panel.show() {
        tracing::error!(%error, "failed to show the tray panel");
        return;
    }

    with_panel_hwnd(|hwnd| {
        // SAFETY: `hwnd` is the live popup window owned by this thread.
        unsafe {
            // The window may have landed on a monitor with a different scale.
            let window_dpi = GetDpiForWindow(hwnd);
            if window_dpi != 0 && window_dpi != dpi {
                width = scale_for_dpi(panel.get_panel_width(), window_dpi);
                height = scale_for_dpi(panel.get_panel_height(), window_dpi);
                (x, y) = place_popup(cursor, monitor, width, height);
            }

            if PREVIOUS_PANEL_WNDPROC.with(|slot| slot.get()).is_none() {
                let replacement = panel_wnd_proc as unsafe extern "system" fn(_, _, _, _) -> _;
                let previous = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, replacement as usize as isize);
                if previous != 0 {
                    PREVIOUS_PANEL_WNDPROC
                        .with(|slot| slot.set(std::mem::transmute::<isize, WNDPROC>(previous)));
                }
            }

            // Keep the popup out of the taskbar and the task switcher.
            let styles = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            let styles = (styles & !WS_EX_APPWINDOW.0) | WS_EX_TOOLWINDOW.0 | WS_EX_TOPMOST.0;
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, styles as isize);
            round_window_corners(hwnd);

            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
            let _ = ShowWindow(hwnd, SW_SHOW);
            // Focus is what lets the subclass above dismiss the popup again.
            let _ = SetForegroundWindow(hwnd);
        }
    });
}

/// Anchors the popup at the cursor, above the taskbar, clamped to the monitor.
fn place_popup(
    cursor: POINT,
    monitor: (i32, i32, i32, i32),
    width: i32,
    height: i32,
) -> (i32, i32) {
    let (left, top, right, bottom) = monitor;
    let mut x = cursor.x - 12;
    let mut y = cursor.y - height - EDGE_MARGIN;

    if y < top + EDGE_MARGIN {
        y = cursor.y + EDGE_MARGIN;
    }
    if x + width > right - EDGE_MARGIN {
        x = right - width - EDGE_MARGIN;
    }
    x = x.max(left + EDGE_MARGIN);
    if y + height > bottom - EDGE_MARGIN {
        y = bottom - height - EDGE_MARGIN;
    }
    (x, y)
}

fn monitor_bounds(point: POINT) -> (i32, i32, i32, i32) {
    // SAFETY: `info` is correctly sized and the monitor handle comes from the
    // call above it.
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let area = info.rcMonitor;
            (area.left, area.top, area.right, area.bottom)
        } else {
            (
                0,
                0,
                GetSystemMetrics(SM_CXSCREEN),
                GetSystemMetrics(SM_CYSCREEN),
            )
        }
    }
}

fn monitor_dpi(point: POINT) -> u32 {
    // SAFETY: both out-parameters are valid for the duration of the call.
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        let (mut dpi_x, mut dpi_y) = (0, 0);
        match GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) {
            Ok(()) if dpi_x != 0 => dpi_x,
            _ => 96,
        }
    }
}

fn scale_for_dpi(logical: f32, dpi: u32) -> i32 {
    (logical as f64 * dpi as f64 / 96.0).round() as i32
}

/// Windows 11 rounds popup corners for us; older releases ignore the attribute.
unsafe fn round_window_corners(hwnd: HWND) {
    use windows_sys::Win32::Graphics::Dwm::{
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
    };

    let preference = DWMWCP_ROUND;
    // SAFETY: the attribute pointer matches the size passed alongside it.
    unsafe {
        DwmSetWindowAttribute(
            hwnd.0,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&raw const preference).cast(),
            size_of::<i32>() as u32,
        );
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
