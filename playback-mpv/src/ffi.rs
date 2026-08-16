use std::{
    env,
    ffi::{CStr, CString, c_char, c_int, c_ulong, c_void},
    path::{Path, PathBuf},
    ptr::NonNull,
    sync::Arc,
};

use libloading::Library;
use thiserror::Error;

pub const FORMAT_NONE: c_int = 0;
pub const FORMAT_STRING: c_int = 1;
pub const FORMAT_FLAG: c_int = 3;
pub const FORMAT_INT64: c_int = 4;
pub const FORMAT_DOUBLE: c_int = 5;
pub const FORMAT_NODE: c_int = 6;
pub const FORMAT_NODE_ARRAY: c_int = 7;
pub const FORMAT_NODE_MAP: c_int = 8;
pub const FORMAT_BYTE_ARRAY: c_int = 9;

pub const EVENT_NONE: c_int = 0;
pub const EVENT_SHUTDOWN: c_int = 1;
pub const EVENT_COMMAND_REPLY: c_int = 5;
pub const EVENT_START_FILE: c_int = 6;
pub const EVENT_END_FILE: c_int = 7;
pub const EVENT_FILE_LOADED: c_int = 8;
pub const EVENT_CLIENT_MESSAGE: c_int = 16;
pub const EVENT_PLAYBACK_RESTART: c_int = 21;
pub const EVENT_PROPERTY_CHANGE: c_int = 22;
pub const EVENT_QUEUE_OVERFLOW: c_int = 24;

pub const END_FILE_EOF: c_int = 0;
pub const END_FILE_STOP: c_int = 2;
pub const END_FILE_QUIT: c_int = 3;
pub const END_FILE_ERROR: c_int = 4;
pub const END_FILE_REDIRECT: c_int = 5;

pub const RENDER_PARAM_INVALID: c_int = 0;
pub const RENDER_PARAM_API_TYPE: c_int = 1;
pub const RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
pub const RENDER_PARAM_OPENGL_FBO: c_int = 3;
pub const RENDER_PARAM_FLIP_Y: c_int = 4;
pub const RENDER_PARAM_ADVANCED_CONTROL: c_int = 10;
pub const RENDER_PARAM_SKIP_RENDERING: c_int = 13;
pub const RENDER_UPDATE_FRAME: u64 = 1 << 0;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ApiVersion {
    pub major: u16,
    pub minor: u16,
}

impl ApiVersion {
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    pub const fn decode(encoded: u64) -> Self {
        Self {
            major: ((encoded >> 16) & 0xffff) as u16,
            minor: (encoded & 0xffff) as u16,
        }
    }

    pub const fn encode(self) -> u64 {
        ((self.major as u64) << 16) | self.minor as u64
    }

    fn ensure_compatible(self) -> Result<(), MpvError> {
        if self.major != HEADER_CLIENT_API_VERSION.major {
            return Err(MpvError::IncompatibleApi {
                required: HEADER_CLIENT_API_VERSION,
                runtime: self,
            });
        }
        if self.minor < HEADER_CLIENT_API_VERSION.minor {
            return Err(MpvError::RuntimeTooOld {
                required: HEADER_CLIENT_API_VERSION,
                runtime: self,
            });
        }
        Ok(())
    }
}

include!("pinned.rs");

#[derive(Debug, Error)]
pub enum MpvError {
    #[error("libmpv client API mismatch: bindings require {required:?}, runtime is {runtime:?}")]
    IncompatibleApi {
        required: ApiVersion,
        runtime: ApiVersion,
    },
    #[error("libmpv runtime is too old: bindings require {required:?}, runtime is {runtime:?}")]
    RuntimeTooOld {
        required: ApiVersion,
        runtime: ApiVersion,
    },
    #[error("could not load MPV runtime {}: {message}", path.display())]
    RuntimeLoad { path: PathBuf, message: String },
    #[error(
        "MPV runtime {} does not export required symbol {symbol}: {message}",
        path.display()
    )]
    RuntimeSymbol {
        path: PathBuf,
        symbol: &'static str,
        message: String,
    },
    #[error("libmpv returned a null player handle")]
    NullHandle,
    #[error("libmpv returned a null render context")]
    NullRenderContext,
    #[error("value contains an interior null byte: {0}")]
    InvalidString(#[from] std::ffi::NulError),
    #[error("string-list property contains too many values: {len}")]
    StringListTooLong { len: usize },
    #[error("invalid data returned by libmpv: {0}")]
    InvalidNode(String),
    #[error("libmpv operation failed ({code}): {message}")]
    Operation { code: c_int, message: String },
    #[error("MPV command queue is full")]
    CommandQueueFull,
    #[error("MPV command queue has closed")]
    CommandQueueClosed,
    #[error("MPV actor thread panicked")]
    ActorPanicked,
    #[error("MPV thumbnail worker has closed")]
    ThumbnailWorkerClosed,
    #[error("MPV thumbnail worker thread panicked")]
    ThumbnailWorkerPanicked,
    #[error("OpenGL rendering is unsupported on this platform")]
    UnsupportedOpenGl,
    #[error("OpenGL operation failed: {0}")]
    OpenGl(String),
    #[error("OpenGL video framebuffer is incomplete (status {status:#x})")]
    IncompleteOpenGlFramebuffer { status: u32 },
}

