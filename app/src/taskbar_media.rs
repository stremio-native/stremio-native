//! Windows taskbar thumbnail toolbar: a single play/pause button shown when the
//! user hovers the app's taskbar icon (`ITaskbarList3::ThumbBarAddButtons`).
//!
//! This complements the media flyout / hardware keys that `souvlaki` drives (see
//! [`crate::media_session`]); the taskbar thumbnail button is a distinct Windows
//! API that souvlaki does not cover. It is a no-op on other platforms — Linux
//! surfaces play/pause through MPRIS, which the media session already provides.

/// What the taskbar button should show.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonState {
    /// No playback — hide the button.
    Hidden,
    /// Playing — show a pause glyph.
    Playing,
    /// Paused — show a play glyph.
    Paused,
}

#[cfg(target_os = "windows")]
pub use windows_impl::{init, set_progress, set_state};

/// Install the taskbar button on the given window (HWND as `usize`). Control
/// clicks toggle `ui`'s player. Safe to call once; later calls are ignored.
#[cfg(not(target_os = "windows"))]
pub fn init(_hwnd: usize, _ui: slint::Weak<crate::MainWindow>) {}

/// Reflect the current playback state onto the button. Callable from any thread.
#[cfg(not(target_os = "windows"))]
pub fn set_state(_state: ButtonState) {}

/// Update the taskbar progress bar. Callable from any thread.
#[cfg(not(target_os = "windows"))]
pub fn set_progress(_position_secs: i64, _duration_secs: i64) {}

#[cfg(target_os = "windows")]
mod windows_impl {
    use std::{
        ffi::c_void,
        sync::atomic::{AtomicU32, AtomicUsize, Ordering},
    };

    use slint::Weak;
    use windows::{
        Win32::{
            Foundation::{HWND, LPARAM, LRESULT, WPARAM},
            Graphics::Gdi::{
                BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection,
                DIB_RGB_COLORS, DeleteObject, HGDIOBJ,
            },
            System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
            UI::{
                Shell::{
                    DefSubclassProc, ITaskbarList3, RemoveWindowSubclass, SetWindowSubclass,
                    TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_PAUSED, THB_FLAGS, THB_ICON, THB_TOOLTIP,
                    THBF_ENABLED, THBF_HIDDEN, THBN_CLICKED, THUMBBUTTON, TaskbarList,
                },
                WindowsAndMessaging::{
                    CreateIconIndirect, DestroyIcon, HICON, ICONINFO, PostMessageW,
                    RegisterWindowMessageW, WM_COMMAND, WM_NCDESTROY,
                },
            },
        },
        core::w,
    };

    use super::ButtonState;
    use crate::MainWindow;

    /// Identifier for our single thumbnail button.
    const BUTTON_ID: u32 = 0x5001;
    /// Subclass id, unique within this window.
    const SUBCLASS_ID: usize = 0x5052_4E42;
    /// Icon canvas size in pixels.
    const ICON_SIZE: i32 = 32;

    /// The window we installed on, so [`set_state`] can post to it from any
    /// thread. Zero until [`init`] succeeds.
    static HWND_BITS: AtomicUsize = AtomicUsize::new(0);
    /// A process-unique registered message that carries a new [`ButtonState`] to
    /// the UI thread. Registered (rather than `WM_APP + n`) so it cannot clash
    /// with the messages winit and Slint use on this same window. Zero until
    /// [`init`] registers it.
    static UPDATE_MSG: AtomicU32 = AtomicU32::new(0);
    /// Companion message carrying playback position (wParam) and duration
    /// (lParam) for the taskbar progress bar. Zero until [`init`] registers it.
    static PROGRESS_MSG: AtomicU32 = AtomicU32::new(0);

    /// Per-window state owned by the subclass, reached through its ref-data
    /// pointer. Only ever touched on the window's (UI) thread.
    struct ThumbState {
        ui: Weak<MainWindow>,
        taskbar: Option<ITaskbarList3>,
        play_icon: HICON,
        pause_icon: HICON,
        /// The `TaskbarButtonCreated` broadcast, the signal that the button may
        /// be added.
        button_created_msg: u32,
        added: bool,
        state: ButtonState,
    }

    pub fn init(hwnd_bits: usize, ui: Weak<MainWindow>) {
        if hwnd_bits == 0 || HWND_BITS.swap(hwnd_bits, Ordering::Relaxed) != 0 {
            return;
        }
        let hwnd = HWND(hwnd_bits as *mut c_void);

        let (Some(play_icon), Some(pause_icon)) = (unsafe { make_icon(draw_play) }, unsafe {
            make_icon(draw_pause)
        }) else {
            tracing::warn!("could not create taskbar button icons");
            HWND_BITS.store(0, Ordering::Relaxed);
            return;
        };

        let button_created_msg = unsafe { RegisterWindowMessageW(w!("TaskbarButtonCreated")) };
        UPDATE_MSG.store(
            unsafe { RegisterWindowMessageW(w!("StremioNativeTaskbarButtonUpdate")) },
            Ordering::Relaxed,
        );
        PROGRESS_MSG.store(
            unsafe { RegisterWindowMessageW(w!("StremioNativeTaskbarProgress")) },
            Ordering::Relaxed,
        );

        let state = Box::new(ThumbState {
            ui,
            taskbar: None,
            play_icon,
            pause_icon,
            button_created_msg,
            added: false,
            state: ButtonState::Hidden,
        });
        let refdata = Box::into_raw(state) as usize;

        let installed =
            unsafe { SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, refdata) };
        if !installed.as_bool() {
            tracing::warn!("could not subclass the window for the taskbar button");
            let state = unsafe { Box::from_raw(refdata as *mut ThumbState) };
            unsafe { destroy_icons(&state) };
            HWND_BITS.store(0, Ordering::Relaxed);
            return;
        }

