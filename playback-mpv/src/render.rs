use std::{
    ffi::{CStr, c_char, c_int, c_void},
    marker::PhantomData,
    num::NonZeroU32,
    ptr::NonNull,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use glow::HasContext;

use crate::ffi::{
    MpvClient, MpvError, MpvOpenGlFbo, MpvOpenGlInitParams, MpvRenderContext, MpvRenderParam,
    RENDER_PARAM_ADVANCED_CONTROL, RENDER_PARAM_API_TYPE, RENDER_PARAM_FLIP_Y,
    RENDER_PARAM_INVALID, RENDER_PARAM_OPENGL_FBO, RENDER_PARAM_OPENGL_INIT_PARAMS,
    RENDER_PARAM_SKIP_RENDERING, RENDER_UPDATE_FRAME,
};

/// OpenGL function resolver supplied by Slint while its context is current.
pub type OpenGlProcAddress<'a> = &'a dyn Fn(&CStr) -> *const c_void;

/// The OpenGL API profile backing Slint's renderer context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenGlProfile {
    Desktop,
    Embedded,
}

/// The desktop OpenGL context profile selected by the driver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenGlContextProfile {
    Core,
    Compatibility,
    Unknown,
}

/// Whether MPV hook shaders can run on the current OpenGL context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoShaderSupport {
    Supported,
    Unsupported(VideoShaderUnsupportedReason),
}

/// Why MPV hook shaders are unavailable while plain video remains supported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoShaderUnsupportedReason {
    EmbeddedProfile,
    VersionTooOld { major: u32, minor: u32 },
}

impl std::fmt::Display for VideoShaderUnsupportedReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmbeddedProfile => write!(
                formatter,
                "custom video shaders require desktop OpenGL 3.3 or newer; OpenGL ES was detected"
            ),
            Self::VersionTooOld { major, minor } => write!(
                formatter,
                "custom video shaders require desktop OpenGL 3.3 or newer; detected OpenGL {major}.{minor}"
            ),
        }
    }
}

#[derive(Clone, Copy)]
struct TextureUnitState {
    unit: u32,
    texture_2d: Option<glow::NativeTexture>,
    sampler: Option<glow::NativeSampler>,
}

#[derive(Clone, Copy)]
struct StencilFaceState {
    function: u32,
    reference: i32,
    value_mask: u32,
    write_mask: u32,
    stencil_fail: u32,
    depth_fail: u32,
    depth_pass: u32,
}