#[repr(C)]
pub struct MpvHandle {
    _private: [u8; 0],
}

#[repr(C)]
pub struct MpvRenderContext {
    _private: [u8; 0],
}

#[repr(C)]
pub struct MpvEvent {
    pub event_id: c_int,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct MpvEventProperty {
    pub name: *const c_char,
    pub format: c_int,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct MpvEventClientMessage {
    pub num_args: c_int,
    pub args: *const *const c_char,
}

#[repr(C)]
pub struct MpvEventEndFile {
    pub reason: c_int,
    pub error: c_int,
    pub playlist_entry_id: i64,
    pub playlist_insert_id: i64,
    pub playlist_insert_num_entries: c_int,
}

#[repr(C)]
pub union MpvNodeValue {
    pub string: *mut c_char,
    pub flag: c_int,
    pub int64: i64,
    pub double_: f64,
    pub list: *mut MpvNodeList,
    pub byte_array: *mut MpvByteArray,
}

#[repr(C)]
pub struct MpvNode {
    pub value: MpvNodeValue,
    pub format: c_int,
}

#[repr(C)]
pub struct MpvNodeList {
    pub num: c_int,
    pub values: *mut MpvNode,
    pub keys: *mut *mut c_char,
}

#[repr(C)]
pub struct MpvByteArray {
    pub data: *mut c_void,
    pub size: usize,
}

#[repr(C)]
pub struct MpvRenderParam {
    pub param_type: c_int,
    pub data: *mut c_void,
}

pub type OpenGlGetProcAddress = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;

#[repr(C)]
pub struct MpvOpenGlInitParams {
    pub get_proc_address: Option<OpenGlGetProcAddress>,
    pub get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
pub struct MpvOpenGlFbo {
    pub fbo: c_int,
    pub width: c_int,
    pub height: c_int,
    pub internal_format: c_int,
}

type ClientApiVersionFn = unsafe extern "C" fn() -> c_ulong;
type ErrorStringFn = unsafe extern "C" fn(c_int) -> *const c_char;
type CreateFn = unsafe extern "C" fn() -> *mut MpvHandle;
type InitializeFn = unsafe extern "C" fn(*mut MpvHandle) -> c_int;
type TerminateDestroyFn = unsafe extern "C" fn(*mut MpvHandle);
type SetOptionStringFn =
    unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int;
type SetPropertyFn =
    unsafe extern "C" fn(*mut MpvHandle, *const c_char, c_int, *mut c_void) -> c_int;
type SetPropertyStringFn =
    unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int;
type GetPropertyFn =
    unsafe extern "C" fn(*mut MpvHandle, *const c_char, c_int, *mut c_void) -> c_int;
type FreeFn = unsafe extern "C" fn(*mut c_void);
type CommandFn = unsafe extern "C" fn(*mut MpvHandle, *const *const c_char) -> c_int;
type CommandRetFn =
    unsafe extern "C" fn(*mut MpvHandle, *const *const c_char, *mut MpvNode) -> c_int;
type FreeNodeContentsFn = unsafe extern "C" fn(*mut MpvNode);
type CommandAsyncFn = unsafe extern "C" fn(*mut MpvHandle, u64, *const *const c_char) -> c_int;
type AbortAsyncCommandFn = unsafe extern "C" fn(*mut MpvHandle, u64);
type ObservePropertyFn = unsafe extern "C" fn(*mut MpvHandle, u64, *const c_char, c_int) -> c_int;
type WaitEventFn = unsafe extern "C" fn(*mut MpvHandle, f64) -> *mut MpvEvent;
pub(crate) type WakeupCallback = unsafe extern "C" fn(*mut c_void);
type SetWakeupCallbackFn =
    unsafe extern "C" fn(*mut MpvHandle, Option<WakeupCallback>, *mut c_void);
type RenderCreateFn =
    unsafe extern "C" fn(*mut *mut MpvRenderContext, *mut MpvHandle, *mut MpvRenderParam) -> c_int;
type RenderUpdateCallback = unsafe extern "C" fn(*mut c_void);
type RenderSetUpdateCallbackFn =
    unsafe extern "C" fn(*mut MpvRenderContext, Option<RenderUpdateCallback>, *mut c_void);
type RenderUpdateFn = unsafe extern "C" fn(*mut MpvRenderContext) -> u64;
type RenderFn = unsafe extern "C" fn(*mut MpvRenderContext, *mut MpvRenderParam) -> c_int;
type RenderFreeFn = unsafe extern "C" fn(*mut MpvRenderContext);

enum MpvRuntimeModule {
    Linked,
    Dynamic { _library: Library },
}

// The symbols are provided by the pinned MPV import library selected by build.rs.
// Keeping this list explicit makes the unsafe ABI surface small and auditable.
unsafe extern "C" {
    fn mpv_client_api_version() -> c_ulong;
    fn mpv_error_string(code: c_int) -> *const c_char;
    fn mpv_create() -> *mut MpvHandle;
    fn mpv_initialize(handle: *mut MpvHandle) -> c_int;
    fn mpv_terminate_destroy(handle: *mut MpvHandle);
    fn mpv_set_option_string(
        handle: *mut MpvHandle,
        name: *const c_char,
        value: *const c_char,
    ) -> c_int;
    fn mpv_set_property(
        handle: *mut MpvHandle,
        name: *const c_char,
        format: c_int,
        data: *mut c_void,
    ) -> c_int;
    fn mpv_set_property_string(
        handle: *mut MpvHandle,
        name: *const c_char,
        value: *const c_char,
    ) -> c_int;
    fn mpv_get_property(
        handle: *mut MpvHandle,
        name: *const c_char,
        format: c_int,
        data: *mut c_void,
    ) -> c_int;
    fn mpv_free(data: *mut c_void);
    fn mpv_command(handle: *mut MpvHandle, args: *const *const c_char) -> c_int;
    fn mpv_command_ret(
        handle: *mut MpvHandle,
        args: *const *const c_char,
        result: *mut MpvNode,
    ) -> c_int;
    fn mpv_free_node_contents(node: *mut MpvNode);
    fn mpv_command_async(
        handle: *mut MpvHandle,
        reply_userdata: u64,
        args: *const *const c_char,
    ) -> c_int;
    fn mpv_abort_async_command(handle: *mut MpvHandle, reply_userdata: u64);
    fn mpv_observe_property(
        handle: *mut MpvHandle,
        reply_userdata: u64,
        name: *const c_char,
        format: c_int,
    ) -> c_int;
    fn mpv_wait_event(handle: *mut MpvHandle, timeout: f64) -> *mut MpvEvent;
    fn mpv_set_wakeup_callback(
        handle: *mut MpvHandle,
        callback: Option<WakeupCallback>,
        context: *mut c_void,
    );
    fn mpv_render_context_create(
        context: *mut *mut MpvRenderContext,
        handle: *mut MpvHandle,
        params: *mut MpvRenderParam,
    ) -> c_int;
    fn mpv_render_context_set_update_callback(
        context: *mut MpvRenderContext,
        callback: Option<RenderUpdateCallback>,
        callback_context: *mut c_void,
    );
    fn mpv_render_context_update(context: *mut MpvRenderContext) -> u64;
    fn mpv_render_context_render(
        context: *mut MpvRenderContext,
        params: *mut MpvRenderParam,
    ) -> c_int;
    fn mpv_render_context_free(context: *mut MpvRenderContext);
}

pub struct MpvApi {
    _runtime: MpvRuntimeModule,
    client_api_version: ClientApiVersionFn,
    error_string: ErrorStringFn,
    create: CreateFn,
    initialize: InitializeFn,
    terminate_destroy: TerminateDestroyFn,
    set_option_string: SetOptionStringFn,
    set_property: SetPropertyFn,
    set_property_string: SetPropertyStringFn,
    get_property: GetPropertyFn,
    free: FreeFn,
    command: CommandFn,
    command_ret: CommandRetFn,
    free_node_contents: FreeNodeContentsFn,
    command_async: CommandAsyncFn,
    abort_async_command: AbortAsyncCommandFn,
    observe_property: ObservePropertyFn,
    wait_event: WaitEventFn,
    set_wakeup_callback: SetWakeupCallbackFn,
    pub render_create: RenderCreateFn,
    pub render_set_update_callback: RenderSetUpdateCallbackFn,
    pub render_update: RenderUpdateFn,
    pub render: RenderFn,
    pub render_free: RenderFreeFn,
}

impl MpvApi {
    /// Selects the runtime that drives playback: an explicitly configured
    /// module, the adjacent Omniphony bundle, or the pinned linked build.
    pub fn playback_runtime() -> Result<Arc<Self>, MpvError> {
        if let Some(path) = env::var_os("STREMIO_MPV_RUNTIME").filter(|path| !path.is_empty()) {
            let path = PathBuf::from(path);
            let api = Self::dynamic(&path)?;
            tracing::info!(path = %path.display(), "using explicitly configured MPV runtime");
            return Ok(Arc::new(api));
        }

        for path in adjacent_omniphony_runtime_candidates() {
            if !path.is_file() {
                continue;
            }
            match Self::dynamic(&path) {
                Ok(api) => {
                    tracing::info!(path = %path.display(), "using adjacent Omniphony MPV runtime");
                    return Ok(Arc::new(api));
                }
                Err(error) => tracing::warn!(
                    path = %path.display(),
                    %error,
                    "could not use adjacent Omniphony MPV runtime; falling back to linked libmpv"
                ),
            }
        }

        Self::pinned_runtime()
    }

    /// Returns the pinned build linked into this application.
    ///
    /// Swappable runtimes are deliberately not considered here. Components that
    /// gain nothing from them stay on the build the application is tested
    /// against, so a defect in a bundled or user-supplied module cannot reach
    /// them.
    pub fn pinned_runtime() -> Result<Arc<Self>, MpvError> {
        let api = Self::from_linked();
        api.api_version().ensure_compatible()?;
        tracing::debug!("using linked MPV runtime");
        Ok(Arc::new(api))
    }

    fn from_linked() -> Self {
        Self {
            _runtime: MpvRuntimeModule::Linked,
            client_api_version: mpv_client_api_version,
            error_string: mpv_error_string,
            create: mpv_create,
            initialize: mpv_initialize,
            terminate_destroy: mpv_terminate_destroy,
            set_option_string: mpv_set_option_string,
            set_property: mpv_set_property,
            set_property_string: mpv_set_property_string,
            get_property: mpv_get_property,
            free: mpv_free,
            command: mpv_command,
            command_ret: mpv_command_ret,
            free_node_contents: mpv_free_node_contents,
            command_async: mpv_command_async,
            abort_async_command: mpv_abort_async_command,
            observe_property: mpv_observe_property,
            wait_event: mpv_wait_event,
            set_wakeup_callback: mpv_set_wakeup_callback,
            render_create: mpv_render_context_create,
            render_set_update_callback: mpv_render_context_set_update_callback,
            render_update: mpv_render_context_update,
            render: mpv_render_context_render,
            render_free: mpv_render_context_free,
        }
    }

    fn dynamic(path: &Path) -> Result<Self, MpvError> {
        #[cfg(target_os = "windows")]
        if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
        {
            return Err(MpvError::RuntimeLoad {
                path: path.to_owned(),
                message: "Windows executables cannot be used safely as in-process libmpv modules; provide libmpv-2.dll instead".to_owned(),
            });
        }

        // SAFETY: Loading a user-selected native module executes its process
        // initialization. Only an explicit path or the app-owned adjacent
        // Omniphony bundle is considered here.
        let library = unsafe { Library::new(path) }.map_err(|error| MpvError::RuntimeLoad {
            path: path.to_owned(),
            message: error.to_string(),
        })?;

        let api = Self {
            client_api_version: load_runtime_symbol(
                &library,
                path,
                b"mpv_client_api_version\0",
                "mpv_client_api_version",
            )?,
            error_string: load_runtime_symbol(
                &library,
                path,
                b"mpv_error_string\0",
                "mpv_error_string",
            )?,
            create: load_runtime_symbol(&library, path, b"mpv_create\0", "mpv_create")?,
            initialize: load_runtime_symbol(&library, path, b"mpv_initialize\0", "mpv_initialize")?,
            terminate_destroy: load_runtime_symbol(
                &library,
                path,
                b"mpv_terminate_destroy\0",
                "mpv_terminate_destroy",
            )?,
            set_option_string: load_runtime_symbol(
                &library,
                path,
                b"mpv_set_option_string\0",
                "mpv_set_option_string",
            )?,
            set_property: load_runtime_symbol(
                &library,
                path,
                b"mpv_set_property\0",
                "mpv_set_property",
            )?,
            set_property_string: load_runtime_symbol(
                &library,
                path,
                b"mpv_set_property_string\0",
                "mpv_set_property_string",
            )?,
            get_property: load_runtime_symbol(
                &library,
                path,
                b"mpv_get_property\0",
                "mpv_get_property",
            )?,
            free: load_runtime_symbol(&library, path, b"mpv_free\0", "mpv_free")?,
            command: load_runtime_symbol(&library, path, b"mpv_command\0", "mpv_command")?,
            command_ret: load_runtime_symbol(
                &library,
                path,
                b"mpv_command_ret\0",
                "mpv_command_ret",
            )?,
            free_node_contents: load_runtime_symbol(
                &library,
                path,
                b"mpv_free_node_contents\0",
                "mpv_free_node_contents",
            )?,
            command_async: load_runtime_symbol(
                &library,
                path,
                b"mpv_command_async\0",
                "mpv_command_async",
            )?,
            abort_async_command: load_runtime_symbol(
                &library,
                path,
                b"mpv_abort_async_command\0",
                "mpv_abort_async_command",
            )?,
            observe_property: load_runtime_symbol(
                &library,
                path,
                b"mpv_observe_property\0",
                "mpv_observe_property",
            )?,
            wait_event: load_runtime_symbol(&library, path, b"mpv_wait_event\0", "mpv_wait_event")?,
            set_wakeup_callback: load_runtime_symbol(
                &library,
                path,
                b"mpv_set_wakeup_callback\0",
                "mpv_set_wakeup_callback",
            )?,
            render_create: load_runtime_symbol(
                &library,
                path,
                b"mpv_render_context_create\0",
                "mpv_render_context_create",
            )?,
            render_set_update_callback: load_runtime_symbol(
                &library,
                path,
                b"mpv_render_context_set_update_callback\0",
                "mpv_render_context_set_update_callback",
            )?,
            render_update: load_runtime_symbol(
                &library,
                path,
                b"mpv_render_context_update\0",
                "mpv_render_context_update",
            )?,
            render: load_runtime_symbol(
                &library,
                path,
                b"mpv_render_context_render\0",
                "mpv_render_context_render",
            )?,
            render_free: load_runtime_symbol(
                &library,
                path,
                b"mpv_render_context_free\0",
                "mpv_render_context_free",
            )?,
            _runtime: MpvRuntimeModule::Dynamic { _library: library },
        };
        api.api_version().ensure_compatible()?;
        Ok(api)
    }

    // `c_ulong` is 64-bit on LP64 targets and 32-bit on Windows, so widening it
    // to `u64` is a no-op on one target and load-bearing on the other. No
    // expression satisfies Clippy on both: `as u64` trips `unnecessary_cast` on
    // Linux, and `u64::from` trips `useless_conversion` there instead. This is
    // `allow` rather than `expect` because the lint genuinely does not fire on
    // Windows, where `expect` would itself warn as unfulfilled.
    #[allow(
        clippy::unnecessary_cast,
        reason = "c_ulong is already u64 on LP64 targets but u32 on Windows"
    )]
    pub fn api_version(&self) -> ApiVersion {
        // SAFETY: Linked and dynamically resolved symbols both use the pinned
        // client.h signature, and dynamic runtimes are version-checked during
        // construction.
        ApiVersion::decode(unsafe { (self.client_api_version)() } as u64)
    }

    pub fn operation_error(&self, code: c_int) -> MpvError {
        // SAFETY: MPV returns a static string for every error code.
        let message = unsafe { (self.error_string)(code) };
        let message = if message.is_null() {
            "unknown MPV error".to_owned()
        } else {
            // SAFETY: The MPV contract guarantees a null-terminated string.
            unsafe { CStr::from_ptr(message) }
                .to_string_lossy()
                .into_owned()
        };
        MpvError::Operation { code, message }
    }

    pub fn result(&self, code: c_int) -> Result<(), MpvError> {
        if code < 0 {
            Err(self.operation_error(code))
        } else {
            Ok(())
        }
    }
}

fn load_runtime_symbol<T: Copy>(
    library: &Library,
    path: &Path,
    name: &'static [u8],
    label: &'static str,
) -> Result<T, MpvError> {
    // SAFETY: Each caller supplies the exact function-pointer type from the
    // pinned mpv/client.h ABI. The module remains owned by MpvApi for at least
    // as long as any copied function pointer can be called.
    unsafe { library.get::<T>(name) }
        .map(|symbol| *symbol)
        .map_err(|error| MpvError::RuntimeSymbol {
            path: path.to_owned(),
            symbol: label,
            message: error.to_string(),
        })
}

fn adjacent_omniphony_runtime_candidates() -> Vec<PathBuf> {
    let Ok(executable) = env::current_exe() else {
        return Vec::new();
    };
    let Some(directory) = executable.parent() else {
        return Vec::new();
    };

    #[cfg(target_os = "windows")]
    {
        vec![directory.join("mpv-omniphony").join("libmpv-2.dll")]
    }
    #[cfg(not(target_os = "windows"))]
    {
        vec![directory.join("mpv-omniphony").join("libmpv.so")]
    }
}

pub struct MpvClient {
    pub api: Arc<MpvApi>,
    handle: NonNull<MpvHandle>,
}

pub(crate) struct MpvOwnedNode {
    api: Arc<MpvApi>,
    node: MpvNode,
}

impl MpvOwnedNode {
    pub(crate) fn as_node(&self) -> &MpvNode {
        &self.node
    }
}

impl Drop for MpvOwnedNode {
    fn drop(&mut self) {
        // SAFETY: `node` was populated by a successful `mpv_command_ret` call
        // and has not been mutated or freed since then.
        unsafe { (self.api.free_node_contents)(&mut self.node) };
    }
}

// SAFETY: MPV's client API is thread-safe. This wrapper serializes normal
// player operations on the actor thread; the render API uses its own context.
unsafe impl Send for MpvClient {}
// SAFETY: Shared access only exposes thread-safe MPV entry points. Handle
// destruction happens after actor and render-context ownership is released.
unsafe impl Sync for MpvClient {}

impl MpvClient {
    pub fn create(api: Arc<MpvApi>) -> Result<Arc<Self>, MpvError> {
        // SAFETY: Function pointer was validated during MpvApi construction.
        let handle = NonNull::new(unsafe { (api.create)() }).ok_or(MpvError::NullHandle)?;
        Ok(Arc::new(Self { api, handle }))
    }

