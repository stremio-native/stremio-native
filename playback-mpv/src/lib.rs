//! Native MPV playback and render integration primitives.
//!
//! On Windows the crate links the import library for the optimized MPV DLL
//! pinned by `mpv.lock.json`; other desktop platforms resolve dynamic libmpv
//! through the system toolchain. It validates the client API, serializes player
//! commands on one actor thread, and exposes a render context for the GUI.

#![deny(unsafe_op_in_unsafe_fn)]

mod actor;
mod ffi;
mod preview;
mod render;
mod spatial_audio;
mod thumbnail;

pub use actor::{
    AudioTrack, Chapter, EndReason, HdrMode, OmniphonyAudioSettings, PlaybackCommand,
    PlaybackController, PlaybackEvent, PlaybackRuntime, PlaybackState, PlayerConfig,
    SpatialAudioMode, SubtitleTrack,
};
pub use ffi::{ApiVersion, HEADER_CLIENT_API_VERSION, MpvError};
pub use preview::{
    PreviewConfig, PreviewController, PreviewEvent, PreviewFrame, PreviewRuntime, PreviewSource,
    PreviewUnavailableReason,
};
pub use render::{
    OpenGlContextProfile, OpenGlDiagnostics, OpenGlProcAddress, OpenGlProfile, RenderContext,
    RenderOutcome, RenderSource, VideoShaderSupport, VideoShaderUnsupportedReason, VideoTexture,
};
pub use thumbnail::{
    ThumbnailConfig, ThumbnailController, ThumbnailEvent, ThumbnailFrame, ThumbnailQuality,
    ThumbnailRequest, ThumbnailRuntime, ThumbnailSource, ThumbnailUnavailableReason,
};