struct OpenGlState {
    capabilities: OpenGlCapabilities,
    draw_framebuffer: Option<glow::NativeFramebuffer>,
    read_framebuffer: Option<glow::NativeFramebuffer>,
    renderbuffer: Option<glow::NativeRenderbuffer>,
    program: Option<glow::NativeProgram>,
    vertex_array: Option<glow::NativeVertexArray>,
    array_buffer: Option<glow::NativeBuffer>,
    element_array_buffer: Option<glow::NativeBuffer>,
    pixel_pack_buffer: Option<glow::NativeBuffer>,
    pixel_unpack_buffer: Option<glow::NativeBuffer>,
    active_texture: u32,
    texture_units: Vec<TextureUnitState>,
    viewport: [c_int; 4],
    scissor_box: [c_int; 4],
    clear_color: [f32; 4],
    stencil_clear_value: i32,
    blend_color: [f32; 4],
    color_mask: [bool; 4],
    blend_equation_rgb: u32,
    blend_equation_alpha: u32,
    blend_source_rgb: u32,
    blend_destination_rgb: u32,
    blend_source_alpha: u32,
    blend_destination_alpha: u32,
    depth_function: u32,
    depth_write_mask: bool,
    cull_face_mode: u32,
    front_face: u32,
    stencil_front: StencilFaceState,
    stencil_back: StencilFaceState,
    pack_alignment: i32,
    unpack_alignment: i32,
    blend: bool,
    cull_face: bool,
    depth_test: bool,
    scissor_test: bool,
    stencil_test: bool,
    dither: bool,
    multisample: bool,
    framebuffer_srgb: bool,
    rasterizer_discard: bool,
    sample_alpha_to_coverage: bool,
    sample_coverage: bool,
    sample_coverage_value: f32,
    sample_coverage_invert: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OpenGlCapabilities {
    core_profile: bool,
    sampler_objects: bool,
    separate_framebuffers: bool,
    pixel_buffer_objects: bool,
    vertex_array_objects: bool,
    texture_swizzle: bool,
    rasterizer_discard: bool,
    sized_rgba8: bool,
    desktop_multisample: bool,
    framebuffer_srgb: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OpenGlContextCapabilities {
    state: OpenGlCapabilities,
    fragment_texture_units: u32,
}

impl OpenGlCapabilities {
    fn from_version(major: u32, minor: u32, is_embedded: bool) -> Self {
        let at_least = |required_major, required_minor| {
            major > required_major || (major == required_major && minor >= required_minor)
        };
        Self {
            core_profile: false,
            // Sampler objects are core in OpenGL ES 3.0 and desktop OpenGL 3.3.
            sampler_objects: if is_embedded {
                major >= 3
            } else {
                at_least(3, 3)
            },
            separate_framebuffers: at_least(3, 0),
            pixel_buffer_objects: if is_embedded {
                major >= 3
            } else {
                at_least(2, 1)
            },
            vertex_array_objects: at_least(3, 0),
            texture_swizzle: if is_embedded {
                major >= 3
            } else {
                at_least(3, 3)
            },
            rasterizer_discard: at_least(3, 0),
            sized_rgba8: !is_embedded || major >= 3,
            // These enable flags are desktop GL state. Querying them on GLES can
            // produce GL_INVALID_ENUM and contaminate the shared context.
            desktop_multisample: !is_embedded,
            framebuffer_srgb: !is_embedded && major >= 3,
        }
    }

    fn for_context(gl: &glow::Context) -> Self {
        let version = gl.version();
        let mut capabilities =
            Self::from_version(version.major, version.minor, version.is_embedded);
        capabilities.core_profile = open_gl_context_profile(gl) == OpenGlContextProfile::Core;
        if !capabilities.vertex_array_objects {
            let extensions = gl.supported_extensions();
            capabilities.vertex_array_objects = extensions.contains("GL_OES_vertex_array_object")
                || extensions.contains("GL_ARB_vertex_array_object")
                || extensions.contains("GL_APPLE_vertex_array_object");
        }
        capabilities
    }
}

fn open_gl_context_profile(gl: &glow::Context) -> OpenGlContextProfile {
    let version = gl.version();
    if version.is_embedded || (version.major, version.minor) < (3, 2) {
        return OpenGlContextProfile::Unknown;
    }
    // SAFETY: GL_CONTEXT_PROFILE_MASK is valid for desktop OpenGL 3.2 and
    // newer, which is checked above, and Slint keeps the context current.
    let profile_mask = unsafe { gl.get_parameter_i32(glow::CONTEXT_PROFILE_MASK) as u32 };
    if profile_mask & glow::CONTEXT_CORE_PROFILE_BIT != 0 {
        OpenGlContextProfile::Core
    } else if profile_mask & glow::CONTEXT_COMPATIBILITY_PROFILE_BIT != 0 {
        OpenGlContextProfile::Compatibility
    } else {
        OpenGlContextProfile::Unknown
    }
}

impl OpenGlContextCapabilities {
    fn for_context(gl: &glow::Context) -> Self {
        // SAFETY: Construction happens while Slint keeps this context current.
        let fragment_texture_units =
            unsafe { gl.get_parameter_i32(glow::MAX_TEXTURE_IMAGE_UNITS).max(1) as u32 };
        Self {
            state: OpenGlCapabilities::for_context(gl),
            fragment_texture_units,
        }
    }

    fn texture_units(self, active_texture: u32) -> Vec<u32> {
        let mut units = (0..self.fragment_texture_units)
            .map(|index| glow::TEXTURE0 + index)
            .collect::<Vec<_>>();
        if !units.contains(&active_texture) {
            units.push(active_texture);
        }
        units
    }
}

fn prepare_for_mpv(gl: &glow::Context, context_capabilities: OpenGlContextCapabilities) {
    // SAFETY: Slint's rendering notifier guarantees that this context is
    // current. The surrounding OpenGlStateGuard restores every changed value.
    unsafe {
        let active_texture = gl.get_parameter_i32(glow::ACTIVE_TEXTURE) as u32;
        for unit in context_capabilities.texture_units(active_texture) {
            gl.active_texture(unit);
            gl.bind_texture(glow::TEXTURE_2D, None);
            if context_capabilities.state.sampler_objects {
                gl.bind_sampler(unit - glow::TEXTURE0, None);
            }
        }
        gl.active_texture(glow::TEXTURE0);
        gl.use_program(None);
        if context_capabilities.state.vertex_array_objects {
            gl.bind_vertex_array(None);
        }
        gl.bind_buffer(glow::ARRAY_BUFFER, None);
        // In a desktop core profile, ELEMENT_ARRAY_BUFFER is VAO state and
        // changing it while VAO 0 is bound is GL_INVALID_OPERATION.
        if !context_capabilities.state.core_profile {
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, None);
        }
        gl.disable(glow::BLEND);
        gl.disable(glow::CULL_FACE);
        gl.disable(glow::DEPTH_TEST);
        gl.disable(glow::SCISSOR_TEST);
        gl.disable(glow::STENCIL_TEST);
        gl.color_mask(true, true, true, true);
        gl.depth_mask(true);
    }
}

impl OpenGlState {
    fn capture(gl: &glow::Context, context_capabilities: OpenGlContextCapabilities) -> Self {
        let mut viewport = [0; 4];
        let mut scissor_box = [0; 4];
        let mut clear_color = [0.0; 4];
        let mut blend_color = [0.0; 4];
        // SAFETY: Slint guarantees that its OpenGL context is current for the
        // rendering notifier callback.
        unsafe {
            let capabilities = context_capabilities.state;
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
            gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut scissor_box);
            gl.get_parameter_f32_slice(glow::COLOR_CLEAR_VALUE, &mut clear_color);
            gl.get_parameter_f32_slice(glow::BLEND_COLOR, &mut blend_color);
            let active_texture = gl.get_parameter_i32(glow::ACTIVE_TEXTURE) as u32;
            let mut texture_units =
                Vec::with_capacity(context_capabilities.fragment_texture_units as usize + 1);
            for unit in context_capabilities.texture_units(active_texture) {
                gl.active_texture(unit);
                texture_units.push(TextureUnitState {
                    unit,
                    texture_2d: native_texture(gl.get_parameter_i32(glow::TEXTURE_BINDING_2D)),
                    sampler: capabilities
                        .sampler_objects
                        .then(|| native_sampler(gl.get_parameter_i32(glow::SAMPLER_BINDING)))
                        .flatten(),
                });
            }
            gl.active_texture(active_texture);
            let (draw_framebuffer, read_framebuffer) = if capabilities.separate_framebuffers {
                (
                    native_framebuffer(gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING)),
                    native_framebuffer(gl.get_parameter_i32(glow::READ_FRAMEBUFFER_BINDING)),
                )
            } else {
                let framebuffer =
                    native_framebuffer(gl.get_parameter_i32(glow::FRAMEBUFFER_BINDING));
                (framebuffer, framebuffer)
            };
            Self {
                capabilities,
                draw_framebuffer,
                read_framebuffer,
                renderbuffer: native_renderbuffer(gl.get_parameter_i32(glow::RENDERBUFFER_BINDING)),
                program: native_program(gl.get_parameter_i32(glow::CURRENT_PROGRAM)),
                vertex_array: capabilities
                    .vertex_array_objects
                    .then(|| native_vertex_array(gl.get_parameter_i32(glow::VERTEX_ARRAY_BINDING)))
                    .flatten(),
                array_buffer: native_buffer(gl.get_parameter_i32(glow::ARRAY_BUFFER_BINDING)),
                element_array_buffer: native_buffer(
                    gl.get_parameter_i32(glow::ELEMENT_ARRAY_BUFFER_BINDING),
                ),
                pixel_pack_buffer: capabilities
                    .pixel_buffer_objects
                    .then(|| native_buffer(gl.get_parameter_i32(glow::PIXEL_PACK_BUFFER_BINDING)))
                    .flatten(),
                pixel_unpack_buffer: capabilities
                    .pixel_buffer_objects
                    .then(|| native_buffer(gl.get_parameter_i32(glow::PIXEL_UNPACK_BUFFER_BINDING)))
                    .flatten(),
                active_texture,
                texture_units,
                viewport,
                scissor_box,
                clear_color,
                stencil_clear_value: gl.get_parameter_i32(glow::STENCIL_CLEAR_VALUE),
                blend_color,
                color_mask: gl.get_parameter_bool_array(glow::COLOR_WRITEMASK),
                blend_equation_rgb: gl.get_parameter_i32(glow::BLEND_EQUATION_RGB) as u32,
                blend_equation_alpha: gl.get_parameter_i32(glow::BLEND_EQUATION_ALPHA) as u32,
                blend_source_rgb: gl.get_parameter_i32(glow::BLEND_SRC_RGB) as u32,
                blend_destination_rgb: gl.get_parameter_i32(glow::BLEND_DST_RGB) as u32,
                blend_source_alpha: gl.get_parameter_i32(glow::BLEND_SRC_ALPHA) as u32,
                blend_destination_alpha: gl.get_parameter_i32(glow::BLEND_DST_ALPHA) as u32,
                depth_function: gl.get_parameter_i32(glow::DEPTH_FUNC) as u32,
                depth_write_mask: gl.get_parameter_bool(glow::DEPTH_WRITEMASK),
                cull_face_mode: gl.get_parameter_i32(glow::CULL_FACE_MODE) as u32,
                front_face: gl.get_parameter_i32(glow::FRONT_FACE) as u32,
                stencil_front: capture_stencil_face(gl, false),
                stencil_back: capture_stencil_face(gl, true),
                pack_alignment: gl.get_parameter_i32(glow::PACK_ALIGNMENT),
                unpack_alignment: gl.get_parameter_i32(glow::UNPACK_ALIGNMENT),
                blend: gl.is_enabled(glow::BLEND),
                cull_face: gl.is_enabled(glow::CULL_FACE),
                depth_test: gl.is_enabled(glow::DEPTH_TEST),
                scissor_test: gl.is_enabled(glow::SCISSOR_TEST),
                stencil_test: gl.is_enabled(glow::STENCIL_TEST),
                dither: gl.is_enabled(glow::DITHER),
                multisample: capabilities.desktop_multisample && gl.is_enabled(glow::MULTISAMPLE),
                framebuffer_srgb: capabilities.framebuffer_srgb
                    && gl.is_enabled(glow::FRAMEBUFFER_SRGB),
                rasterizer_discard: capabilities.rasterizer_discard
                    && gl.is_enabled(glow::RASTERIZER_DISCARD),
                sample_alpha_to_coverage: gl.is_enabled(glow::SAMPLE_ALPHA_TO_COVERAGE),
                sample_coverage: gl.is_enabled(glow::SAMPLE_COVERAGE),
                sample_coverage_value: gl.get_parameter_f32(glow::SAMPLE_COVERAGE_VALUE),
                sample_coverage_invert: gl.get_parameter_bool(glow::SAMPLE_COVERAGE_INVERT),
            }
        }
    }

    fn restore(self, gl: &glow::Context) {
        // SAFETY: These values were captured from the same current context
        // immediately before MPV rendering.
        unsafe {
            if self.capabilities.separate_framebuffers {
                gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, self.draw_framebuffer);
                gl.bind_framebuffer(glow::READ_FRAMEBUFFER, self.read_framebuffer);
            } else {
                gl.bind_framebuffer(glow::FRAMEBUFFER, self.draw_framebuffer);
            }
            gl.bind_renderbuffer(glow::RENDERBUFFER, self.renderbuffer);
            gl.viewport(
                self.viewport[0],
                self.viewport[1],
                self.viewport[2],
                self.viewport[3],
            );
            gl.scissor(
                self.scissor_box[0],
                self.scissor_box[1],
                self.scissor_box[2],
                self.scissor_box[3],
            );
            gl.use_program(self.program);
            if self.capabilities.vertex_array_objects {
                gl.bind_vertex_array(self.vertex_array);
            }
            gl.bind_buffer(glow::ARRAY_BUFFER, self.array_buffer);
            if !self.capabilities.core_profile || self.vertex_array.is_some() {
                gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, self.element_array_buffer);
            }
            if self.capabilities.pixel_buffer_objects {
                gl.bind_buffer(glow::PIXEL_PACK_BUFFER, self.pixel_pack_buffer);
                gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, self.pixel_unpack_buffer);
            }
            for texture_unit in self.texture_units {
                gl.active_texture(texture_unit.unit);
                gl.bind_texture(glow::TEXTURE_2D, texture_unit.texture_2d);
                if self.capabilities.sampler_objects {
                    gl.bind_sampler(texture_unit.unit - glow::TEXTURE0, texture_unit.sampler);
                }
            }
            gl.active_texture(self.active_texture);
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, self.pack_alignment);
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, self.unpack_alignment);
            gl.blend_equation_separate(self.blend_equation_rgb, self.blend_equation_alpha);
            gl.blend_func_separate(
                self.blend_source_rgb,
                self.blend_destination_rgb,
                self.blend_source_alpha,
                self.blend_destination_alpha,
            );
            gl.blend_color(
                self.blend_color[0],
                self.blend_color[1],
                self.blend_color[2],
                self.blend_color[3],
            );
            gl.clear_color(
                self.clear_color[0],
                self.clear_color[1],
                self.clear_color[2],
                self.clear_color[3],
            );
            gl.clear_stencil(self.stencil_clear_value);
            gl.sample_coverage(self.sample_coverage_value, self.sample_coverage_invert);
            gl.color_mask(
                self.color_mask[0],
                self.color_mask[1],
                self.color_mask[2],
                self.color_mask[3],
            );
            gl.depth_func(self.depth_function);
            gl.depth_mask(self.depth_write_mask);
            gl.cull_face(self.cull_face_mode);
            gl.front_face(self.front_face);
            restore_stencil_face(gl, glow::FRONT, self.stencil_front);
            restore_stencil_face(gl, glow::BACK, self.stencil_back);
            restore_capability(gl, glow::BLEND, self.blend);
            restore_capability(gl, glow::CULL_FACE, self.cull_face);
            restore_capability(gl, glow::DEPTH_TEST, self.depth_test);
            restore_capability(gl, glow::SCISSOR_TEST, self.scissor_test);
            restore_capability(gl, glow::STENCIL_TEST, self.stencil_test);
            restore_capability(gl, glow::DITHER, self.dither);
            if self.capabilities.desktop_multisample {
                restore_capability(gl, glow::MULTISAMPLE, self.multisample);
            }
            if self.capabilities.framebuffer_srgb {
                restore_capability(gl, glow::FRAMEBUFFER_SRGB, self.framebuffer_srgb);
            }
            if self.capabilities.rasterizer_discard {
                restore_capability(gl, glow::RASTERIZER_DISCARD, self.rasterizer_discard);
            }
            restore_capability(
                gl,
                glow::SAMPLE_ALPHA_TO_COVERAGE,
                self.sample_alpha_to_coverage,
            );
            restore_capability(gl, glow::SAMPLE_COVERAGE, self.sample_coverage);
        }
    }
}

