use std::{
    ffi::{CStr, c_char, c_int, c_void},
    fs,
    net::UdpSocket,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::{
    RenderSource,
    ffi::{
        END_FILE_EOF, END_FILE_ERROR, END_FILE_QUIT, END_FILE_REDIRECT, END_FILE_STOP,
        EVENT_CLIENT_MESSAGE, EVENT_COMMAND_REPLY, EVENT_END_FILE, EVENT_FILE_LOADED, EVENT_NONE,
        EVENT_PLAYBACK_RESTART, EVENT_PROPERTY_CHANGE, EVENT_QUEUE_OVERFLOW, EVENT_SHUTDOWN,
        EVENT_START_FILE, FORMAT_DOUBLE, FORMAT_FLAG, FORMAT_INT64, FORMAT_NODE, FORMAT_NODE_ARRAY,
        FORMAT_NODE_MAP, FORMAT_NONE, FORMAT_STRING, MpvApi, MpvClient, MpvError, MpvEvent,
        MpvEventClientMessage, MpvEventEndFile, MpvEventProperty, MpvNode, MpvNodeList,
    },
    spatial_audio::{OrenderProbe, probe_orender},
};

const ADD_SUBTITLE_COMMAND_REPLY_ID: u64 = 1;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioTrack {
    pub id: String,
    pub title: Option<String>,
    pub language: Option<String>,
    pub codec: Option<String>,
    pub selected: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubtitleTrack {
    pub id: String,
    pub title: Option<String>,
    pub language: Option<String>,
    pub codec: Option<String>,
    pub selected: bool,
    pub external: bool,
    /// `external-filename` from MPV's track list: the URL an external track was
    /// added from. It is the only handle back to the add-on that supplied it.
    pub source_url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Chapter {
    pub index: usize,
    pub title: String,
    pub start: f64,
    pub end: f64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HdrMode {
    #[default]
    Auto,
    Passthrough,
    ToneMap,
    Disabled,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SpatialAudioMode {
    #[default]
    Disabled,
    OmniphonySpatial,
    BinauralHrtf,
    SurroundMatrix,
}

impl SpatialAudioMode {
    fn label(self) -> &'static str {
        match self {
            Self::Disabled => "Disabled",
            Self::OmniphonySpatial => "Omniphony 3D",
            Self::BinauralHrtf => "Binaural HRTF",
            Self::SurroundMatrix => "7.1 Virtual Surround",
        }
    }
}

/// Listener-facing controls supported by Omniphony's embedded renderer.
///
/// Values are kept host-neutral here: the app owns persistence while the MPV
/// actor owns validation, renderer configuration, and live OSC dispatch.
#[derive(Clone, Debug, PartialEq)]
pub struct OmniphonyAudioSettings {
    pub headphones: bool,
    pub hrir_source: String,
    pub sofa_path: String,
    pub pinna_preset: String,
    pub pinna_d_scale_pct: u16,
    pub pinna_depth_pct: u16,
    pub prtf_frequency_scale_pct: u16,
    pub prtf_depth_pct: u16,
    pub unit_scale_m: f32,
    pub head_radius_m: f32,
    pub reflections_enabled: bool,
    pub reflection_level: f32,
    pub room_width_m: f32,
    pub room_depth_m: f32,
    pub room_height_m: f32,
    pub reverb_enabled: bool,
    pub reverb_level: f32,
    pub reverb_rt60_s: f32,
    pub reverb_predelay_ms: f32,
    pub air_absorption: bool,
    pub tracking_address: String,
    pub tracking_format: String,
    pub tracking_smoothing: f32,
    pub tracking_invert: bool,
    pub osc_rx_port: u16,
}

impl Default for OmniphonyAudioSettings {
    fn default() -> Self {
        Self {
            headphones: false,
            hrir_source: "saf".to_owned(),
            sofa_path: String::new(),
            pinna_preset: "pbnh".to_owned(),
            pinna_d_scale_pct: 100,
            pinna_depth_pct: 100,
            prtf_frequency_scale_pct: 100,
            prtf_depth_pct: 100,
            unit_scale_m: 1.0,
            head_radius_m: 0.0875,
            reflections_enabled: false,
            reflection_level: 0.5,
            room_width_m: 4.0,
            room_depth_m: 5.0,
            room_height_m: 2.7,
            reverb_enabled: false,
            reverb_level: 0.25,
            reverb_rt60_s: 0.35,
            reverb_predelay_ms: 20.0,
            air_absorption: true,
            tracking_address: String::new(),
            tracking_format: "auto".to_owned(),
            tracking_smoothing: 0.2,
            tracking_invert: false,
            osc_rx_port: 9_000,
        }
    }
}

impl OmniphonyAudioSettings {
    pub fn sanitized(mut self) -> Self {
        self.hrir_source = match self.hrir_source.trim().to_ascii_lowercase().as_str() {
            "synthetic" => "synthetic",
            "pinna" => "pinna",
            "prtf" => "prtf",
            "sofa" => "sofa",
            _ => "saf",
        }
        .to_owned();
        self.pinna_preset = if self.pinna_preset.eq_ignore_ascii_case("rd") {
            "rd"
        } else {
            "pbnh"
        }
        .to_owned();
        self.pinna_d_scale_pct = self.pinna_d_scale_pct.clamp(50, 150);
        self.pinna_depth_pct = self.pinna_depth_pct.min(100);
        self.prtf_frequency_scale_pct = self.prtf_frequency_scale_pct.clamp(50, 150);
        self.prtf_depth_pct = self.prtf_depth_pct.min(100);
        self.unit_scale_m = finite_clamp(self.unit_scale_m, 1.0, 0.1, 10.0);
        self.head_radius_m = finite_clamp(self.head_radius_m, 0.0875, 0.05, 0.15);
        self.reflection_level = finite_clamp(self.reflection_level, 0.5, 0.0, 1.0);
        self.room_width_m = finite_clamp(self.room_width_m, 4.0, 1.0, 20.0);
        self.room_depth_m = finite_clamp(self.room_depth_m, 5.0, 1.0, 20.0);
        self.room_height_m = finite_clamp(self.room_height_m, 2.7, 1.0, 20.0);
        self.reverb_level = finite_clamp(self.reverb_level, 0.25, 0.0, 1.0);
        self.reverb_rt60_s = finite_clamp(self.reverb_rt60_s, 0.35, 0.1, 3.0);
        self.reverb_predelay_ms = finite_clamp(self.reverb_predelay_ms, 20.0, 0.0, 100.0);
        self.tracking_format = match self.tracking_format.trim().to_ascii_lowercase().as_str() {
            "quat" => "quat",
            "rotvec" => "rotvec",
            "euler" => "euler",
            _ => "auto",
        }
        .to_owned();
        self.tracking_smoothing = finite_clamp(self.tracking_smoothing, 0.2, 0.0, 0.99);
        if self.osc_rx_port == 0 {
            self.osc_rx_port = 9_000;
        }
        self
    }

    fn output_mode(&self) -> &'static str {
        if self.headphones {
            "binaural"
        } else {
            "speaker"
        }
    }

    fn hrir_selector(&self) -> String {
        match self.hrir_source.as_str() {
            "pinna" => format!(
                "pinna:{}:{}:{}",
                self.pinna_preset, self.pinna_d_scale_pct, self.pinna_depth_pct
            ),
            "prtf" => format!(
                "prtf:{}:{}",
                self.prtf_frequency_scale_pct, self.prtf_depth_pct
            ),
            "sofa" if !self.sofa_path.trim().is_empty() => {
                format!("sofa:{}", self.sofa_path.trim())
            }
            other => other.to_owned(),
        }
    }
}

fn finite_clamp(value: f32, fallback: f32, minimum: f32, maximum: f32) -> f32 {
    if value.is_finite() {
        value.clamp(minimum, maximum)
    } else {
        fallback
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackState {
    pub loading: bool,
    pub loaded: bool,
    pub paused: bool,
    pub buffering: bool,
    pub seeking: bool,
    pub time: f64,
    pub duration: f64,
    pub buffered_until: f64,
    pub cache_buffering_percent: f64,
    pub volume: f64,
    pub muted: bool,
    pub speed: f64,
    pub audio_tracks: Arc<[AudioTrack]>,
    pub subtitle_tracks: Arc<[SubtitleTrack]>,
    pub active_audio_track: Option<String>,
    pub active_subtitle_track: Option<String>,
    pub active_secondary_subtitle_track: Option<String>,
    pub chapters: Arc<[Chapter]>,
    pub ab_loop_a: Option<f64>,
    pub ab_loop_b: Option<f64>,
    pub hdr_mode: HdrMode,
    pub hdr_content: bool,
    pub hdr_passthrough_available: bool,
    pub spatial_audio_requested: SpatialAudioMode,
    pub spatial_audio_applied: SpatialAudioMode,
    pub video_primaries: Option<String>,
    pub video_transfer: Option<String>,
    pub filename: Option<String>,
    pub file_size: Option<u64>,
    pub file_format: Option<String>,
    pub video_format: Option<String>,
    pub audio_format: Option<String>,
    pub hardware_decoder: Option<String>,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            loading: false,
            loaded: false,
            paused: true,
            buffering: false,
            seeking: false,
            time: 0.0,
            duration: 0.0,
            buffered_until: 0.0,
            cache_buffering_percent: 0.0,
            volume: 1.0,
            muted: false,
            speed: 1.0,
            audio_tracks: Arc::from([]),
            subtitle_tracks: Arc::from([]),
            active_audio_track: None,
            active_subtitle_track: None,
            active_secondary_subtitle_track: None,
            chapters: Arc::from([]),
            ab_loop_a: None,
            ab_loop_b: None,
            hdr_mode: HdrMode::Auto,
            hdr_content: false,
            hdr_passthrough_available: true,
            spatial_audio_requested: SpatialAudioMode::Disabled,
            spatial_audio_applied: SpatialAudioMode::Disabled,
            video_primaries: None,
            video_transfer: None,
            filename: None,
            file_size: None,
            file_format: None,
            video_format: None,
            audio_format: None,
            hardware_decoder: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndReason {
    Eof,
    Stopped,
    Quit,
    Error,
    Redirect,
    Unknown,
}

#[derive(Clone, Debug)]
pub enum PlaybackEvent {
    State(Arc<PlaybackState>),
    FileLoaded,
    Ended {
        reason: EndReason,
        error: Option<String>,
    },
    ClientMessage(Vec<String>),
    VideoShadersConfigured {
        request_id: u64,
    },
    VideoShadersRejected {
        request_id: u64,
        message: String,
    },
    FrameCaptured {
        request_id: u64,
        path: PathBuf,
    },
    FrameCaptureFailed {
        request_id: u64,
        path: PathBuf,
        message: String,
    },
    ChaptersUpdated(Arc<[Chapter]>),
    HdrStateChanged {
        requested: HdrMode,
        applied: HdrMode,
        content_hdr: bool,
        passthrough_available: bool,
    },
    SpatialAudioChanged {
        requested: SpatialAudioMode,
        applied: SpatialAudioMode,
        message: String,
    },
    Warning(String),
    Error(String),
    Shutdown,
}

#[derive(Clone, Debug)]
pub enum PlaybackCommand {
    Load {
        url: String,
        start_at: Option<f64>,
    },
    Stop,
    SetPaused(bool),
    TogglePaused,
    SeekAbsolute(f64),
    SeekRelative(f64),
    SetVolume(f64),
    SetMuted(bool),
    SetSpeed(f64),
    SetVideoScale(u8),
    SetAudioTrack(Option<String>),
    SetSubtitleTrack(Option<String>),
    SetSecondarySubtitleTrack(Option<String>),
    SetAudioLanguage(String),
    SetSubtitleLanguage(String),
    AddSubtitle {
        url: String,
        title: Option<String>,
        language: Option<String>,
    },
    SetSubtitleDelay(i64),
    SetSubtitleScale(f64),
    SetSubtitlePosition(f64),
    SetSecondarySubtitleScale(f64),
    SetSecondarySubtitlePosition(f64),
    SetAudioDelay(i64),
    SetAbLoop {
        a: Option<f64>,
        b: Option<f64>,
    },
    CaptureFrame {
        request_id: u64,
        path: PathBuf,
        include_subtitles: bool,
    },
    SetHdrMode(HdrMode),
    SetSpatialAudioMode(SpatialAudioMode),
    ConfigureOmniphonyAudio(OmniphonyAudioSettings),
    RecenterOmniphonyHead,
    ConfigureVideoShaders {
        request_id: u64,
        paths: Vec<String>,
    },
    ScriptMessage(Vec<String>),
    Shutdown,
}

#[derive(Clone, Debug)]
pub struct PlayerConfig {
    pub config_dir: Option<PathBuf>,
    pub hardware_decoding: bool,
    pub spatial_audio_sofa_path: Option<PathBuf>,
    pub omniphony_config_path: Option<PathBuf>,
    /// Explicit `yt-dlp` location for MPV's `ytdl_hook`. [`None`] leaves the
    /// hook to find the binary on `PATH` itself.
    pub ytdl_path: Option<PathBuf>,
}

/// Sites whose streams MPV can only play by way of `ytdl_hook`.
///
/// YouTube hands out one signed URL per track, so the hook stitches the chosen
/// video and audio back together as an EDL. Those URLs also reject the
/// open-ended range requests FFmpeg issues when nothing buffers ahead of it,
/// which is why the demuxer cache has to be forced on for them even though
/// local streams are better off without it.
fn needs_ytdl(url: &str) -> bool {
    const HOSTS: [&str; 4] = [
        "youtube.com/",
        "youtu.be/",
        "www.youtube.com/",
        "m.youtube.com/",
    ];
    let trimmed = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    HOSTS.iter().any(|host| trimmed.starts_with(host))
}

/// Per-file `loadfile` options, or [`None`] when the defaults already suffice.
fn load_file_options(url: &str, start_at: Option<f64>) -> Option<String> {
    let mut options = Vec::new();
    if needs_ytdl(url) {
        options.push("cache=yes".to_owned());
    }
    if let Some(start_at) = start_at.filter(|time| time.is_finite() && *time > 0.0) {
        options.push(format!("start={start_at:.3}"));
    }
    (!options.is_empty()).then(|| options.join(","))
}

/// Formats an explicit extractor path as an MPV `script-opts` entry.
///
/// The `%len%` prefix is MPV's escaping for key/value list entries; without it a
/// comma anywhere in the path would be read as the start of another option.
pub(crate) fn ytdl_script_opt(path: &std::path::Path) -> String {
    let path = path.to_string_lossy();
    format!("ytdl_hook-ytdl_path=%{}%{}", path.len(), path)
}

#[derive(Clone)]
pub struct PlaybackController {
    sender: SyncSender<PlaybackCommand>,
    wake: Arc<ActorWake>,
}

impl PlaybackController {
    pub fn send(&self, command: PlaybackCommand) -> Result<(), MpvError> {
        self.sender
            .try_send(command)
            .map(|()| self.wake.signal())
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => MpvError::CommandQueueFull,
                mpsc::TrySendError::Disconnected(_) => MpvError::CommandQueueClosed,
            })
    }

    fn shutdown(&self) {
        // Shutdown must not be dropped just because the bounded queue is
        // temporarily full. The actor continuously drains this queue.
        self.wake.signal();
        let _ = self.sender.send(PlaybackCommand::Shutdown);
        self.wake.signal();
    }
}

pub struct PlaybackRuntime {
    controller: PlaybackController,
    render_source: RenderSource,
    omniphony_decoder_available: bool,
    actor: Option<JoinHandle<()>>,
}

impl PlaybackRuntime {
    pub fn start(
        config: PlayerConfig,
        event_sink: impl Fn(PlaybackEvent) + Send + Sync + 'static,
    ) -> Result<Self, MpvError> {
        let PlayerConfig {
            config_dir,
            hardware_decoding,
            spatial_audio_sofa_path,
            omniphony_config_path,
            ytdl_path,
        } = config;
        let api = MpvApi::playback_runtime()?;
        let client = MpvClient::create(api)?;

        if let Some(config_dir) = config_dir {
            client.set_option("config-dir", &config_dir.to_string_lossy())?;
            client.set_option("config", "yes")?;
            client.set_option("load-scripts", "yes")?;
        }
        // `ytdl_hook` takes the extractor path as a script option; there is no
        // top-level MPV option for it. A failure here is not fatal, it just
        // leaves the hook searching `PATH`.
        if let Some(ytdl_path) = ytdl_path.as_deref()
            && let Err(error) = client.set_option("script-opts", &ytdl_script_opt(ytdl_path))
        {
            tracing::warn!(%error, path = %ytdl_path.display(), "could not point ytdl_hook at yt-dlp");
        }
        client.set_option("ytdl", "yes")?;
        client.set_option("ytdl-format", "bestvideo+bestaudio/best")?;
        client.set_option("terminal", "no")?;
        client.set_option("input-default-bindings", "no")?;
        client.set_option("input-vo-keyboard", "no")?;
        client.set_option("osc", "no")?;
        client.set_option("vo", "libmpv")?;
        client.set_option("idle", "yes")?;
        client.set_option("keep-open", "no")?;
        client.set_option("cache", "no")?;
        client.set_option("cache-pause", "yes")?;
        client.set_option("cache-pause-initial", "no")?;
        client.set_option("cache-pause-wait", "0.5")?;
        client.set_option("cache-secs", "60")?;
        client.set_option("demuxer-max-bytes", "300000000")?;
        client.set_option("vd-lavc-threads", "0")?;
        client.set_option("ad-lavc-threads", "0")?;
        client.set_option("audio-fallback-to-null", "yes")?;
        client.set_option("audio-client-name", "Stremio")?;
        client.set_option("title", "Stremio")?;
        // Slint supplies a desktop WGL/OpenGL context on Windows. Direct
        // D3D11 hardware surfaces require ANGLE in libmpv, so use copy-safe
        // decoding and keep decoder-to-texture direct rendering disabled.
        client.set_option("vd-lavc-dr", "no")?;
        client.set_option("hwdec", hardware_decoding_option(hardware_decoding))?;
        client.initialize()?;
        let (_, _, hdr_result) = apply_hdr_mode(&client, HdrMode::Auto);
        hdr_result?;
        let spatial_audio =
            SpatialAudioRuntime::detect(&client, spatial_audio_sofa_path, omniphony_config_path);
        let omniphony_decoder_available = spatial_audio.omniphony_available();
        observe_properties(&client)?;

        let (sender, receiver) = mpsc::sync_channel(128);
        let wake = Arc::new(ActorWake::default());
        client.set_wakeup_callback(
            Some(wakeup_actor),
            Arc::as_ptr(&wake).cast_mut().cast::<c_void>(),
        );
        let controller = PlaybackController {
            sender,
            wake: wake.clone(),
        };
        let render_source = RenderSource::new(client.clone());
        let sink = Arc::new(event_sink);
        wake.signal();
        let actor = thread::Builder::new()
            .name("mpv-player".to_owned())
            .spawn(move || actor_loop(client, receiver, sink, wake, spatial_audio))
            .map_err(|_| MpvError::ActorPanicked)?;

        Ok(Self {
            controller,
            render_source,
            omniphony_decoder_available,
            actor: Some(actor),
        })
    }

    pub fn controller(&self) -> PlaybackController {
        self.controller.clone()
    }

    pub fn render_source(&self) -> RenderSource {
        self.render_source.clone()
    }

    /// Whether a compatible Omniphony runtime is available to the active libmpv.
    pub fn omniphony_decoder_available(&self) -> bool {
        self.omniphony_decoder_available
    }

    pub fn shutdown(mut self) -> Result<(), MpvError> {
        self.controller.shutdown();
        if let Some(actor) = self.actor.take() {
            actor.join().map_err(|_| MpvError::ActorPanicked)?;
        }
        Ok(())
    }
}

fn hardware_decoding_option(enabled: bool) -> &'static str {
    if !enabled {
        return "no";
    }
    #[cfg(target_os = "windows")]
    {
        "d3d11va-copy,auto-copy"
    }
    #[cfg(not(target_os = "windows"))]
    {
        "auto-copy"
    }
}

#[derive(Debug)]
struct SpatialAudioRuntime {
    base_ad: String,
    base_af: String,
    base_audio_channels: String,
    sofa_path: Option<PathBuf>,
    omniphony_config_path: Option<PathBuf>,
    omniphony_settings: OmniphonyAudioSettings,
    omniphony_decoder: bool,
    omniphony_engine: Result<OrenderProbe, String>,
}

struct SpatialAudioResolution {
    applied: SpatialAudioMode,
    message: String,
}

impl SpatialAudioRuntime {
    fn detect(
        client: &MpvClient,
        sofa_path: Option<PathBuf>,
        omniphony_config_path: Option<PathBuf>,
    ) -> Self {
        let base_ad = client.get_string("ad").unwrap_or_default();
        let base_af = client.get_string("af").unwrap_or_default();
        let base_audio_channels = client
            .get_string("audio-channels")
            .unwrap_or_else(|_| "auto-safe".to_owned());
        // `orender` is a dynamically selected audio decoder, so it is not
        // guaranteed to appear in MPV's decoder-list before a file is loaded.
        // The decoder patch does, however, register this private option.
        let omniphony_decoder = client.get_node("option-info/ad-orender-library").is_ok();
        let omniphony_engine = probe_orender();
        let omniphony_available = omniphony_decoder && omniphony_engine.is_ok();

        match &omniphony_engine {
            Ok(engine) => tracing::info!(
                path = %engine.path.display(),
                abi_major = 0,
                abi_minor = engine.minor,
                build_id = engine.build_id.as_deref().unwrap_or("unknown"),
                spatial_query = engine.spatial_query,
                omniphony_decoder,
                omniphony_available,
                "detected Omniphony runtime"
            ),
            Err(error) => tracing::info!(
                omniphony_decoder,
                omniphony_available,
                %error,
                "Omniphony runtime is unavailable"
            ),
        }

        Self {
            base_ad,
            base_af,
            base_audio_channels,
            sofa_path,
            omniphony_config_path,
            omniphony_settings: OmniphonyAudioSettings::default(),
            omniphony_decoder,
            omniphony_engine,
        }
    }

    fn omniphony_available(&self) -> bool {
        self.omniphony_decoder && self.omniphony_engine.is_ok()
    }

    fn apply(
        &self,
        client: &MpvClient,
        requested: SpatialAudioMode,
        loaded: bool,
        active_audio_track: Option<&str>,
    ) -> Result<SpatialAudioResolution, MpvError> {
        let candidates: &[SpatialAudioMode] = match requested {
            SpatialAudioMode::Disabled => &[SpatialAudioMode::Disabled],
            SpatialAudioMode::OmniphonySpatial => &[
                SpatialAudioMode::OmniphonySpatial,
                SpatialAudioMode::BinauralHrtf,
                SpatialAudioMode::SurroundMatrix,
                SpatialAudioMode::Disabled,
            ],
            SpatialAudioMode::BinauralHrtf => &[
                SpatialAudioMode::BinauralHrtf,
                SpatialAudioMode::SurroundMatrix,
                SpatialAudioMode::Disabled,
            ],
            SpatialAudioMode::SurroundMatrix => {
                &[SpatialAudioMode::SurroundMatrix, SpatialAudioMode::Disabled]
            }
        };
        let mut failures = Vec::new();

        for &candidate in candidates {
            if let Some(reason) = self.unavailable_reason(candidate) {
                failures.push(format!("{}: {reason}", candidate.label()));
                continue;
            }
            let result = self
                .configure(client, candidate)
                .and_then(|()| {
                    if loaded {
                        reselect_audio_track(client, active_audio_track)
                    } else {
                        Ok(())
                    }
                })
                .map(|()| {
                    if candidate == SpatialAudioMode::OmniphonySpatial && loaded {
                        self.send_live_controls();
                    }
                });
            match result {
                Ok(()) => {
                    let message = if candidate == requested {
                        self.active_message(candidate)
                    } else {
                        format!(
                            "{} unavailable ({}); using {}",
                            requested.label(),
                            failures.join("; "),
                            candidate.label()
                        )
                    };
                    return Ok(SpatialAudioResolution {
                        applied: candidate,
                        message,
                    });
                }
                Err(error) if candidate != SpatialAudioMode::Disabled => {
                    failures.push(format!("{}: {error}", candidate.label()));
                }
                Err(error) => return Err(error),
            }
        }

        Err(MpvError::InvalidNode(
            "spatial-audio fallback resolution exhausted unexpectedly".to_owned(),
        ))
    }

    fn unavailable_reason(&self, mode: SpatialAudioMode) -> Option<String> {
        match mode {
            SpatialAudioMode::OmniphonySpatial if !self.omniphony_decoder => Some(
                "the active libmpv does not expose the Omniphony decoder option".to_owned(),
            ),
            SpatialAudioMode::OmniphonySpatial => self
                .omniphony_engine
                .as_ref()
                .err()
                .map(|error| format!("liborender probe failed: {error}")),
            SpatialAudioMode::BinauralHrtf => match self.sofa_path.as_deref() {
                Some(path) if path.is_file() => None,
                Some(path) => Some(format!("SOFA file not found at {}", path.display())),
                None => Some(
                    "no SOFA file is configured (set STREMIO_SOFA_PATH or install hrtf/default.sofa in the MPV config directory)"
                        .to_owned(),
                ),
            },
            _ => None,
        }
    }

    fn configure(&self, client: &MpvClient, mode: SpatialAudioMode) -> Result<(), MpvError> {
        self.restore_base(client)?;
        match mode {
            SpatialAudioMode::Disabled => Ok(()),
            SpatialAudioMode::OmniphonySpatial => {
                let engine = self.omniphony_engine.as_ref().map_err(|error| {
                    MpvError::InvalidNode(format!("liborender probe failed: {error}"))
                })?;
                client.set_string("ad-orender-library", engine.path.to_string_lossy().as_ref())?;
                if let Some(path) = self.omniphony_config_path.as_deref() {
                    client.set_string("ad-orender-config", path.to_string_lossy().as_ref())?;
                }
                client.set_flag("ad-orender-osc", true)?;
                client.set_string(
                    "ad-orender-osc-rx-port",
                    &self.omniphony_settings.osc_rx_port.to_string(),
                )?;
                client.set_string("ad", &prepend_decoder(&self.base_ad, "orender"))
            }
            SpatialAudioMode::BinauralHrtf => {
                let Some(path) = self.sofa_path.as_deref() else {
                    return Err(MpvError::InvalidNode(
                        "binaural HRTF requested without a SOFA path".to_owned(),
                    ));
                };
                let filter = format!(
                    "lavfi=[sofalizer=sofa='{}':type=freq:normalize=1]",
                    escape_lavfi_value(&path.to_string_lossy())
                );
                client
                    .set_string("audio-channels", "auto")
                    .and_then(|()| client.set_string("af", &append_filter(&self.base_af, &filter)))
            }
            SpatialAudioMode::SurroundMatrix => {
                client.set_string("audio-channels", "7.1").and_then(|()| {
                    client.set_string(
                        "af",
                        &append_filter(&self.base_af, "lavfi=[surround=chl_out=7.1:chl_in=stereo]"),
                    )
                })
            }
        }
    }

    fn restore_base(&self, client: &MpvClient) -> Result<(), MpvError> {
        client
            .set_string("ad", &self.base_ad)
            .and_then(|()| client.set_string("af", &self.base_af))
            .and_then(|()| client.set_string("audio-channels", &self.base_audio_channels))
    }

    fn active_message(&self, mode: SpatialAudioMode) -> String {
        match mode {
            SpatialAudioMode::Disabled => "Spatial audio disabled".to_owned(),
            SpatialAudioMode::OmniphonySpatial => self.omniphony_engine.as_ref().map_or_else(
                |_| "Omniphony 3D active".to_owned(),
                |engine| {
                    let output = if self.omniphony_settings.headphones {
                        "headphones"
                    } else {
                        "speakers"
                    };
                    format!(
                        "Omniphony 3D active for {output} (liborender ABI 0.{})",
                        engine.minor
                    )
                },
            ),
            SpatialAudioMode::BinauralHrtf => "Binaural HRTF active".to_owned(),
            SpatialAudioMode::SurroundMatrix => "7.1 virtual surround active".to_owned(),
        }
    }

    fn configure_omniphony(&mut self, settings: OmniphonyAudioSettings) -> Result<(), MpvError> {
        self.omniphony_settings = settings.sanitized();
        if let Some(path) = self.omniphony_config_path.as_deref() {
            write_omniphony_config(path, &self.omniphony_settings).map_err(|error| {
                MpvError::InvalidNode(format!(
                    "could not write Omniphony config {}: {error}",
                    path.display()
                ))
            })?;
        }
        Ok(())
    }

    fn send_live_controls(&self) {
        let target = ("127.0.0.1", self.omniphony_settings.osc_rx_port);
        let Ok(socket) = UdpSocket::bind(("127.0.0.1", 0)) else {
            tracing::warn!("could not bind local socket for Omniphony controls");
            return;
        };
        let settings = &self.omniphony_settings;
        let string_controls = [
            ("/omniphony/control/output_mode", settings.output_mode()),
            (
                "/omniphony/control/head/tracking/address",
                settings.tracking_address.as_str(),
            ),
            (
                "/omniphony/control/head/tracking/format",
                settings.tracking_format.as_str(),
            ),
        ];
        for (address, value) in string_controls {
            send_osc_packet(&socket, target, osc_string_packet(address, value));
        }
        let hrir_selector = settings.hrir_selector();
        send_osc_packet(
            &socket,
            target,
            osc_string_packet("/omniphony/control/binaural/hrir_source", &hrir_selector),
        );
        let float_controls = [
            (
                "/omniphony/control/binaural/unit_scale",
                settings.unit_scale_m,
            ),
            (
                "/omniphony/control/binaural/head_radius",
                settings.head_radius_m,
            ),
            (
                "/omniphony/control/binaural/reflections/level",
                settings.reflection_level,
            ),
            (
                "/omniphony/control/binaural/reflections/room_width",
                settings.room_width_m,
            ),
            (
                "/omniphony/control/binaural/reflections/room_depth",
                settings.room_depth_m,
            ),
            (
                "/omniphony/control/binaural/reflections/room_height",
                settings.room_height_m,
            ),
            (
                "/omniphony/control/binaural/reverb/level",
                settings.reverb_level,
            ),
            (
                "/omniphony/control/binaural/reverb/rt60",
                settings.reverb_rt60_s,
            ),
            (
                "/omniphony/control/binaural/reverb/predelay",
                settings.reverb_predelay_ms,
            ),
            (
                "/omniphony/control/head/tracking/smoothing",
                settings.tracking_smoothing,
            ),
        ];
        for (address, value) in float_controls {
            send_osc_packet(&socket, target, osc_float_packet(address, value));
        }
        let bool_controls = [
            (
                "/omniphony/control/binaural/reflections/enabled",
                settings.reflections_enabled,
            ),
            (
                "/omniphony/control/binaural/reverb/enabled",
                settings.reverb_enabled,
            ),
            (
                "/omniphony/control/binaural/air_absorption",
                settings.air_absorption,
            ),
            (
                "/omniphony/control/head/tracking/invert",
                settings.tracking_invert,
            ),
        ];
        for (address, value) in bool_controls {
            send_osc_packet(&socket, target, osc_int_packet(address, i32::from(value)));
        }
    }

    fn recenter_head(&self) -> Result<(), MpvError> {
        if !self.omniphony_available() {
            return Err(MpvError::InvalidNode(
                "Omniphony head tracking is unavailable".to_owned(),
            ));
        }
        let socket = UdpSocket::bind(("127.0.0.1", 0)).map_err(|error| {
            MpvError::InvalidNode(format!("could not bind Omniphony control socket: {error}"))
        })?;
        socket
            .send_to(
                &osc_empty_packet("/omniphony/control/head/recenter"),
                ("127.0.0.1", self.omniphony_settings.osc_rx_port),
            )
            .map_err(|error| {
                MpvError::InvalidNode(format!("could not recenter Omniphony head pose: {error}"))
            })?;
        Ok(())
    }
}

fn reselect_audio_track(
    client: &MpvClient,
    active_audio_track: Option<&str>,
) -> Result<(), MpvError> {
    let Some(active_audio_track) = active_audio_track.filter(|track| *track != "no") else {
        return Ok(());
    };
    client
        .set_string("aid", "no")
        .and_then(|()| client.set_string("aid", active_audio_track))
}

fn prepend_decoder(base: &str, decoder: &str) -> String {
    if base.split(',').any(|item| item.trim() == decoder) {
        base.to_owned()
    } else if base.trim().is_empty() {
        decoder.to_owned()
    } else {
        format!("{decoder},{base}")
    }
}

fn append_filter(base: &str, filter: &str) -> String {
    if base.trim().is_empty() {
        filter.to_owned()
    } else {
        format!("{base},{filter}")
    }
}

fn write_omniphony_config(path: &Path, settings: &OmniphonyAudioSettings) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let reference_quat = fs::read_to_string(path)
        .ok()
        .and_then(|yaml| preserved_reference_quat(&yaml));
    let hrir_selector = settings.hrir_selector();
    let mut yaml = format!(
        "render:\n  osc: true\n  osc_rx_port: {}\n  binaural:\n    output_mode: {}\n    unit_scale_m: {:.4}\n    head_radius_m: {:.4}\n    hrir_source: {}\n",
        settings.osc_rx_port,
        settings.output_mode(),
        settings.unit_scale_m,
        settings.head_radius_m,
        yaml_string(&hrir_selector),
    );
    if settings.hrir_source == "sofa" && !settings.sofa_path.trim().is_empty() {
        yaml.push_str(&format!(
            "    hrtf_sofa_path: {}\n",
            yaml_string(settings.sofa_path.trim())
        ));
    }
    yaml.push_str("    head_tracking:\n");
    yaml.push_str(&format!(
        "      osc_address: {}\n      format: {}\n",
        yaml_string(settings.tracking_address.trim()),
        settings.tracking_format,
    ));
    if let Some(reference) = reference_quat {
        yaml.push_str(&format!("      reference_quat: {reference}\n"));
    }
    yaml.push_str(&format!(
        concat!(
            "    reflections:\n",
            "      enabled: {}\n",
            "      room_width_m: {:.3}\n",
            "      room_depth_m: {:.3}\n",
            "      room_height_m: {:.3}\n",
            "      level: {:.3}\n",
            "    reverb:\n",
            "      enabled: {}\n",
            "      level: {:.3}\n",
            "      rt60_s: {:.3}\n",
            "      predelay_ms: {:.3}\n",
            "    air_absorption: {}\n"
        ),
        settings.reflections_enabled,
        settings.room_width_m,
        settings.room_depth_m,
        settings.room_height_m,
        settings.reflection_level,
        settings.reverb_enabled,
        settings.reverb_level,
        settings.reverb_rt60_s,
        settings.reverb_predelay_ms,
        settings.air_absorption,
    ));
    fs::write(path, yaml)
}

fn preserved_reference_quat(yaml: &str) -> Option<String> {
    yaml.lines().find_map(|line| {
        let value = line.trim().strip_prefix("reference_quat:")?.trim();
        (!value.is_empty()
            && value.chars().all(|character| {
                character.is_ascii_digit()
                    || matches!(
                        character,
                        '[' | ']' | ',' | '.' | '-' | '+' | 'e' | 'E' | ' ' | '\t'
                    )
            }))
        .then(|| value.to_owned())
    })
}

fn yaml_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("\"{escaped}\"")
}

fn osc_padded_string(buffer: &mut Vec<u8>, value: &str) {
    buffer.extend_from_slice(value.as_bytes());
    buffer.push(0);
    while !buffer.len().is_multiple_of(4) {
        buffer.push(0);
    }
}

fn osc_empty_packet(address: &str) -> Vec<u8> {
    let mut packet = Vec::with_capacity(address.len() + 8);
    osc_padded_string(&mut packet, address);
    osc_padded_string(&mut packet, ",");
    packet
}

fn osc_string_packet(address: &str, value: &str) -> Vec<u8> {
    let mut packet = Vec::with_capacity(address.len() + value.len() + 12);
    osc_padded_string(&mut packet, address);
    osc_padded_string(&mut packet, ",s");
    osc_padded_string(&mut packet, value);
    packet
}

fn osc_float_packet(address: &str, value: f32) -> Vec<u8> {
    let mut packet = Vec::with_capacity(address.len() + 12);
    osc_padded_string(&mut packet, address);
    osc_padded_string(&mut packet, ",f");
    packet.extend_from_slice(&value.to_bits().to_be_bytes());
    packet
}

fn osc_int_packet(address: &str, value: i32) -> Vec<u8> {
    let mut packet = Vec::with_capacity(address.len() + 12);
    osc_padded_string(&mut packet, address);
    osc_padded_string(&mut packet, ",i");
    packet.extend_from_slice(&value.to_be_bytes());
    packet
}

fn send_osc_packet(socket: &UdpSocket, target: (&str, u16), packet: Vec<u8>) {
    if let Err(error) = socket.send_to(&packet, target) {
        tracing::debug!(%error, "could not send an Omniphony live control");
    }
}

fn escape_lavfi_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace(':', "\\:")
        .replace('\'', "\\'")
}

