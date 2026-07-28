//! Background GPU-driver pre-warm.
//!
//! On a cold Windows/Intel driver, libmpv's render-context creation stalls for
//! many seconds. With `advanced-control` enabled and a D3D11 hardware decoder
//! (`hwdec=d3d11va-copy`), that call synchronously spins up a D3D11 device, and
//! creating the first device cold-loads the D3D11 runtime and the Intel display
//! driver — the multi-second freeze seen on cold starts (see
//! `playback-mpv/src/render.rs` phase timings, where `render_create_ms` is the
//! whole cost).
//!
//! This creates a throwaway D3D11 device on a background thread first, so the
//! runtime and driver are already hot by the time libmpv needs them on the UI
//! thread. The device is kept alive for the process lifetime so the driver does
//! not cool back down. The MPV render context must be *gated* on [`is_ready`]:
//! warming only helps if it finishes before that synchronous call runs.

use std::sync::atomic::{AtomicBool, Ordering};

/// Set once warming has finished (or been skipped/failed). Non-Windows targets
/// have nothing to warm and start ready.
static READY: AtomicBool = AtomicBool::new(!cfg!(target_os = "windows"));

/// Whether libmpv may create its render context yet. Gate the (synchronous,
/// UI-thread) creation on this so it runs against a warm driver.
pub fn is_ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Start warming on a background thread. `on_complete` runs once warming
/// finishes — used to nudge a redraw so the now-unblocked render context is
/// created promptly.
#[cfg(target_os = "windows")]
pub fn spawn(on_complete: impl FnOnce() + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name("gpu-prewarm".to_owned())
        .spawn(move || {
            let start = std::time::Instant::now();
            match windows_impl::warm() {
                Ok(()) => tracing::info!(
                    elapsed_ms = start.elapsed().as_millis(),
                    "GPU driver pre-warm complete"
                ),
                Err(error) => tracing::warn!(%error, "GPU driver pre-warm skipped"),
            }
            READY.store(true, Ordering::Release);
            on_complete();
        });
    if spawned.is_err() {
        // Could not start the warmer; do not gate playback on it.
        READY.store(true, Ordering::Release);
        tracing::warn!("could not spawn the GPU pre-warm thread");
    }
}

#[cfg(not(target_os = "windows"))]
pub fn spawn(on_complete: impl FnOnce() + Send + 'static) {
    // Nothing to warm; `READY` is already true.
    on_complete();
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use windows::Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL},
            Direct3D11::{
                D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
            },
        },
    };

    pub(super) fn warm() -> Result<(), String> {
        let mut device: Option<ID3D11Device> = None;
        // SAFETY: a standard hardware device creation with default parameters;
        // all output pointers are valid for the call.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE(std::ptr::null_mut()),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut D3D_FEATURE_LEVEL::default()),
                None,
            )
        }
        .map_err(|error| format!("D3D11CreateDevice: {error}"))?;

        // Hold the device for the process lifetime so the driver stays hot;
        // libmpv creates its own device against the now-warm runtime.
        if let Some(device) = device {
            std::mem::forget(device);
        }
        Ok(())
    }
}