struct OpenGlStateGuard<'a> {
    gl: &'a glow::Context,
    state: Option<OpenGlState>,
}

impl<'a> OpenGlStateGuard<'a> {
    fn new(gl: &'a glow::Context, capabilities: OpenGlContextCapabilities) -> Self {
        Self {
            gl,
            state: Some(OpenGlState::capture(gl, capabilities)),
        }
    }
}

impl Drop for OpenGlStateGuard<'_> {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            state.restore(self.gl);
        }
    }
}

unsafe fn capture_stencil_face(gl: &glow::Context, back: bool) -> StencilFaceState {
    let parameters = if back {
        [
            glow::STENCIL_BACK_FUNC,
            glow::STENCIL_BACK_REF,
            glow::STENCIL_BACK_VALUE_MASK,
            glow::STENCIL_BACK_WRITEMASK,
            glow::STENCIL_BACK_FAIL,
            glow::STENCIL_BACK_PASS_DEPTH_FAIL,
            glow::STENCIL_BACK_PASS_DEPTH_PASS,
        ]
    } else {
        [
            glow::STENCIL_FUNC,
            glow::STENCIL_REF,
            glow::STENCIL_VALUE_MASK,
            glow::STENCIL_WRITEMASK,
            glow::STENCIL_FAIL,
            glow::STENCIL_PASS_DEPTH_FAIL,
            glow::STENCIL_PASS_DEPTH_PASS,
        ]
    };
    // SAFETY: All queried values are scalar OpenGL state from the current
    // context guaranteed by Slint's rendering notifier.
    unsafe {
        StencilFaceState {
            function: gl.get_parameter_i32(parameters[0]) as u32,
            reference: gl.get_parameter_i32(parameters[1]),
            value_mask: gl.get_parameter_i32(parameters[2]) as u32,
            write_mask: gl.get_parameter_i32(parameters[3]) as u32,
            stencil_fail: gl.get_parameter_i32(parameters[4]) as u32,
            depth_fail: gl.get_parameter_i32(parameters[5]) as u32,
            depth_pass: gl.get_parameter_i32(parameters[6]) as u32,
        }
    }
}