impl Drop for PlaybackRuntime {
    fn drop(&mut self) {
        if let Some(actor) = self.actor.take() {
            self.controller.shutdown();
            let _ = actor.join();
        }
    }
}

fn observe_properties(client: &MpvClient) -> Result<(), MpvError> {
    let properties = [
        (1, "pause", FORMAT_FLAG),
        (2, "time-pos", FORMAT_DOUBLE),
        (3, "duration", FORMAT_DOUBLE),
        (4, "demuxer-cache-time", FORMAT_DOUBLE),
        (5, "paused-for-cache", FORMAT_FLAG),
        (6, "seeking", FORMAT_FLAG),
        (7, "volume", FORMAT_DOUBLE),
        (8, "mute", FORMAT_FLAG),
        (9, "speed", FORMAT_DOUBLE),
        (10, "aid", FORMAT_STRING),
        (11, "sid", FORMAT_STRING),
        (12, "track-list", FORMAT_NODE),
        (13, "filename", FORMAT_STRING),
        (14, "file-size", FORMAT_INT64),
        (15, "file-format", FORMAT_STRING),
        (16, "video-format", FORMAT_STRING),
        (17, "audio-codec-name", FORMAT_STRING),
        (18, "hwdec-current", FORMAT_STRING),
        (19, "cache-buffering-state", FORMAT_INT64),
        (20, "secondary-sid", FORMAT_STRING),
        (21, "chapter-list", FORMAT_NODE),
        (22, "video-params/primaries", FORMAT_STRING),
        (23, "video-params/gamma", FORMAT_STRING),
    ];
    for (id, name, format) in properties {
        client.observe(id, name, format)?;
    }
    Ok(())
}