    pub fn handle(&self) -> *mut MpvHandle {
        self.handle.as_ptr()
    }

    pub fn set_option(&self, name: &str, value: &str) -> Result<(), MpvError> {
        let name = CString::new(name)?;
        let value = CString::new(value)?;
        // SAFETY: Pointers remain valid through the synchronous MPV call.
        self.api.result(unsafe {
            (self.api.set_option_string)(self.handle(), name.as_ptr(), value.as_ptr())
        })
    }

    pub fn initialize(&self) -> Result<(), MpvError> {
        // SAFETY: Handle is valid and initialized exactly once by PlaybackRuntime.
        self.api
            .result(unsafe { (self.api.initialize)(self.handle()) })
    }

    pub fn command(&self, args: &[&str]) -> Result<(), MpvError> {
        let strings = args
            .iter()
            .map(|arg| CString::new(*arg))
            .collect::<Result<Vec<_>, _>>()?;
        let mut pointers = strings.iter().map(|arg| arg.as_ptr()).collect::<Vec<_>>();
        pointers.push(std::ptr::null());
        // SAFETY: All strings and the null-terminated pointer array remain valid
        // for the duration of the synchronous command.
        self.api
            .result(unsafe { (self.api.command)(self.handle(), pointers.as_ptr()) })
    }