unsafe fn restore_stencil_face(gl: &glow::Context, face: u32, state: StencilFaceState) {
    // SAFETY: The values were captured from this same current OpenGL context.
    unsafe {
        gl.stencil_func_separate(face, state.function, state.reference, state.value_mask);
        gl.stencil_mask_separate(face, state.write_mask);
        gl.stencil_op_separate(face, state.stencil_fail, state.depth_fail, state.depth_pass);
    }
}

unsafe fn restore_capability(gl: &glow::Context, capability: u32, enabled: bool) {
    if enabled {
        // SAFETY: `capability` is a valid OpenGL capability captured from this
        // same current context immediately before MPV rendering.
        unsafe { gl.enable(capability) };
    } else {
        // SAFETY: See the enabled branch above.
        unsafe { gl.disable(capability) };
    }
}

fn native_framebuffer(value: c_int) -> Option<glow::NativeFramebuffer> {
    NonZeroU32::new(value as u32).map(glow::NativeFramebuffer)
}

fn native_texture(value: c_int) -> Option<glow::NativeTexture> {
    NonZeroU32::new(value as u32).map(glow::NativeTexture)
}

fn native_buffer(value: c_int) -> Option<glow::NativeBuffer> {
    NonZeroU32::new(value as u32).map(glow::NativeBuffer)
}

