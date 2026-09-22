//! Zero-copy frame → RGBA texture: AHardwareBuffer → EGLImage → `GL_TEXTURE_EXTERNAL_OES`
//! (the GPU converts YUV) → rotate/mirror pass into one of two RGBA textures that Slint shows as
//! borrowed textures.
//!
//! Runs on Slint's Skia GL context inside `BeforeRendering`. Skia keeps a cache of GL state and
//! does not reset it afterwards, so all state touched here is saved and restored.

use std::ffi::{CStr, c_void};
use std::num::NonZeroU32;
use std::ptr::{null, null_mut};

use ndk_gl_sys::*;

use crate::Error;

const VERTEX_SHADER: &CStr = c"#version 300 es
uniform int u_turns;
uniform bool u_mirror;
out vec2 v_uv;
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
    vec2 c = p - 0.5;
    for (int i = 0; i < u_turns; i++) c = vec2(c.y, -c.x);
    vec2 uv = c + 0.5;
    if (u_mirror) uv.x = 1.0 - uv.x;
    v_uv = uv;
}";

const FRAGMENT_SHADER: &CStr = c"#version 300 es
#extension GL_OES_EGL_image_external_essl3 : require
precision mediump float;
uniform samplerExternalOES u_tex;
in vec2 v_uv;
out vec4 o;
void main() { o = vec4(texture(u_tex, v_uv).rgb, 1.0); }";

/// Fullscreen triangle generated from `gl_VertexID`.
const TRIANGLE_VERTICES: GLsizei = 3;
const TEXTURE_UNIT: GLint = 0;
const OUTPUTS: usize = 2;
pub const TURNS_PER_REVOLUTION: i32 = 4;
const INFO_LOG_CAPACITY: usize = 1024;
const CAPABILITIES: [u32; 5] = [GL_SCISSOR_TEST, GL_BLEND, GL_DEPTH_TEST, GL_STENCIL_TEST, GL_CULL_FACE];

/// A frame rendered into one of the preview's output textures.
#[derive(Clone, Copy)]
pub struct Frame {
    pub texture: NonZeroU32,
    pub width: u32,
    pub height: u32,
}

pub struct Preview {
    program: GLuint,
    u_turns: GLint,
    u_mirror: GLint,
    u_tex: GLint,
    vao: GLuint,
    fbo: GLuint,
    external: GLuint,
    outputs: [NonZeroU32; OUTPUTS],
    output_size: (GLsizei, GLsizei),
    current: usize,
}

fn gl_uint(v: GLint) -> GLuint {
    GLuint::try_from(v).unwrap_or_default()
}

// SAFETY (all fns below): callers guarantee Slint's GL context is current on this thread.

unsafe fn get_int(pname: u32) -> GLint {
    let mut v = 0;
    unsafe { glGetIntegerv(pname, &raw mut v) };
    v
}

struct SavedState {
    draw_fbo: GLint,
    read_fbo: GLint,
    viewport: [GLint; 4],
    program: GLint,
    active_texture: GLint,
    texture_2d: GLint,
    texture_external: GLint,
    sampler: GLint,
    vao: GLint,
    unpack_buffer: GLint,
    color_mask: [GLboolean; 4],
    capabilities: [(GLenum, GLboolean); CAPABILITIES.len()],
}

impl SavedState {
    unsafe fn capture() -> Self {
        unsafe {
            let active_texture = get_int(GL_ACTIVE_TEXTURE);
            glActiveTexture(GL_TEXTURE0);
            let mut viewport = [0; 4];
            glGetIntegerv(GL_VIEWPORT, viewport.as_mut_ptr());
            let mut color_mask = [0; 4];
            glGetBooleanv(GL_COLOR_WRITEMASK, color_mask.as_mut_ptr());
            Self {
                draw_fbo: get_int(GL_DRAW_FRAMEBUFFER_BINDING),
                read_fbo: get_int(GL_READ_FRAMEBUFFER_BINDING),
                viewport,
                program: get_int(GL_CURRENT_PROGRAM),
                active_texture,
                texture_2d: get_int(GL_TEXTURE_BINDING_2D),
                texture_external: get_int(GL_TEXTURE_BINDING_EXTERNAL_OES),
                sampler: get_int(GL_SAMPLER_BINDING),
                vao: get_int(GL_VERTEX_ARRAY_BINDING),
                unpack_buffer: get_int(GL_PIXEL_UNPACK_BUFFER_BINDING),
                color_mask,
                capabilities: CAPABILITIES.map(|c| (c, glIsEnabled(c))),
            }
        }
    }