fn actor_loop(
    client: Arc<MpvClient>,
    receiver: Receiver<PlaybackCommand>,
    sink: Arc<dyn Fn(PlaybackEvent) + Send + Sync>,
    wake: Arc<ActorWake>,
    mut spatial_audio: SpatialAudioRuntime,
) {
    let mut state = PlaybackState::default();
    let mut running = true;

    while running {
        wake.wait(Duration::from_secs(1));
        loop {
            match receiver.try_recv() {
                Ok(command) => {
                    running =
                        handle_command(&client, command, &mut state, &sink, &mut spatial_audio);
                    if !running {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    running = false;
                    break;
                }
            }
        }
        if !running {
            break;
        }

        drain_events(&client, &mut state, &sink, &mut running, &spatial_audio);
    }

    client.set_wakeup_callback(None, std::ptr::null_mut());
    client.abort_async_command(ADD_SUBTITLE_COMMAND_REPLY_ID);
    let _ = client.command(&["stop"]);
    sink(PlaybackEvent::Shutdown);
}

#[derive(Default)]
struct ActorWake {
    pending: Mutex<bool>,
    condvar: Condvar,
}

impl ActorWake {
    fn signal(&self) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *pending = true;
        self.condvar.notify_one();
    }

    fn wait(&self, timeout: Duration) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !*pending {
            let (guard, _) = self
                .condvar
                .wait_timeout(pending, timeout)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            pending = guard;
        }
        *pending = false;
    }
}