fn native_renderbuffer(value: c_int) -> Option<glow::NativeRenderbuffer> {
    NonZeroU32::new(value as u32).map(glow::NativeRenderbuffer)
}

fn native_program(value: c_int) -> Option<glow::NativeProgram> {
    NonZeroU32::new(value as u32).map(glow::NativeProgram)
}

fn native_vertex_array(value: c_int) -> Option<glow::NativeVertexArray> {
    NonZeroU32::new(value as u32).map(glow::NativeVertexArray)
}

fn native_sampler(value: c_int) -> Option<glow::NativeSampler> {
    NonZeroU32::new(value as u32).map(glow::NativeSampler)
}

struct OpenGlRenderTarget {
    framebuffer: glow::NativeFramebuffer,
    texture: glow::NativeTexture,
    width: i32,
    height: i32,
    mpv_internal_format: c_int,
}

struct OpenGlRenderTargets {
    slots: [OpenGlRenderTarget; 2],
    next_render_index: usize,
}

impl OpenGlRenderTargets {
    fn has_size(&self, width: i32, height: i32) -> bool {
        self.slots[0].width == width && self.slots[0].height == height
    }
}

/// Borrowed OpenGL texture information for compositing the MPV frame in Slint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoTexture {
    texture_id: NonZeroU32,
    width: u32,
    height: u32,
}

impl VideoTexture {
    pub fn texture_id(self) -> NonZeroU32 {
        self.texture_id
    }

    pub fn width(self) -> u32 {
        self.width
    }

    pub fn height(self) -> u32 {
        self.height
    }
}

fn create_render_target(
    gl: &glow::Context,
    context_capabilities: OpenGlContextCapabilities,
    width: i32,
    height: i32,
) -> Result<OpenGlRenderTarget, MpvError> {
    let _state_guard = OpenGlStateGuard::new(gl, context_capabilities);
    (|| {
        let capabilities = context_capabilities.state;
        // SAFETY: All resources are created in Slint's current OpenGL context.
        let texture = unsafe { gl.create_texture() }.map_err(MpvError::OpenGl)?;
        unsafe {
            gl.active_texture(glow::TEXTURE0);
            if capabilities.pixel_buffer_objects {
                gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            }
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            // Texture swizzle is core only in desktop GL 3.3 / GLES 3.0. MPV's
            // video shader writes opaque output on GLES2, where querying this
            // state would contaminate Slint's shared context with INVALID_ENUM.
            if capabilities.texture_swizzle {
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_SWIZZLE_A, glow::ONE as i32);
            }
            let texture_internal_format = if capabilities.sized_rgba8 {
                glow::RGBA8
            } else {
                glow::RGBA
            };
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                texture_internal_format as i32,
                width,
                height,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
        }

        let framebuffer = match unsafe { gl.create_framebuffer() } {
            Ok(framebuffer) => framebuffer,
            Err(error) => {
                // SAFETY: `texture` was created above in the current context.
                unsafe { gl.delete_texture(texture) };
                return Err(MpvError::OpenGl(error));
            }
        };
        // SAFETY: The framebuffer and texture belong to the current context.
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );
        }
        let status = unsafe { gl.check_framebuffer_status(glow::FRAMEBUFFER) };
        if status != glow::FRAMEBUFFER_COMPLETE {
            // SAFETY: Both resources were created in this current context.
            unsafe {
                gl.delete_framebuffer(framebuffer);
                gl.delete_texture(texture);
            }
            return Err(MpvError::IncompleteOpenGlFramebuffer { status });
        }

        Ok(OpenGlRenderTarget {
            framebuffer,
            texture,
            width,
            height,
            mpv_internal_format: if capabilities.sized_rgba8 {
                glow::RGBA8 as c_int
            } else {
                0
            },
        })
    })()
}

fn delete_render_target(gl: &glow::Context, target: OpenGlRenderTarget) {
    // SAFETY: The target was created in this current OpenGL context and is no
    // longer borrowed by the caller when this function takes ownership.
    unsafe {
        gl.delete_framebuffer(target.framebuffer);
        gl.delete_texture(target.texture);
    }
}

fn create_render_targets(
    gl: &glow::Context,
    context_capabilities: OpenGlContextCapabilities,
    width: i32,
    height: i32,
) -> Result<OpenGlRenderTargets, MpvError> {
    let first = create_render_target(gl, context_capabilities, width, height)?;
    let second = match create_render_target(gl, context_capabilities, width, height) {
        Ok(target) => target,
        Err(error) => {
            delete_render_target(gl, first);
            return Err(error);
        }
    };
    Ok(OpenGlRenderTargets {
        slots: [first, second],
        next_render_index: 0,
    })
}

#[derive(Clone)]
pub struct RenderSource {
    client: Arc<MpvClient>,
}

impl RenderSource {
    pub(crate) fn new(client: Arc<MpvClient>) -> Self {
        Self { client }
    }

    /// Creates the MPV OpenGL render context on the GUI thread.
    ///
    /// The caller must invoke this while Slint's OpenGL context is current and
    /// retain the returned context on that same thread.
    pub fn create_context(
        &self,
        resolver: OpenGlProcAddress<'_>,
        request_redraw: impl FnMut() + Send + 'static,
    ) -> Result<RenderContext, MpvError> {
        ResolverBridge::create(&self.client, resolver, request_redraw)
    }
}

struct RedrawCallback {
    pending: AtomicBool,
    callback: Mutex<Box<dyn FnMut() + Send>>,
}

struct ResolverBridge<'a> {
    resolver: OpenGlProcAddress<'a>,
}