    unsafe fn restore(&self) {
        unsafe {
            glBindFramebuffer(GL_DRAW_FRAMEBUFFER, gl_uint(self.draw_fbo));
            glBindFramebuffer(GL_READ_FRAMEBUFFER, gl_uint(self.read_fbo));
            let [x, y, w, h] = self.viewport;
            glViewport(x, y, w, h);
            glUseProgram(gl_uint(self.program));
            glActiveTexture(GL_TEXTURE0);
            glBindTexture(GL_TEXTURE_2D, gl_uint(self.texture_2d));
            glBindTexture(GL_TEXTURE_EXTERNAL_OES, gl_uint(self.texture_external));
            glBindSampler(gl_uint(TEXTURE_UNIT), gl_uint(self.sampler));
            glActiveTexture(gl_uint(self.active_texture));
            glBindVertexArray(gl_uint(self.vao));
            glBindBuffer(GL_PIXEL_UNPACK_BUFFER, gl_uint(self.unpack_buffer));
            let [r, g, b, a] = self.color_mask;
            glColorMask(r, g, b, a);
            for (cap, enabled) in self.capabilities {
                if enabled == 0 { glDisable(cap) } else { glEnable(cap) }
            }
        }
    }
}

unsafe fn compile(kind: u32, source: &CStr) -> Result<GLuint, Error> {
    unsafe {
        let shader = glCreateShader(kind);
        let sources = [source.as_ptr()];
        glShaderSource(shader, sources.len() as GLsizei, sources.as_ptr(), null());
        glCompileShader(shader);
        let mut ok = 0;
        glGetShaderiv(shader, GL_COMPILE_STATUS, &raw mut ok);
        if ok == 0 {
            let mut log = vec![0u8; INFO_LOG_CAPACITY];
            let mut len = 0;
            glGetShaderInfoLog(shader, log.len() as GLsizei, &raw mut len, log.as_mut_ptr().cast());
            log.truncate(usize::try_from(len).unwrap_or_default());
            glDeleteShader(shader);
            return Err(Error::Shader(String::from_utf8_lossy(&log).into_owned()));
        }
        Ok(shader)
    }
}

impl Preview {
    /// # Safety
    /// Slint's GL context must be current (call from the rendering notifier).
    pub unsafe fn new() -> Result<Self, Error> {
        unsafe {
            let saved = SavedState::capture();
            let vs = compile(GL_VERTEX_SHADER, VERTEX_SHADER)?;
            let fs = compile(GL_FRAGMENT_SHADER, FRAGMENT_SHADER)?;
            let program = glCreateProgram();
            glAttachShader(program, vs);
            glAttachShader(program, fs);
            glLinkProgram(program);
            glDeleteShader(vs);
            glDeleteShader(fs);
            let mut linked = 0;
            glGetProgramiv(program, GL_LINK_STATUS, &raw mut linked);
            if linked == 0 {
                glDeleteProgram(program);
                return Err(Error::Shader("program link failed".into()));
            }
            let mut ids = [0; OUTPUTS];
            glGenTextures(OUTPUTS as GLsizei, ids.as_mut_ptr());
            let valid: Vec<NonZeroU32> = ids.into_iter().filter_map(NonZeroU32::new).collect();
            let Ok(outputs) = <[NonZeroU32; OUTPUTS]>::try_from(valid) else {
                glDeleteTextures(OUTPUTS as GLsizei, ids.as_ptr());
                glDeleteProgram(program);
                saved.restore();
                return Err(Error::GlObject);
            };
            let mut preview = Self {
                program,
                u_turns: glGetUniformLocation(program, c"u_turns".as_ptr()),
                u_mirror: glGetUniformLocation(program, c"u_mirror".as_ptr()),
                u_tex: glGetUniformLocation(program, c"u_tex".as_ptr()),
                vao: 0,
                fbo: 0,
                external: 0,
                outputs,
                output_size: (0, 0),
                current: 0,
            };
            glGenVertexArrays(1, &raw mut preview.vao);
            glGenFramebuffers(1, &raw mut preview.fbo);
            glGenTextures(1, &raw mut preview.external);
            saved.restore();
            Ok(preview)
        }
    }

    /// Deletes all GL objects.
    ///
    /// # Safety
    /// Slint's GL context must be current (call from `RenderingTeardown`).
    pub unsafe fn destroy(self) {
        let textures = [self.outputs.map(NonZeroU32::get).as_slice(), &[self.external]].concat();
        unsafe {
            glDeleteTextures(textures.len() as GLsizei, textures.as_ptr());
            glDeleteFramebuffers(1, &raw const self.fbo);
            glDeleteVertexArrays(1, &raw const self.vao);
            glDeleteProgram(self.program);
        }
    }