    pub(crate) fn command_result(&self, args: &[&str]) -> Result<MpvOwnedNode, MpvError> {
        let strings = args
            .iter()
            .map(|arg| CString::new(*arg))
            .collect::<Result<Vec<_>, _>>()?;
        let mut pointers = strings.iter().map(|arg| arg.as_ptr()).collect::<Vec<_>>();
        pointers.push(std::ptr::null());
        let mut node = MpvNode {
            value: MpvNodeValue { int64: 0 },
            format: FORMAT_NONE,
        };
        // SAFETY: The arguments remain alive and null-terminated during the
        // synchronous call. MPV initializes `node` on success and transfers
        // ownership of its nested allocations to the caller.
        let code = unsafe { (self.api.command_ret)(self.handle(), pointers.as_ptr(), &mut node) };
        self.api.result(code)?;
        Ok(MpvOwnedNode {
            api: self.api.clone(),
            node,
        })
    }

    pub fn command_async(&self, reply_userdata: u64, args: &[&str]) -> Result<(), MpvError> {
        let strings = args
            .iter()
            .map(|arg| CString::new(*arg))
            .collect::<Result<Vec<_>, _>>()?;
        let mut pointers = strings.iter().map(|arg| arg.as_ptr()).collect::<Vec<_>>();
        pointers.push(std::ptr::null());
        // SAFETY: MPV parses and queues the command before returning. The
        // completion result is delivered later through MPV_EVENT_COMMAND_REPLY.
        self.api.result(unsafe {
            (self.api.command_async)(self.handle(), reply_userdata, pointers.as_ptr())
        })
    }