unsafe extern "C" fn wakeup_actor(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: The pointer comes from Arc::as_ptr and remains alive until the
    // callback is unregistered at actor shutdown.
    let wake = unsafe { &*context.cast::<ActorWake>() };
    wake.signal();
}

fn handle_command(
    client: &MpvClient,
    command: PlaybackCommand,
    state: &mut PlaybackState,
    sink: &Arc<dyn Fn(PlaybackEvent) + Send + Sync>,
    spatial_audio: &mut SpatialAudioRuntime,
) -> bool {
    let fatal_error = matches!(&command, PlaybackCommand::Load { .. });
    let result = match command {
        PlaybackCommand::Load { url, start_at } => {
            let hdr_mode = state.hdr_mode;
            let hdr_passthrough_available = state.hdr_passthrough_available;
            let spatial_audio_requested = state.spatial_audio_requested;
            let spatial_audio_applied = state.spatial_audio_applied;
            *state = PlaybackState {
                loading: true,
                paused: false,
                hdr_mode,
                hdr_passthrough_available,
                spatial_audio_requested,
                spatial_audio_applied,
                ..PlaybackState::default()
            };
            sink(PlaybackEvent::State(Arc::new(state.clone())));
            let file_options = load_file_options(&url, start_at);
            set_optional_double(client, "ab-loop-a", None)
                .and_then(|()| set_optional_double(client, "ab-loop-b", None))
                .and_then(|()| match file_options {
                    Some(options) => client.command(&["loadfile", &url, "replace", "-1", &options]),
                    None => client.command(&["loadfile", &url, "replace"]),
                })
        }
        PlaybackCommand::Stop => {
            client.abort_async_command(ADD_SUBTITLE_COMMAND_REPLY_ID);
            client.command(&["stop"])
        }
        PlaybackCommand::SetPaused(paused) => client.set_flag("pause", paused),
        PlaybackCommand::TogglePaused => client.command(&["cycle", "pause"]),
        PlaybackCommand::SeekAbsolute(time) => {
            client.command(&["seek", &time.max(0.0).to_string(), "absolute+exact"])
        }
        PlaybackCommand::SeekRelative(seconds) => {
            client.command(&["seek", &seconds.to_string(), "relative+exact"])
        }
        PlaybackCommand::SetVolume(volume) => {
            client.set_double("volume", volume.clamp(0.0, 2.0) * 100.0)
        }
        PlaybackCommand::SetMuted(muted) => client.set_flag("mute", muted),
        PlaybackCommand::SetSpeed(speed) => client.set_double("speed", speed.clamp(0.25, 4.0)),
        PlaybackCommand::SetVideoScale(mode) => match mode % 3 {
            // contain: preserve the source aspect and letterbox inside the FBO
            0 => client
                .set_flag("keepaspect", true)
                .and_then(|()| client.set_double("panscan", 0.0)),
            // cover: preserve the source aspect and crop until the FBO is full
            1 => client
                .set_flag("keepaspect", true)
                .and_then(|()| client.set_double("panscan", 1.0)),
            // fill: match the FBO exactly (the web player's third scale mode)
            _ => client.set_flag("keepaspect", false),
        },
        PlaybackCommand::SetAudioTrack(track) => {
            client.set_string("aid", track.as_deref().unwrap_or("no"))
        }
        PlaybackCommand::SetSubtitleTrack(track) => {
            if track.as_ref() == state.active_secondary_subtitle_track.as_ref() {
                client
                    .set_string("secondary-sid", "no")
                    .and_then(|()| client.set_string("sid", track.as_deref().unwrap_or("no")))
            } else {
                client.set_string("sid", track.as_deref().unwrap_or("no"))
            }
        }
        PlaybackCommand::SetSecondarySubtitleTrack(track) => {
            if track.as_ref() == state.active_subtitle_track.as_ref() && track.is_some() {
                sink(PlaybackEvent::Warning(
                    "The primary and secondary subtitle tracks must be different".to_owned(),
                ));
                Ok(())
            } else {
                client.set_string("secondary-sid", track.as_deref().unwrap_or("no"))
            }
        }
        PlaybackCommand::SetAudioLanguage(language) => client.set_string("alang", &language),
        PlaybackCommand::SetSubtitleLanguage(language) => client.set_string("slang", &language),
        PlaybackCommand::AddSubtitle {
            url,
            title,
            language,
        } => {
            let title = title.unwrap_or_default();
            let language = language.unwrap_or_default();
            client.command_async(
                ADD_SUBTITLE_COMMAND_REPLY_ID,
                &["sub-add", &url, "auto", &title, &language],
            )
        }
        PlaybackCommand::SetSubtitleDelay(milliseconds) => {
            client.set_double("sub-delay", milliseconds as f64 / 1_000.0)
        }
        PlaybackCommand::SetSubtitleScale(scale) => {
            client.set_double("sub-scale", scale.clamp(0.25, 4.0))
        }
        PlaybackCommand::SetSubtitlePosition(position) => {
            client.set_double("sub-pos", position.clamp(0.0, 100.0))
        }
        PlaybackCommand::SetSecondarySubtitleScale(scale) => {
            client.set_double("secondary-sub-scale", scale.clamp(0.25, 4.0))
        }
        PlaybackCommand::SetSecondarySubtitlePosition(position) => {
            client.set_double("secondary-sub-pos", position.clamp(0.0, 100.0))
        }
        PlaybackCommand::SetAudioDelay(milliseconds) => {
            client.set_double("audio-delay", milliseconds as f64 / 1_000.0)
        }
        PlaybackCommand::SetAbLoop { a, b } => {
            if let Err(message) = validate_ab_loop(a, b) {
                sink(PlaybackEvent::Warning(message.to_owned()));
                Ok(())
            } else {
                set_optional_double(client, "ab-loop-a", a).and_then(|()| {
                    set_optional_double(client, "ab-loop-b", b).map(|()| {
                        state.ab_loop_a = a;
                        state.ab_loop_b = b;
                        sink(PlaybackEvent::State(Arc::new(state.clone())));
                    })
                })
            }
        }
        PlaybackCommand::CaptureFrame {
            request_id,
            path,
            include_subtitles,
        } => {
            let path_string = path.to_string_lossy();
            let flags = if include_subtitles {
                "subtitles"
            } else {
                "video"
            };
            match client.command(&["screenshot-to-file", &path_string, flags]) {
                Ok(()) => sink(PlaybackEvent::FrameCaptured { request_id, path }),
                Err(error) => sink(PlaybackEvent::FrameCaptureFailed {
                    request_id,
                    path,
                    message: error.to_string(),
                }),
            }
            Ok(())
        }
        PlaybackCommand::SetHdrMode(requested) => {
            let (applied, passthrough_available, result) = apply_hdr_mode(client, requested);
            state.hdr_mode = applied;
            state.hdr_passthrough_available = passthrough_available;
            sink(PlaybackEvent::HdrStateChanged {
                requested,
                applied,
                content_hdr: state.hdr_content,
                passthrough_available,
            });
            sink(PlaybackEvent::State(Arc::new(state.clone())));
            result
        }
        PlaybackCommand::SetSpatialAudioMode(requested) => {
            match spatial_audio.apply(
                client,
                requested,
                state.loaded,
                state.active_audio_track.as_deref(),
            ) {
                Ok(resolution) => {
                    state.spatial_audio_requested = requested;
                    state.spatial_audio_applied = resolution.applied;
                    sink(PlaybackEvent::SpatialAudioChanged {
                        requested,
                        applied: resolution.applied,
                        message: resolution.message,
                    });
                    sink(PlaybackEvent::State(Arc::new(state.clone())));
                    Ok(())
                }
                Err(error) => Err(error),
            }
        }
        PlaybackCommand::ConfigureOmniphonyAudio(settings) => {
            match spatial_audio.configure_omniphony(settings) {
                Err(error) => Err(error),
                Ok(()) if state.spatial_audio_requested == SpatialAudioMode::OmniphonySpatial => {
                    match spatial_audio.apply(
                        client,
                        SpatialAudioMode::OmniphonySpatial,
                        state.loaded,
                        state.active_audio_track.as_deref(),
                    ) {
                        Ok(resolution) => {
                            state.spatial_audio_applied = resolution.applied;
                            sink(PlaybackEvent::SpatialAudioChanged {
                                requested: SpatialAudioMode::OmniphonySpatial,
                                applied: resolution.applied,
                                message: resolution.message,
                            });
                            sink(PlaybackEvent::State(Arc::new(state.clone())));
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                }
                Ok(()) => Ok(()),
            }
        }
        PlaybackCommand::RecenterOmniphonyHead => spatial_audio.recenter_head(),
        PlaybackCommand::ConfigureVideoShaders { request_id, paths } => {
            match client.set_string_list("glsl-shaders", &paths) {
                Ok(()) => sink(PlaybackEvent::VideoShadersConfigured { request_id }),
                Err(error) => {
                    let clear_result = client.set_string_list("glsl-shaders", &[]);
                    let message = match clear_result {
                        Ok(()) => error.to_string(),
                        Err(clear_error) => format!(
                            "{error}; clearing rejected video shaders also failed: {clear_error}"
                        ),
                    };
                    sink(PlaybackEvent::VideoShadersRejected {
                        request_id,
                        message,
                    });
                }
            }
            Ok(())
        }
        PlaybackCommand::ScriptMessage(ref args) => {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            client.command(&refs)
        }
        PlaybackCommand::Shutdown => return false,
    };
    if let Err(error) = result {
        sink(if fatal_error {
            PlaybackEvent::Error(error.to_string())
        } else {
            PlaybackEvent::Warning(error.to_string())
        });
    }
    true
}

fn validate_ab_loop(a: Option<f64>, b: Option<f64>) -> Result<(), &'static str> {
    if a.is_some_and(|value| !value.is_finite() || value < 0.0)
        || b.is_some_and(|value| !value.is_finite() || value < 0.0)
    {
        return Err("A/B loop points must be finite, non-negative timestamps");
    }
    if let (Some(a), Some(b)) = (a, b)
        && b - a < 0.25
    {
        return Err("Loop point B must be at least 250 ms after point A");
    }
    Ok(())
}

fn set_optional_double(
    client: &MpvClient,
    property: &str,
    value: Option<f64>,
) -> Result<(), MpvError> {
    match value {
        Some(value) => client.set_double(property, value),
        None => client.set_string(property, "no"),
    }
}

fn apply_hdr_mode(client: &MpvClient, requested: HdrMode) -> (HdrMode, bool, Result<(), MpvError>) {
    let result = configure_hdr_mode(client, requested);
    if result.is_ok() {
        return (requested, true, result);
    }
    if matches!(requested, HdrMode::Auto | HdrMode::Passthrough) {
        return (
            HdrMode::ToneMap,
            false,
            configure_hdr_mode(client, HdrMode::ToneMap),
        );
    }
    (requested, false, result)
}

fn configure_hdr_mode(client: &MpvClient, mode: HdrMode) -> Result<(), MpvError> {
    let (target_hint, tone_mapping, compute_peak) = hdr_properties(mode);
    client
        .set_flag("target-colorspace-hint", target_hint)
        .and_then(|()| client.set_string("tone-mapping", tone_mapping))
        .and_then(|()| client.set_flag("hdr-compute-peak", compute_peak))
}

fn hdr_properties(mode: HdrMode) -> (bool, &'static str, bool) {
    match mode {
        HdrMode::Auto | HdrMode::Passthrough => (true, "auto", true),
        HdrMode::ToneMap => (false, "auto", true),
        HdrMode::Disabled => (false, "clip", false),
    }
}

fn drain_events(
    client: &MpvClient,
    state: &mut PlaybackState,
    sink: &Arc<dyn Fn(PlaybackEvent) + Send + Sync>,
    running: &mut bool,
    spatial_audio: &SpatialAudioRuntime,
) {
    loop {
        let event = client.wait_event(0.0);
        if event.is_null() {
            return;
        }
        // SAFETY: MPV guarantees the event pointer until the next wait call.
        let event = unsafe { &*event };
        match event.event_id {
            EVENT_NONE => return,
            EVENT_COMMAND_REPLY if event.error < 0 => {
                let error = client.api.operation_error(event.error).to_string();
                sink(PlaybackEvent::Warning(error));
            }
            EVENT_COMMAND_REPLY => {}
            EVENT_START_FILE => {
                state.loading = true;
                state.loaded = false;
                state.cache_buffering_percent = 0.0;
                sink(PlaybackEvent::State(Arc::new(state.clone())));
            }
            EVENT_FILE_LOADED => {
                state.loading = false;
                state.loaded = true;
                if state.spatial_audio_applied == SpatialAudioMode::OmniphonySpatial {
                    spatial_audio.send_live_controls();
                }
                sink(PlaybackEvent::FileLoaded);
                sink(PlaybackEvent::State(Arc::new(state.clone())));
            }
            EVENT_PLAYBACK_RESTART => {
                state.buffering = false;
                sink(PlaybackEvent::State(Arc::new(state.clone())));
            }
            EVENT_PROPERTY_CHANGE => {
                let update = update_property(event, state);
                match update {
                    PropertyUpdate::Chapters => {
                        sink(PlaybackEvent::ChaptersUpdated(state.chapters.clone()));
                    }
                    PropertyUpdate::Hdr => sink(PlaybackEvent::HdrStateChanged {
                        requested: state.hdr_mode,
                        applied: state.hdr_mode,
                        content_hdr: state.hdr_content,
                        passthrough_available: state.hdr_passthrough_available,
                    }),
                    PropertyUpdate::StateOnly => {}
                }
                sink(PlaybackEvent::State(Arc::new(state.clone())));
            }
            EVENT_CLIENT_MESSAGE if !event.data.is_null() => {
                // SAFETY: mpv guarantees the data pointer is a valid
                // MpvEventClientMessage for CLIENT_MESSAGE events.
                let msg = unsafe { &*(event.data as *const MpvEventClientMessage) };
                let mut args = Vec::with_capacity(msg.num_args as usize);
                for i in 0..msg.num_args as isize {
                    // SAFETY: args array has num_args valid C string pointers.
                    let ptr = unsafe { *msg.args.offset(i) };
                    if !ptr.is_null() {
                        let s = unsafe { CStr::from_ptr(ptr) }
                            .to_string_lossy()
                            .into_owned();
                        args.push(s);
                    }
                }
                sink(PlaybackEvent::ClientMessage(args));
            }
            EVENT_END_FILE => handle_end_file(client, event, state, sink),
            EVENT_QUEUE_OVERFLOW => sink(PlaybackEvent::Warning(
                "MPV event queue overflowed; playback state may be stale".to_owned(),
            )),
            EVENT_SHUTDOWN => {
                *running = false;
                return;
            }
            _ => {}
        }
    }
}

fn handle_end_file(
    client: &MpvClient,
    event: &MpvEvent,
    state: &mut PlaybackState,
    sink: &Arc<dyn Fn(PlaybackEvent) + Send + Sync>,
) {
    if event.data.is_null() {
        return;
    }
    // SAFETY: EVENT_END_FILE data is mpv_event_end_file for this event lifetime.
    let data = unsafe { &*(event.data as *const MpvEventEndFile) };
    let reason = match data.reason {
        END_FILE_EOF => EndReason::Eof,
        END_FILE_STOP => EndReason::Stopped,
        END_FILE_QUIT => EndReason::Quit,
        END_FILE_ERROR => EndReason::Error,
        END_FILE_REDIRECT => EndReason::Redirect,
        _ => EndReason::Unknown,
    };
    let error =
        (data.reason == END_FILE_ERROR).then(|| client.api.operation_error(data.error).to_string());
    state.loading = false;
    state.loaded = false;
    sink(PlaybackEvent::State(Arc::new(state.clone())));
    sink(PlaybackEvent::Ended { reason, error });
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PropertyUpdate {
    StateOnly,
    Chapters,
    Hdr,
}

fn update_property(event: &MpvEvent, state: &mut PlaybackState) -> PropertyUpdate {
    if event.data.is_null() {
        return PropertyUpdate::StateOnly;
    }
    // SAFETY: PROPERTY_CHANGE data has this layout for the event lifetime.
    let property = unsafe { &*(event.data as *const MpvEventProperty) };
    if property.name.is_null() || property.format == FORMAT_NONE {
        return PropertyUpdate::StateOnly;
    }
    // SAFETY: MPV property names are null-terminated strings.
    let name = unsafe { CStr::from_ptr(property.name) }.to_string_lossy();
    match name.as_ref() {
        "pause" => state.paused = property_flag(property).unwrap_or(state.paused),
        "time-pos" => state.time = property_double(property).unwrap_or(state.time).max(0.0),
        "duration" => {
            state.duration = property_double(property).unwrap_or(state.duration).max(0.0);
            if !state.chapters.is_empty() {
                let mut chapters = state.chapters.to_vec();
                normalize_chapter_ends(&mut chapters, state.duration);
                state.chapters = chapters.into();
                return PropertyUpdate::Chapters;
            }
        }
        "demuxer-cache-time" => {
            state.buffered_until = property_double(property)
                .unwrap_or(state.buffered_until)
                .max(0.0)
        }
        "paused-for-cache" => state.buffering = property_flag(property).unwrap_or(state.buffering),
        "seeking" => state.seeking = property_flag(property).unwrap_or(state.seeking),
        "volume" => {
            state.volume =
                (property_double(property).unwrap_or(state.volume * 100.0) / 100.0).clamp(0.0, 2.0)
        }
        "mute" => state.muted = property_flag(property).unwrap_or(state.muted),
        "speed" => state.speed = property_double(property).unwrap_or(state.speed),
        "aid" => state.active_audio_track = property_track_id(property),
        "sid" => state.active_subtitle_track = property_track_id(property),
        "secondary-sid" => state.active_secondary_subtitle_track = property_track_id(property),
        "filename" => state.filename = property_string(property),
        "file-size" => {
            state.file_size = property_int64(property).and_then(|size| u64::try_from(size).ok())
        }
        "file-format" => state.file_format = property_string(property),
        "video-format" => state.video_format = property_string(property),
        "audio-codec-name" => state.audio_format = property_string(property),
        "hwdec-current" => state.hardware_decoder = property_string(property),
        "cache-buffering-state" => {
            state.cache_buffering_percent = property_int64(property)
                .map(|percent| percent as f64)
                .unwrap_or(state.cache_buffering_percent)
                .clamp(0.0, 100.0)
        }
        "chapter-list" => {
            if let Some(node) = property_node(property) {
                let mut chapters = parse_chapters(node);
                normalize_chapter_ends(&mut chapters, state.duration);
                state.chapters = chapters.into();
                return PropertyUpdate::Chapters;
            }
        }
        "video-params/primaries" => {
            state.video_primaries = property_string(property);
            update_hdr_content(state);
            return PropertyUpdate::Hdr;
        }
        "video-params/gamma" => {
            state.video_transfer = property_string(property);
            update_hdr_content(state);
            return PropertyUpdate::Hdr;
        }
        "track-list" => {
            if let Some(node) = property_node(property) {
                let (audio, subtitles) = parse_tracks(node);
                state.audio_tracks = audio.into();
                state.subtitle_tracks = subtitles.into();
            }
        }
        _ => {}
    }
    PropertyUpdate::StateOnly
}

fn property_track_id(property: &MpvEventProperty) -> Option<String> {
    property_string(property).filter(|value| value != "no" && value != "auto")
}

fn update_hdr_content(state: &mut PlaybackState) {
    let wide_primaries = state.video_primaries.as_deref().is_some_and(|value| {
        value.eq_ignore_ascii_case("bt.2020") || value.eq_ignore_ascii_case("bt.2020-ncl")
    });
    let hdr_transfer = state.video_transfer.as_deref().is_some_and(|value| {
        value.eq_ignore_ascii_case("pq")
            || value.eq_ignore_ascii_case("smpte2084")
            || value.eq_ignore_ascii_case("hlg")
            || value.eq_ignore_ascii_case("arib-std-b67")
    });
    state.hdr_content = wide_primaries && hdr_transfer;
}

fn property_flag(property: &MpvEventProperty) -> Option<bool> {
    if property.format != FORMAT_FLAG || property.data.is_null() {
        return None;
    }
    // SAFETY: FORMAT_FLAG data points to a C int for this event lifetime.
    Some(unsafe { *(property.data as *const c_int) } != 0)
}

fn property_double(property: &MpvEventProperty) -> Option<f64> {
    if property.format != FORMAT_DOUBLE || property.data.is_null() {
        return None;
    }
    // SAFETY: FORMAT_DOUBLE data points to a double for this event lifetime.
    Some(unsafe { *(property.data as *const f64) })
}

fn property_int64(property: &MpvEventProperty) -> Option<i64> {
    if property.format != FORMAT_INT64 || property.data.is_null() {
        return None;
    }
    // SAFETY: FORMAT_INT64 data points to int64_t for this event lifetime.
    Some(unsafe { *(property.data as *const i64) })
}

fn property_string(property: &MpvEventProperty) -> Option<String> {
    if property.format != FORMAT_STRING || property.data.is_null() {
        return None;
    }
    // SAFETY: FORMAT_STRING event data points to a char pointer.
    let value = unsafe { *(property.data as *const *const c_char) };
    if value.is_null() {
        None
    } else {
        // SAFETY: MPV provides a null-terminated string for the event lifetime.
        Some(
            unsafe { CStr::from_ptr(value) }
                .to_string_lossy()
                .into_owned(),
        )
    }
}

fn property_node(property: &MpvEventProperty) -> Option<&MpvNode> {
    if property.format != FORMAT_NODE || property.data.is_null() {
        None
    } else {
        // SAFETY: FORMAT_NODE data points to an mpv_node for the event lifetime.
        Some(unsafe { &*(property.data as *const MpvNode) })
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::{PlaybackRuntime, PlayerConfig, hardware_decoding_option};

    #[test]
    fn hardware_decoding_should_use_copy_safe_windows_backends() {
        assert_eq!(hardware_decoding_option(true), "d3d11va-copy,auto-copy");
    }

    #[test]
    fn playback_runtime_should_start_with_dynamic_engine() {
        let runtime = PlaybackRuntime::start(
            PlayerConfig {
                config_dir: None,
                hardware_decoding: false,
                spatial_audio_sofa_path: None,
                omniphony_config_path: None,
                ytdl_path: None,
            },
            |_| {},
        )
        .expect("the dynamically linked MPV runtime should start");

        runtime
            .shutdown()
            .expect("the MPV actor should shut down cleanly");
    }
}

fn parse_tracks(node: &MpvNode) -> (Vec<AudioTrack>, Vec<SubtitleTrack>) {
    let Some(entries) = node_list(node, FORMAT_NODE_ARRAY) else {
        return (Vec::new(), Vec::new());
    };
    let mut audio = Vec::new();
    let mut subtitles = Vec::new();
    for entry in entries {
        let Some(map) = node_map(entry) else {
            continue;
        };
        let Some(values) = (!map.values.is_null()).then_some(map.values) else {
            continue;
        };
        let Some(keys) = (!map.keys.is_null()).then_some(map.keys) else {
            continue;
        };
        let Some(len) = usize::try_from(map.num).ok() else {
            continue;
        };

        let mut kind = None;
        let mut id = None;
        let mut title = None;
        let mut language = None;
        let mut codec = None;
        let mut selected = false;
        let mut external = false;
        let mut source_url = None;
        for index in 0..len {
            // SAFETY: MPV guarantees both map arrays contain `num` entries.
            let key = unsafe { *keys.add(index) };
            if key.is_null() {
                continue;
            }
            // SAFETY: MPV map keys are null-terminated and values has the same length.
            let key = unsafe { CStr::from_ptr(key) }.to_bytes();
            let value = unsafe { &*values.add(index) };
            match key {
                b"type" => {
                    kind = node_string_bytes(value).and_then(|value| match value {
                        b"audio" => Some(TrackKind::Audio),
                        b"sub" => Some(TrackKind::Subtitle),
                        _ => None,
                    });
                }
                b"id" => id = node_int(value).map(|id| id.to_string()),
                b"title" => title = node_string(value),
                b"lang" => language = node_string(value),
                b"codec" => codec = node_string(value),
                b"selected" => selected = node_flag(value).unwrap_or(false),
                b"external" => external = node_flag(value).unwrap_or(false),
                b"external-filename" => source_url = node_string(value),
                _ => {}
            }
        }

        let Some(id) = id else { continue };
        match kind {
            Some(TrackKind::Audio) => audio.push(AudioTrack {
                id,
                title,
                language,
                codec,
                selected,
            }),
            Some(TrackKind::Subtitle) => subtitles.push(SubtitleTrack {
                id,
                title,
                language,
                codec,
                selected,
                external,
                source_url,
            }),
            _ => {}
        }
    }
    (audio, subtitles)
}

fn parse_chapters(node: &MpvNode) -> Vec<Chapter> {
    let Some(entries) = node_list(node, FORMAT_NODE_ARRAY) else {
        return Vec::new();
    };
    let mut chapters = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(map) = node_map(entry) else {
            continue;
        };
        let Some(values) = (!map.values.is_null()).then_some(map.values) else {
            continue;
        };
        let Some(keys) = (!map.keys.is_null()).then_some(map.keys) else {
            continue;
        };
        let Some(len) = usize::try_from(map.num).ok() else {
            continue;
        };

        let mut start = None;
        let mut title = None;
        for index in 0..len {
            // SAFETY: MPV guarantees both map arrays contain `num` entries.
            let key = unsafe { *keys.add(index) };
            if key.is_null() {
                continue;
            }
            // SAFETY: MPV map keys are null-terminated and values has the same length.
            let key = unsafe { CStr::from_ptr(key) }.to_bytes();
            let value = unsafe { &*values.add(index) };
            match key {
                b"time" => start = node_double(value),
                b"title" => title = node_string(value),
                _ => {}
            }
        }
        let Some(start) = start.filter(|value| value.is_finite() && *value >= 0.0) else {
            continue;
        };
        let index = chapters.len();
        chapters.push(Chapter {
            index,
            title: title
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| format!("Chapter {}", index + 1)),
            start,
            end: start,
        });
    }
    chapters.sort_by(|left, right| left.start.total_cmp(&right.start));
    for (index, chapter) in chapters.iter_mut().enumerate() {
        chapter.index = index;
    }
    chapters
}

fn normalize_chapter_ends(chapters: &mut [Chapter], duration: f64) {
    for index in 0..chapters.len() {
        let start = chapters[index].start;
        let end = chapters
            .get(index + 1)
            .map(|next| next.start)
            .unwrap_or(duration)
            .max(start);
        chapters[index].end = end;
    }
}

#[derive(Clone, Copy)]
enum TrackKind {
    Audio,
    Subtitle,
}

fn node_list(node: &MpvNode, expected_format: c_int) -> Option<&[MpvNode]> {
    if node.format != expected_format {
        return None;
    }
    // SAFETY: The active union member for NODE_ARRAY/NODE_MAP is `list`.
    let list = unsafe { node.value.list };
    if list.is_null() {
        return Some(&[]);
    }
    // SAFETY: MPV guarantees num non-negative and values contains num entries.
    let list = unsafe { &*list };
    let len = usize::try_from(list.num).ok()?;
    if len == 0 {
        Some(&[])
    } else if list.values.is_null() {
        None
    } else {
        // SAFETY: Validated non-null values and MPV-provided length.
        Some(unsafe { std::slice::from_raw_parts(list.values, len) })
    }
}

fn node_map(node: &MpvNode) -> Option<&MpvNodeList> {
    if node.format != FORMAT_NODE_MAP {
        return None;
    }
    // SAFETY: The active union member for NODE_MAP is `list`.
    let list = unsafe { node.value.list };
    (!list.is_null()).then(|| unsafe { &*list })
}

fn node_string_bytes(node: &MpvNode) -> Option<&[u8]> {
    if node.format != FORMAT_STRING {
        return None;
    }
    // SAFETY: Active union member for FORMAT_STRING is string.
    let value = unsafe { node.value.string };
    (!value.is_null()).then(|| unsafe { CStr::from_ptr(value) }.to_bytes())
}

fn node_string(node: &MpvNode) -> Option<String> {
    node_string_bytes(node).map(|value| String::from_utf8_lossy(value).into_owned())
}

fn node_int(node: &MpvNode) -> Option<i64> {
    (node.format == FORMAT_INT64).then_some(unsafe { node.value.int64 })
}

fn node_double(node: &MpvNode) -> Option<f64> {
    (node.format == FORMAT_DOUBLE).then_some(unsafe { node.value.double_ })
}

fn node_flag(node: &MpvNode) -> Option<bool> {
    (node.format == FORMAT_FLAG).then_some(unsafe { node.value.flag != 0 })
}

#[cfg(test)]
mod logic_tests {
    use super::{
        Chapter, HdrMode, append_filter, escape_lavfi_value, hdr_properties,
        normalize_chapter_ends, prepend_decoder, validate_ab_loop,
    };

    #[test]
    fn prepend_decoder_should_preserve_existing_preferences() {
        assert_eq!(prepend_decoder("lavc", "orender"), "orender,lavc");
    }

    #[test]
    fn append_filter_should_preserve_existing_filters() {
        assert_eq!(
            append_filter("scaletempo", "lavfi=[surround]"),
            "scaletempo,lavfi=[surround]"
        );
    }

    #[test]
    fn lavfi_value_should_escape_windows_path_separators_and_drive_colon() {
        assert_eq!(
            escape_lavfi_value(r"C:\HRTF\default.sofa"),
            r"C\:\\HRTF\\default.sofa"
        );
    }

    #[test]
    fn validate_ab_loop_should_accept_a_without_b() {
        assert!(validate_ab_loop(Some(12.0), None).is_ok());
    }

    #[test]
    fn validate_ab_loop_should_reject_b_before_minimum_span() {
        assert_eq!(
            validate_ab_loop(Some(12.0), Some(12.2)),
            Err("Loop point B must be at least 250 ms after point A")
        );
    }

    #[test]
    fn normalize_chapter_ends_should_use_next_start_and_duration() {
        let mut chapters = vec![
            Chapter {
                index: 0,
                title: "Intro".to_owned(),
                start: 0.0,
                end: 0.0,
            },
            Chapter {
                index: 1,
                title: "Story".to_owned(),
                start: 90.0,
                end: 90.0,
            },
        ];

        normalize_chapter_ends(&mut chapters, 3_600.0);

        assert_eq!(
            chapters
                .iter()
                .map(|chapter| chapter.end)
                .collect::<Vec<_>>(),
            vec![90.0, 3_600.0]
        );
    }

    #[test]
    fn hdr_properties_should_disable_passthrough_for_tone_mapping() {
        assert_eq!(hdr_properties(HdrMode::ToneMap), (false, "auto", true));
    }
}