impl<'a> ResolverBridge<'a> {
    fn create(
        client: &Arc<MpvClient>,
        resolver: OpenGlProcAddress<'a>,
        request_redraw: impl FnMut() + Send + 'static,
    ) -> Result<RenderContext, MpvError> {
        // SAFETY: Slint invokes this function with its OpenGL context current,
        // and its resolver remains valid for synchronous symbol loading.
        //
        // Phase timings are logged because this whole function runs synchronously
        // on the UI thread; a cold GL driver can make one of these steps stall for
        // seconds, freezing the window, and the breakdown pinpoints which one.
        let load_start = std::time::Instant::now();
        let mut resolved_symbols = 0u32;
        let gl = Rc::new(unsafe {
            glow::Context::from_loader_function_cstr(|name| {
                resolved_symbols += 1;
                resolver(name)
            })
        });
        let load_ms = load_start.elapsed().as_millis();

        let query_start = std::time::Instant::now();
        let capabilities = OpenGlContextCapabilities::for_context(&gl);
        let diagnostics = OpenGlDiagnostics::for_context(&gl);
        let query_ms = query_start.elapsed().as_millis();

        let gl_state = OpenGlStateGuard::new(&gl, capabilities);
        let prepare_start = std::time::Instant::now();
        prepare_for_mpv(&gl, capabilities);
        let prepare_ms = prepare_start.elapsed().as_millis();
        let mut bridge = Self { resolver };
        let mut init_params = MpvOpenGlInitParams {
            get_proc_address: Some(resolve_open_gl),
            get_proc_address_ctx: (&mut bridge as *mut Self).cast(),
        };
        let mut advanced_control: c_int = 1;
        let api_type = c"opengl";
        let mut params = [
            MpvRenderParam {
                param_type: RENDER_PARAM_API_TYPE,
                data: api_type.as_ptr().cast_mut().cast(),
            },
            MpvRenderParam {
                param_type: RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: (&mut init_params as *mut MpvOpenGlInitParams).cast(),
            },
            MpvRenderParam {
                param_type: RENDER_PARAM_ADVANCED_CONTROL,
                data: (&mut advanced_control as *mut c_int).cast(),
            },
            MpvRenderParam {
                param_type: RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        let mut raw_context = std::ptr::null_mut();
        // SAFETY: Slint's context is current. Parameter storage and the resolver
        // bridge remain valid for the synchronous context creation call.
        let create_start = std::time::Instant::now();
        client.api.result(unsafe {
            (client.api.render_create)(&mut raw_context, client.handle(), params.as_mut_ptr())
        })?;
        let render_create_ms = create_start.elapsed().as_millis();

        tracing::info!(
            symbol_load_ms = load_ms,
            resolved_symbols,
            capability_query_ms = query_ms,
            state_prepare_ms = prepare_ms,
            render_create_ms,
            "MPV render context phase timings"
        );

        let context = NonNull::new(raw_context).ok_or(MpvError::NullRenderContext)?;
        let redraw = Box::new(RedrawCallback {
            pending: AtomicBool::new(false),
            callback: Mutex::new(Box::new(request_redraw)),
        });
        // SAFETY: `redraw` remains boxed at a stable address until Drop removes
        // the callback before freeing it.
        unsafe {
            (client.api.render_set_update_callback)(
                context.as_ptr(),
                Some(render_update),
                (&*redraw as *const RedrawCallback).cast_mut().cast(),
            )
        };
        // Context creation is permitted to alter OpenGL state, including
        // disabling dithering. Restore Slint's exact state before returning.
        drop(gl_state);

        Ok(RenderContext {
            context,
            client: client.clone(),
            gl,
            capabilities,
            diagnostics,
            render_targets: None,
            force_render: false,
            frame_pending: false,
            last_gl_error: None,
            pending_gl_error: None,
            _redraw: redraw,
            _not_send: PhantomData,
        })
    }
}

unsafe extern "C" fn resolve_open_gl(context: *mut c_void, name: *const c_char) -> *mut c_void {
    if context.is_null() || name.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: Context points to ResolverBridge during the synchronous create
    // call, and MPV supplies a null-terminated GL function name.
    let bridge = unsafe { &*(context as *const ResolverBridge<'_>) };
    // SAFETY: Name validity is guaranteed by MPV's render API.
    let name = unsafe { CStr::from_ptr(name) };
    (bridge.resolver)(name).cast_mut()
}

unsafe extern "C" fn render_update(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: Context points to RedrawCallback until the callback is removed.
    let callback = unsafe { &*(context as *const RedrawCallback) };
    if callback.pending.swap(true, Ordering::AcqRel) {
        return;
    }
    let redraw_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut redraw = callback
            .callback
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        redraw();
    }));
    if redraw_result.is_err() {
        callback.pending.store(false, Ordering::Release);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenGlDiagnostics {
    pub vendor: String,
    pub renderer: String,
    pub version: String,
    pub major: u32,
    pub minor: u32,
    pub profile: OpenGlProfile,
    pub context_profile: OpenGlContextProfile,
    pub shading_language_version: String,
}

impl OpenGlDiagnostics {
    fn for_context(gl: &glow::Context) -> Self {
        let version = gl.version();
        // SAFETY: Diagnostics are captured during render-context construction,
        // while Slint keeps the OpenGL context current.
        unsafe {
            Self {
                vendor: gl.get_parameter_string(glow::VENDOR),
                renderer: gl.get_parameter_string(glow::RENDERER),
                version: gl.get_parameter_string(glow::VERSION),
                major: version.major,
                minor: version.minor,
                profile: if version.is_embedded {
                    OpenGlProfile::Embedded
                } else {
                    OpenGlProfile::Desktop
                },
                context_profile: open_gl_context_profile(gl),
                shading_language_version: gl.get_parameter_string(glow::SHADING_LANGUAGE_VERSION),
            }
        }
    }

    pub fn video_shader_support(&self) -> VideoShaderSupport {
        match self.profile {
            OpenGlProfile::Embedded => {
                VideoShaderSupport::Unsupported(VideoShaderUnsupportedReason::EmbeddedProfile)
            }
            OpenGlProfile::Desktop if (self.major, self.minor) < (3, 3) => {
                VideoShaderSupport::Unsupported(VideoShaderUnsupportedReason::VersionTooOld {
                    major: self.major,
                    minor: self.minor,
                })
            }
            OpenGlProfile::Desktop => VideoShaderSupport::Supported,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderOutcome {
    NoFrame,
    Rendered {
        texture: VideoTexture,
        frame_ready: bool,
    },
}

/// MPV render context owned and used exclusively by Slint's GUI thread.
pub struct RenderContext {
    context: NonNull<MpvRenderContext>,
    client: Arc<MpvClient>,
    gl: Rc<glow::Context>,
    capabilities: OpenGlContextCapabilities,
    diagnostics: OpenGlDiagnostics,
    render_targets: Option<OpenGlRenderTargets>,
    force_render: bool,
    frame_pending: bool,
    last_gl_error: Option<u32>,
    pending_gl_error: Option<u32>,
    _redraw: Box<RedrawCallback>,
    _not_send: PhantomData<Rc<()>>,
}

impl RenderContext {
    pub fn open_gl_diagnostics(&self) -> OpenGlDiagnostics {
        self.diagnostics.clone()
    }

    /// Allocates the double-buffered private video textures on first use.
    ///
    /// Returns `true` when the targets were created or recreated. Both texture
    /// IDs remain owned by this render context until resize or teardown.
    pub fn ensure_video_textures(&mut self, width: i32, height: i32) -> Result<bool, MpvError> {
        if width <= 0 || height <= 0 {
            return Ok(false);
        }
        if self
            .render_targets
            .as_ref()
            .is_some_and(|targets| targets.has_size(width, height))
        {
            return Ok(false);
        }
        let targets = create_render_targets(&self.gl, self.capabilities, width, height)?;
        if let Some(previous) = self.render_targets.replace(targets) {
            for target in previous.slots {
                delete_render_target(&self.gl, target);
            }
        }
        self.force_render = true;
        self.frame_pending = false;
        Ok(true)
    }

    pub fn has_video_textures(&self) -> bool {
        self.render_targets.is_some()
    }

    /// Drains libmpv's advanced-control update callback.
    ///
    /// This must be called for every requested redraw even while the player is
    /// hidden. Hidden frames are explicitly skipped so libmpv can continue its
    /// render lifecycle without presenting stale work later.
    pub fn process_updates(&mut self, render_visible: bool) -> Result<bool, MpvError> {
        if !self._redraw.pending.swap(false, Ordering::AcqRel) {
            return Ok(false);
        }
        let state_guard = OpenGlStateGuard::new(&self.gl, self.capabilities);
        prepare_for_mpv(&self.gl, self.capabilities);
        // SAFETY: Called on the render-context owner thread while Slint's
        // OpenGL context is current.
        let update_flags = unsafe { (self.client.api.render_update)(self.context.as_ptr()) };
        let has_frame = update_flags & RENDER_UPDATE_FRAME != 0;
        let result = if has_frame && !render_visible {
            let mut skip: c_int = 1;
            let mut params = [
                MpvRenderParam {
                    param_type: RENDER_PARAM_SKIP_RENDERING,
                    data: (&mut skip as *mut c_int).cast(),
                },
                MpvRenderParam {
                    param_type: RENDER_PARAM_INVALID,
                    data: std::ptr::null_mut(),
                },
            ];
            // SAFETY: The same OpenGL context used to create the render context
            // is current. MPV explicitly permits skip rendering without an FBO.
            self.client.api.result(unsafe {
                (self.client.api.render)(self.context.as_ptr(), params.as_mut_ptr())
            })
        } else {
            Ok(())
        };
        drop(state_guard);
        self.capture_gl_error();
        result?;
        if has_frame {
            self.frame_pending = render_visible;
        }
        Ok(has_frame)
    }

    /// Renders the next MPV frame into the back RGBA texture.
    ///
    /// The caller should publish the returned texture to Slint and request one
    /// more redraw. Alternating targets prevents Slint from sampling the same
    /// texture that libmpv is updating.
    pub fn render(&mut self) -> Result<RenderOutcome, MpvError> {
        let update_requested = self._redraw.pending.swap(false, Ordering::AcqRel);
        if !update_requested && !self.frame_pending && !self.force_render {
            return Ok(RenderOutcome::NoFrame);
        }

        let state_guard = OpenGlStateGuard::new(&self.gl, self.capabilities);
        prepare_for_mpv(&self.gl, self.capabilities);
        if update_requested {
            // SAFETY: Called on the render-context owner thread while Slint's
            // OpenGL context is current.
            let update_flags = unsafe { (self.client.api.render_update)(self.context.as_ptr()) };
            if update_flags & RENDER_UPDATE_FRAME != 0 {
                self.frame_pending = true;
            }
        }

        let Some(targets) = self.render_targets.as_ref() else {
            drop(state_guard);
            self.capture_gl_error();
            return Ok(RenderOutcome::NoFrame);
        };
        if !self.frame_pending && !self.force_render {
            drop(state_guard);
            self.capture_gl_error();
            return Ok(RenderOutcome::NoFrame);
        }
        let frame_ready = self.frame_pending;
        let render_index = targets.next_render_index;
        let target = &targets.slots[render_index];
        let framebuffer_id = target.framebuffer.0.get() as c_int;
        let texture = VideoTexture {
            texture_id: target.texture.0,
            width: target.width as u32,
            height: target.height as u32,
        };
        let width = target.width;
        let height = target.height;
        let mpv_internal_format = target.mpv_internal_format;

        // SAFETY: The target belongs to this current context. Clearing first
        // guarantees deterministic black letterboxing.
        unsafe {
            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(target.framebuffer));
            self.gl.viewport(0, 0, width, height);
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.color_mask(true, true, true, true);
            self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
        }
        let mut framebuffer = MpvOpenGlFbo {
            fbo: framebuffer_id,
            width,
            height,
            internal_format: mpv_internal_format,
        };
        // Slint's borrowed OpenGL texture path samples this FBO in the same
        // orientation produced by libmpv. Requesting MPV's optional flip here
        // applies a second coordinate conversion and turns the frame upside
        // down (confirmed on Slint's Skia OpenGL render path).
        let mut flip_y: c_int = 0;
        let mut params = [
            MpvRenderParam {
                param_type: RENDER_PARAM_OPENGL_FBO,
                data: (&mut framebuffer as *mut MpvOpenGlFbo).cast(),
            },
            MpvRenderParam {
                param_type: RENDER_PARAM_FLIP_Y,
                data: (&mut flip_y as *mut c_int).cast(),
            },
            MpvRenderParam {
                param_type: RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        // SAFETY: Called on the render-context owner thread while Slint keeps
        // the framebuffer and GL context current.
        let result = self.client.api.result(unsafe {
            (self.client.api.render)(self.context.as_ptr(), params.as_mut_ptr())
        });
        drop(state_guard);
        self.capture_gl_error();
        result.map(|()| {
            if let Some(targets) = self.render_targets.as_mut() {
                targets.next_render_index = (render_index + 1) % targets.slots.len();
            }
            self.force_render = false;
            self.frame_pending = false;
            RenderOutcome::Rendered {
                texture,
                frame_ready,
            }
        })
    }

    fn capture_gl_error(&mut self) {
        // SAFETY: The current context remains owned by Slint's rendering
        // callback. Reading the error flag does not mutate render resources.
        let gl_error = unsafe { self.gl.get_error() };
        if gl_error != glow::NO_ERROR && self.last_gl_error != Some(gl_error) {
            self.last_gl_error = Some(gl_error);
            self.pending_gl_error = Some(gl_error);
        }
    }

    /// Returns and clears the most recent OpenGL error observed after rendering.
    pub fn take_gl_error(&mut self) -> Option<u32> {
        self.pending_gl_error.take()
    }
}

impl Drop for RenderContext {
    fn drop(&mut self) {
        // SAFETY: Drop runs on the GUI thread. Removing the update callback
        // prevents MPV from touching `_redraw` before the context is freed.
        unsafe {
            (self.client.api.render_set_update_callback)(
                self.context.as_ptr(),
                None,
                std::ptr::null_mut(),
            );
            (self.client.api.render_free)(self.context.as_ptr());
        }
        if let Some(targets) = self.render_targets.take() {
            for target in targets.slots {
                delete_render_target(&self.gl, target);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        OpenGlCapabilities, OpenGlContextProfile, OpenGlDiagnostics, OpenGlProfile,
        VideoShaderSupport, VideoShaderUnsupportedReason,
    };

    fn diagnostics(major: u32, minor: u32, profile: OpenGlProfile) -> OpenGlDiagnostics {
        OpenGlDiagnostics {
            vendor: "test vendor".to_owned(),
            renderer: "test renderer".to_owned(),
            version: format!("{major}.{minor}"),
            major,
            minor,
            profile,
            context_profile: OpenGlContextProfile::Unknown,
            shading_language_version: "test GLSL".to_owned(),
        }
    }

    #[test]
    fn sampler_objects_are_supported_by_opengl_es_3() {
        let capabilities = OpenGlCapabilities::from_version(3, 0, true);

        assert!(capabilities.sampler_objects);
    }

    #[test]
    fn sampler_objects_require_desktop_opengl_3_3() {
        let before = OpenGlCapabilities::from_version(3, 2, false);
        let supported = OpenGlCapabilities::from_version(3, 3, false);

        assert!(!before.sampler_objects && supported.sampler_objects);
    }

    #[test]
    fn desktop_only_enable_flags_are_not_queried_on_opengl_es() {
        let capabilities = OpenGlCapabilities::from_version(3, 2, true);

        assert!(!capabilities.desktop_multisample && !capabilities.framebuffer_srgb);
    }

    #[test]
    fn opengl_es_2_skips_es_3_only_shared_state() {
        let capabilities = OpenGlCapabilities::from_version(2, 0, true);

        assert!(!capabilities.sampler_objects);
        assert!(!capabilities.separate_framebuffers);
        assert!(!capabilities.pixel_buffer_objects);
        assert!(!capabilities.vertex_array_objects);
        assert!(!capabilities.texture_swizzle);
        assert!(!capabilities.rasterizer_discard);
        assert!(!capabilities.sized_rgba8);
    }

    #[test]
    fn desktop_opengl_3_3_and_newer_support_video_shaders() {
        assert_eq!(
            diagnostics(3, 3, OpenGlProfile::Desktop).video_shader_support(),
            VideoShaderSupport::Supported
        );
        assert_eq!(
            diagnostics(4, 6, OpenGlProfile::Desktop).video_shader_support(),
            VideoShaderSupport::Supported
        );
    }

    #[test]
    fn desktop_opengl_3_2_and_older_reject_video_shaders() {
        assert_eq!(
            diagnostics(3, 2, OpenGlProfile::Desktop).video_shader_support(),
            VideoShaderSupport::Unsupported(VideoShaderUnsupportedReason::VersionTooOld {
                major: 3,
                minor: 2,
            })
        );
    }

    #[test]
    fn opengl_es_rejects_video_shaders_without_rejecting_rendering() {
        assert_eq!(
            diagnostics(3, 2, OpenGlProfile::Embedded).video_shader_support(),
            VideoShaderSupport::Unsupported(VideoShaderUnsupportedReason::EmbeddedProfile)
        );
    }
}