    pub fn abort_async_command(&self, reply_userdata: u64) {
        // SAFETY: The id is an opaque value previously supplied to this same
        // client. MPV performs cancellation asynchronously and retains no Rust
        // references.
        unsafe { (self.api.abort_async_command)(self.handle(), reply_userdata) };
    }

    pub fn set_flag(&self, name: &str, enabled: bool) -> Result<(), MpvError> {
        let name = CString::new(name)?;
        let mut value: c_int = enabled.into();
        // SAFETY: MPV copies the scalar during this synchronous call.
        self.api.result(unsafe {
            (self.api.set_property)(
                self.handle(),
                name.as_ptr(),
                FORMAT_FLAG,
                (&mut value as *mut c_int).cast(),
            )
        })
    }

    pub fn set_double(&self, name: &str, mut value: f64) -> Result<(), MpvError> {
        let name = CString::new(name)?;
        // SAFETY: MPV copies the scalar during this synchronous call.
        self.api.result(unsafe {
            (self.api.set_property)(
                self.handle(),
                name.as_ptr(),
                FORMAT_DOUBLE,
                (&mut value as *mut f64).cast(),
            )
        })
    }

    pub fn set_string(&self, name: &str, value: &str) -> Result<(), MpvError> {
        let name = CString::new(name)?;
        let value = CString::new(value)?;
        // SAFETY: Strings remain valid for the synchronous call.
        self.api.result(unsafe {
            (self.api.set_property_string)(self.handle(), name.as_ptr(), value.as_ptr())
        })
    }