    unsafe fn ensure_output_size(&mut self, width: GLsizei, height: GLsizei) {
        if self.output_size == (width, height) {
            return;
        }
        unsafe {
            // A bound PBO would turn the null pixel pointer into an offset.
            glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0);
            for texture in self.outputs {
                glBindTexture(GL_TEXTURE_2D, texture.get());
                glTexImage2D(
                    GL_TEXTURE_2D,
                    0,
                    GL_RGBA8 as GLint,
                    width,
                    height,
                    0,
                    GL_RGBA,
                    GL_UNSIGNED_BYTE,
                    null(),
                );
                glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR as GLint);
                glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR as GLint);
                glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE as GLint);
                glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE as GLint);
            }
        }
        self.output_size = (width, height);
    }

    /// Renders `buffer` (an `AHardwareBuffer*` of `width`x`height`) turned by `turns` quarter
    /// turns into the next output texture.
    ///
    /// # Safety
    /// Slint's GL context must be current and `buffer` must stay valid until the next frame.
    pub unsafe fn draw(
        &mut self,
        buffer: *mut c_void,
        width: u32,
        height: u32,
        turns: i32,
        mirror: bool,
    ) -> Result<Frame, Error> {
        let turns = turns.rem_euclid(TURNS_PER_REVOLUTION);
        let (out_w, out_h) = if turns % 2 == 1 { (height, width) } else { (width, height) };
        let (Ok(gl_w), Ok(gl_h)) = (GLsizei::try_from(out_w), GLsizei::try_from(out_h)) else {
            return Err(Error::Gl { call: "frame size", code: GL_INVALID_VALUE });
        };
        unsafe {
            let display = eglGetCurrentDisplay();
            let client_buffer = eglGetNativeClientBufferANDROID(buffer.cast());
            if client_buffer.is_null() {
                return Err(Error::Egl { call: "eglGetNativeClientBufferANDROID", code: eglGetError() });
            }
            let attrs = [EGL_IMAGE_PRESERVED_KHR as EGLint, EGL_TRUE as EGLint, EGL_NONE as EGLint];
            let image = eglCreateImageKHR(display, null_mut(), EGL_NATIVE_BUFFER_ANDROID, client_buffer, attrs.as_ptr());
            if image.is_null() {
                return Err(Error::Egl { call: "eglCreateImageKHR", code: eglGetError() });
            }

            let saved = SavedState::capture();
            self.ensure_output_size(gl_w, gl_h);
            let next = (self.current + 1) % OUTPUTS;
            let target = self.outputs[next];

            glBindTexture(GL_TEXTURE_EXTERNAL_OES, self.external);
            glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MIN_FILTER, GL_LINEAR as GLint);
            glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MAG_FILTER, GL_LINEAR as GLint);
            glEGLImageTargetTexture2DOES(GL_TEXTURE_EXTERNAL_OES, image);
            glBindSampler(gl_uint(TEXTURE_UNIT), 0);

            glBindFramebuffer(GL_FRAMEBUFFER, self.fbo);
            glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, target.get(), 0);
            let status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
            let result = if status == GL_FRAMEBUFFER_COMPLETE {
                for cap in CAPABILITIES {
                    glDisable(cap);
                }
                glColorMask(GL_TRUE as GLboolean, GL_TRUE as GLboolean, GL_TRUE as GLboolean, GL_TRUE as GLboolean);
                glViewport(0, 0, gl_w, gl_h);
                glUseProgram(self.program);
                glUniform1i(self.u_tex, TEXTURE_UNIT);
                glUniform1i(self.u_turns, turns);
                glUniform1i(self.u_mirror, GLint::from(mirror));
                glBindVertexArray(self.vao);
                glDrawArrays(GL_TRIANGLES, 0, TRIANGLE_VERTICES);
                self.current = next;
                Ok(Frame { texture: target, width: out_w, height: out_h })
            } else {
                Err(Error::Gl { call: "glCheckFramebufferStatus", code: status })
            };
            saved.restore();
            // The texture keeps its own reference to the image storage.
            eglDestroyImageKHR(display, image);
            match glGetError() {
                GL_NO_ERROR => result,
                code => Err(Error::Gl { call: "preview draw", code }),
            }
        }
    }
}