        // The taskbar button usually already exists by the first window event;
        // try now, and otherwise the TaskbarButtonCreated message retries.
        unsafe { ensure_added(hwnd, &mut *(refdata as *mut ThumbState)) };
    }

    pub fn set_state(state: ButtonState) {
        let bits = HWND_BITS.load(Ordering::Relaxed);
        let update_msg = UPDATE_MSG.load(Ordering::Relaxed);
        if bits == 0 || update_msg == 0 {
            return;
        }
        let encoded = match state {
            ButtonState::Hidden => 0,
            ButtonState::Playing => 1,
            ButtonState::Paused => 2,
        };
        // PostMessageW is thread-safe; the actual update runs on the UI thread.
        let _ = unsafe {
            PostMessageW(
                Some(HWND(bits as *mut c_void)),
                update_msg,
                WPARAM(encoded),
                LPARAM(0),
            )
        };
    }

    pub fn set_progress(position_secs: i64, duration_secs: i64) {
        let bits = HWND_BITS.load(Ordering::Relaxed);
        let progress_msg = PROGRESS_MSG.load(Ordering::Relaxed);
        if bits == 0 || progress_msg == 0 {
            return;
        }
        let _ = unsafe {
            PostMessageW(
                Some(HWND(bits as *mut c_void)),
                progress_msg,
                WPARAM(position_secs.max(0) as usize),
                LPARAM(duration_secs.max(0) as isize),
            )
        };
    }

    unsafe extern "system" fn subclass_proc(
        hwnd: HWND,
        umsg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _subclass_id: usize,
        refdata: usize,
    ) -> LRESULT {
        let state = unsafe { &mut *(refdata as *mut ThumbState) };

        if umsg == state.button_created_msg {
            unsafe { ensure_added(hwnd, state) };
        } else if umsg == WM_COMMAND
            && hiword(wparam.0) == THBN_CLICKED
            && loword(wparam.0) == BUTTON_ID
        {
            if let Some(ui) = state.ui.upgrade()
                && ui.get_show_player()
            {
                ui.invoke_player_toggle_pause();
                ui.invoke_player_activity();
            }
            return LRESULT(0);
        } else if umsg == UPDATE_MSG.load(Ordering::Relaxed) {
            state.state = match wparam.0 {
                1 => ButtonState::Playing,
                2 => ButtonState::Paused,
                _ => ButtonState::Hidden,
            };
            unsafe {
                update_button(hwnd, state);
                apply_progress_state(hwnd, state);
            }
            return LRESULT(0);
        } else if umsg == PROGRESS_MSG.load(Ordering::Relaxed) {
            let position = wparam.0 as u64;
            let duration = lparam.0 as u64;
            unsafe { set_progress_value(hwnd, state, position, duration) };
            return LRESULT(0);
        } else if umsg == WM_NCDESTROY {
            unsafe {
                let _ = RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID);
                destroy_icons(state);
                drop(Box::from_raw(refdata as *mut ThumbState));
            }
            HWND_BITS.store(0, Ordering::Relaxed);
            return unsafe { DefSubclassProc(hwnd, umsg, wparam, lparam) };
        }

        unsafe { DefSubclassProc(hwnd, umsg, wparam, lparam) }
    }

    /// Create and initialize the taskbar list once. Returns whether it is
    /// available. The progress bar needs only this; the thumbnail button
    /// additionally needs [`ensure_added`].
    unsafe fn ensure_taskbar(state: &mut ThumbState) -> bool {
        if state.taskbar.is_some() {
            return true;
        }
        match unsafe {
            CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER)
        } {
            Ok(taskbar) if unsafe { taskbar.HrInit() }.is_ok() => {
                state.taskbar = Some(taskbar);
                true
            }
            Ok(_) => false,
            Err(error) => {
                tracing::warn!(%error, "taskbar controls unavailable");
                false
            }
        }
    }

    /// Add the thumbnail button. Retried until the window's taskbar button
    /// actually exists.
    unsafe fn ensure_added(hwnd: HWND, state: &mut ThumbState) {
        if state.added || !unsafe { ensure_taskbar(state) } {
            return;
        }
        let Some(taskbar) = state.taskbar.as_ref() else {
            return;
        };
        let button = make_button(state);
        if unsafe { taskbar.ThumbBarAddButtons(hwnd, &[button]) }.is_ok() {
            state.added = true;
        }
    }

    /// Map the button state onto the taskbar progress bar's mode: a normal
    /// (green) bar while playing, a paused (yellow) bar while paused, and no bar
    /// when idle.
    unsafe fn apply_progress_state(hwnd: HWND, state: &mut ThumbState) {
        if !unsafe { ensure_taskbar(state) } {
            return;
        }
        let Some(taskbar) = state.taskbar.as_ref() else {
            return;
        };
        let flag = match state.state {
            ButtonState::Playing => TBPF_NORMAL,
            ButtonState::Paused => TBPF_PAUSED,
            ButtonState::Hidden => TBPF_NOPROGRESS,
        };
        let _ = unsafe { taskbar.SetProgressState(hwnd, flag) };
    }

    unsafe fn set_progress_value(hwnd: HWND, state: &mut ThumbState, position: u64, duration: u64) {
        if duration == 0 || !unsafe { ensure_taskbar(state) } {
            return;
        }
        let Some(taskbar) = state.taskbar.as_ref() else {
            return;
        };
        let _ = unsafe { taskbar.SetProgressValue(hwnd, position.min(duration), duration) };
    }

    unsafe fn update_button(hwnd: HWND, state: &mut ThumbState) {
        if !state.added {
            unsafe { ensure_added(hwnd, state) };
        }
        if let Some(taskbar) = state.taskbar.as_ref()
            && state.added
        {
            let button = make_button(state);
            let _ = unsafe { taskbar.ThumbBarUpdateButtons(hwnd, &[button]) };
        }
    }

    fn make_button(state: &ThumbState) -> THUMBBUTTON {
        let (icon, tip, flags) = match state.state {
            ButtonState::Hidden => (state.play_icon, "Play", THBF_ENABLED | THBF_HIDDEN),
            ButtonState::Playing => (state.pause_icon, "Pause", THBF_ENABLED),
            ButtonState::Paused => (state.play_icon, "Play", THBF_ENABLED),
        };
        let mut button = THUMBBUTTON {
            dwMask: THB_ICON | THB_TOOLTIP | THB_FLAGS,
            iId: BUTTON_ID,
            hIcon: icon,
            dwFlags: flags,
            ..Default::default()
        };
        write_tip(&mut button.szTip, tip);
        button
    }

    fn write_tip(dst: &mut [u16; 260], text: &str) {
        let mut i = 0;
        for unit in text.encode_utf16() {
            if i + 1 >= dst.len() {
                break;
            }
            dst[i] = unit;
            i += 1;
        }
        dst[i] = 0;
    }

    unsafe fn destroy_icons(state: &ThumbState) {
        let _ = unsafe { DestroyIcon(state.play_icon) };
        let _ = unsafe { DestroyIcon(state.pause_icon) };
    }

    fn hiword(value: usize) -> u32 {
        ((value >> 16) & 0xFFFF) as u32
    }

    fn loword(value: usize) -> u32 {
        (value & 0xFFFF) as u32
    }

    /// Build a 32bpp icon by filling an ARGB canvas (top-down) with `draw`.
    unsafe fn make_icon(draw: fn(&mut [u32])) -> Option<HICON> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: ICON_SIZE,
                // Negative height selects a top-down bitmap (row 0 is the top).
                biHeight: -ICON_SIZE,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut c_void = std::ptr::null_mut();
        let color =
            unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) }.ok()?;
        if bits.is_null() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(color.0));
            }
            return None;
        }

        let pixels = unsafe {
            std::slice::from_raw_parts_mut(bits.cast::<u32>(), (ICON_SIZE * ICON_SIZE) as usize)
        };
        pixels.fill(0);
        draw(pixels);

        // A 1bpp mask is required, but with a 32bpp color bitmap the alpha
        // channel governs transparency, so the mask contents are irrelevant.
        let mask = unsafe { CreateBitmap(ICON_SIZE, ICON_SIZE, 1, 1, None) };
        let icon_info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = unsafe { CreateIconIndirect(&icon_info) }.ok();

        unsafe {
            let _ = DeleteObject(HGDIOBJ(color.0));
            let _ = DeleteObject(HGDIOBJ(mask.0));
        }
        icon
    }

    /// Opaque white; the DIB is straight-alpha ARGB stored as `0xAARRGGBB`.
    const GLYPH: u32 = 0xFFFF_FFFF;

    /// A right-pointing triangle.
    fn draw_play(pixels: &mut [u32]) {
        let size = ICON_SIZE;
        let half_height = 9.0_f32;
        for y in 7i32..=25 {
            let distance = (y - 16).abs() as f32;
            if distance > half_height {
                continue;
            }
            let right = 10.0 + 14.0 * (1.0 - distance / half_height);
            for x in 10..=right as i32 {
                if (0..size).contains(&x) {
                    pixels[(y * size + x) as usize] = GLYPH;
                }
            }
        }
    }

    /// Two vertical bars.
    fn draw_pause(pixels: &mut [u32]) {
        let size = ICON_SIZE;
        for y in 7i32..=25 {
            for x in 9..=13 {
                pixels[(y * size + x) as usize] = GLYPH;
            }
            for x in 18..=22 {
                pixels[(y * size + x) as usize] = GLYPH;
            }
        }
    }
}