    pub(crate) fn get_string(&self, name: &str) -> Result<String, MpvError> {
        let name = CString::new(name)?;
        let mut value: *mut c_char = std::ptr::null_mut();
        // SAFETY: MPV writes one allocated C-string pointer into `value` on
        // success. The property name remains valid for the synchronous call.
        let code = unsafe {
            (self.api.get_property)(
                self.handle(),
                name.as_ptr(),
                FORMAT_STRING,
                (&mut value as *mut *mut c_char).cast(),
            )
        };
        self.api.result(code)?;
        let value = NonNull::new(value).ok_or_else(|| {
            MpvError::InvalidNode("string property returned a null pointer".to_owned())
        })?;
        // SAFETY: A successful MPV_FORMAT_STRING read returns a valid,
        // null-terminated allocation owned by MPV.
        let result = unsafe { CStr::from_ptr(value.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: `value` is the allocation returned by `mpv_get_property` and
        // is freed exactly once after its contents have been copied.
        unsafe { (self.api.free)(value.as_ptr().cast()) };
        Ok(result)
    }

    pub(crate) fn get_flag(&self, name: &str) -> Result<bool, MpvError> {
        let name = CString::new(name)?;
        let mut value: c_int = 0;
        // SAFETY: MPV synchronously writes one `int` to the valid scalar
        // pointer, using the ABI defined by `MPV_FORMAT_FLAG`.
        let code = unsafe {
            (self.api.get_property)(
                self.handle(),
                name.as_ptr(),
                FORMAT_FLAG,
                (&mut value as *mut c_int).cast(),
            )
        };
        self.api.result(code)?;
        Ok(value != 0)
    }

    pub(crate) fn get_i64(&self, name: &str) -> Result<i64, MpvError> {
        let name = CString::new(name)?;
        let mut value = 0_i64;
        // SAFETY: MPV synchronously writes one `int64_t` to the valid scalar
        // pointer, using the ABI defined by `MPV_FORMAT_INT64`.
        let code = unsafe {
            (self.api.get_property)(
                self.handle(),
                name.as_ptr(),
                FORMAT_INT64,
                (&mut value as *mut i64).cast(),
            )
        };
        self.api.result(code)?;
        Ok(value)
    }

    pub(crate) fn get_double(&self, name: &str) -> Result<f64, MpvError> {
        let name = CString::new(name)?;
        let mut value = 0.0_f64;
        // SAFETY: MPV synchronously writes one `double` to the valid scalar
        // pointer, using the ABI defined by `MPV_FORMAT_DOUBLE`.
        let code = unsafe {
            (self.api.get_property)(
                self.handle(),
                name.as_ptr(),
                FORMAT_DOUBLE,
                (&mut value as *mut f64).cast(),
            )
        };
        self.api.result(code)?;
        Ok(value)
    }

    pub(crate) fn get_node(&self, name: &str) -> Result<MpvOwnedNode, MpvError> {
        let name = CString::new(name)?;
        let mut node = MpvNode {
            value: MpvNodeValue { int64: 0 },
            format: FORMAT_NONE,
        };
        // SAFETY: MPV initializes `node` on success and transfers ownership of
        // its nested allocations to the caller. The property name remains
        // valid for the synchronous call.
        let code = unsafe {
            (self.api.get_property)(
                self.handle(),
                name.as_ptr(),
                FORMAT_NODE,
                (&mut node as *mut MpvNode).cast(),
            )
        };
        self.api.result(code)?;
        Ok(MpvOwnedNode {
            api: self.api.clone(),
            node,
        })
    }

    pub fn set_string_list(&self, name: &str, values: &[String]) -> Result<(), MpvError> {
        let name = CString::new(name)?;
        let (strings, mut nodes) = string_list_nodes(values)?;
        let mut list = MpvNodeList {
            num: c_int::try_from(nodes.len())
                .map_err(|_| MpvError::StringListTooLong { len: nodes.len() })?,
            values: if nodes.is_empty() {
                std::ptr::null_mut()
            } else {
                nodes.as_mut_ptr()
            },
            keys: std::ptr::null_mut(),
        };
        let mut value = MpvNode {
            value: MpvNodeValue {
                list: &mut list as *mut MpvNodeList,
            },
            format: FORMAT_NODE_ARRAY,
        };
        // SAFETY: MPV synchronously copies the complete caller-owned node tree.
        // `strings`, `nodes`, and `list` remain alive and immovable for the call.
        let result = self.api.result(unsafe {
            (self.api.set_property)(
                self.handle(),
                name.as_ptr(),
                FORMAT_NODE,
                (&mut value as *mut MpvNode).cast(),
            )
        });
        drop(strings);
        result
    }

    pub fn observe(&self, id: u64, name: &str, format: c_int) -> Result<(), MpvError> {
        let name = CString::new(name)?;
        // SAFETY: MPV copies the property name and posts values to this handle.
        self.api.result(unsafe {
            (self.api.observe_property)(self.handle(), id, name.as_ptr(), format)
        })
    }

    pub fn wait_event(&self, timeout: f64) -> *mut MpvEvent {
        // SAFETY: The returned pointer remains valid until the next wait call.
        unsafe { (self.api.wait_event)(self.handle(), timeout) }
    }

    pub fn set_wakeup_callback(&self, callback: Option<WakeupCallback>, context: *mut c_void) {
        // SAFETY: MPV only retains the callback and opaque context. The actor
        // unregisters it before the context owner is released.
        unsafe { (self.api.set_wakeup_callback)(self.handle(), callback, context) };
    }
}

fn string_list_nodes(values: &[String]) -> Result<(Vec<CString>, Vec<MpvNode>), MpvError> {
    let strings = values
        .iter()
        .map(|value| CString::new(value.as_str()))
        .collect::<Result<Vec<_>, _>>()?;
    let nodes = strings
        .iter()
        .map(|value| MpvNode {
            value: MpvNodeValue {
                string: value.as_ptr().cast_mut(),
            },
            format: FORMAT_STRING,
        })
        .collect();
    Ok((strings, nodes))
}

impl Drop for MpvClient {
    fn drop(&mut self) {
        // SAFETY: This is the final Arc owner, so no render or actor call can be
        // in flight. The handle was created by this API and is destroyed once.
        unsafe { (self.api.terminate_destroy)(self.handle()) };
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;

    use super::{
        ApiVersion, FORMAT_STRING, HEADER_CLIENT_API_VERSION, MpvApi, MpvClient, MpvError,
        string_list_nodes,
    };

    #[test]
    fn dynamically_linked_engine_should_initialize() -> Result<(), MpvError> {
        let api = MpvApi::playback_runtime()?;
        let client = MpvClient::create(api)?;
        client.set_option("terminal", "no")?;
        client.set_option("vo", "libmpv")?;
        client.set_option("idle", "yes")?;
        client.initialize()
    }

    #[test]
    fn api_version_should_round_trip_packed_value() {
        let version = ApiVersion::new(2, 5);
        assert_eq!(ApiVersion::decode(version.encode()), version);
    }

    #[test]
    fn newer_minor_runtime_should_be_compatible() {
        assert!(ApiVersion::new(2, 8).ensure_compatible().is_ok());
    }

    #[test]
    fn older_runtime_should_be_rejected() {
        assert!(matches!(
            ApiVersion::new(2, 4).ensure_compatible(),
            Err(MpvError::RuntimeTooOld {
                required: HEADER_CLIENT_API_VERSION,
                ..
            })
        ));
    }

    #[test]
    fn string_list_nodes_support_empty_values() -> Result<(), MpvError> {
        let (strings, nodes) = string_list_nodes(&[])?;

        assert!(strings.is_empty());
        assert!(nodes.is_empty());
        Ok(())
    }

    #[test]
    fn string_list_nodes_preserve_single_path() -> Result<(), MpvError> {
        let path = String::from("~~/shaders/FSR.glsl");
        let (_strings, nodes) = string_list_nodes(std::slice::from_ref(&path))?;

        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].format, FORMAT_STRING);
        // SAFETY: The node points into `_strings`, which remains alive here.
        let actual = unsafe { CStr::from_ptr(nodes[0].value.string) }.to_string_lossy();
        assert_eq!(actual, path);
        Ok(())
    }

    #[test]
    fn string_list_nodes_keep_multiple_paths_separate() -> Result<(), MpvError> {
        let paths = vec![
            String::from("C:\\Shaders\\first.glsl"),
            String::from("/opt/shaders/second.glsl"),
        ];
        let (_strings, nodes) = string_list_nodes(&paths)?;
        let actual = nodes
            .iter()
            .map(|node| {
                // SAFETY: Every node points into `_strings`, which remains alive.
                unsafe { CStr::from_ptr(node.value.string) }
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();

        assert_eq!(actual, paths);
        assert!(actual.iter().all(|path| !path.contains(';')));
        Ok(())
    }
}
