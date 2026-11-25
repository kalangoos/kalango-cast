//! A headless OpenGL context on the GPU (EGL device platform, no window, no display server) and the few GL calls
//! the video path needs: importing a DMA-BUF as a texture (no copy: the texture *is* the compositor's buffer),
//! uploading a shared-memory frame, and scaling/cropping one texture into another on the GPU.
//!
//! libEGL is opened at run time (dlopen) so the engine starts on machines without a GPU; every GL function comes from
//! `eglGetProcAddress`. All calls happen on the thread that made the context current (the capture thread).

// The transmute target type is the (extern fn) type of the field being filled; spelling out every signature twice adds nothing.
#![allow(clippy::missing_transmute_annotations)]

use std::ffi::{c_char, c_void, CStr};
use std::ptr;

use cast_core::{fourcc, modifier, DmaBufFrame, Rect};
use libloading::Library;

pub type EGLDisplay = *mut c_void;
pub type EGLContext = *mut c_void;
pub type EGLImage = *mut c_void;
pub type EGLDeviceEXT = *mut c_void;
type EGLint = i32;
type EGLBoolean = u32;
type EGLenum = u32;
type GLuint = u32;
type GLint = i32;
type GLenum = u32;
type GLsizei = i32;

const EGL_NONE: EGLint = 0x3038;
const EGL_EXTENSIONS: EGLint = 0x3055;
const EGL_WIDTH: EGLint = 0x3057;
const EGL_HEIGHT: EGLint = 0x3056;
const EGL_OPENGL_API: EGLenum = 0x30A2;
const EGL_CONTEXT_MAJOR_VERSION: EGLint = 0x3098;
const EGL_CONTEXT_MINOR_VERSION: EGLint = 0x30FB;
const EGL_CONTEXT_OPENGL_PROFILE_MASK: EGLint = 0x30FD;
const EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT: EGLint = 1;
const EGL_PLATFORM_DEVICE_EXT: EGLenum = 0x313F;
const EGL_DRM_RENDER_NODE_FILE_EXT: EGLint = 0x3377;
const EGL_LINUX_DMA_BUF_EXT: EGLenum = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: EGLint = 0x3271;
const EGL_IMAGE_PRESERVED_KHR: EGLint = 0x30D2;
/// Per plane: fd, offset, pitch, modifier low, modifier high.
const EGL_PLANE_ATTRIBS: [[EGLint; 5]; 4] = [
    [0x3272, 0x3273, 0x3274, 0x3443, 0x3444],
    [0x3275, 0x3276, 0x3277, 0x3445, 0x3446],
    [0x3278, 0x3279, 0x327A, 0x3447, 0x3448],
    [0x3440, 0x3441, 0x3442, 0x3449, 0x344A],
];

pub const GL_TEXTURE_2D: GLenum = 0x0DE1;
const GL_RGBA8: GLenum = 0x8058;
const GL_BGRA: GLenum = 0x80E1;
const GL_RGBA: GLenum = 0x1908;
const GL_UNSIGNED_BYTE: GLenum = 0x1401;
const GL_UNPACK_ROW_LENGTH: GLenum = 0x0CF2;
const GL_PACK_ROW_LENGTH: GLenum = 0x0D02;
const GL_PACK_ALIGNMENT: GLenum = 0x0D05;
const GL_UNPACK_ALIGNMENT: GLenum = 0x0CF5;
const GL_TEXTURE_MIN_FILTER: GLenum = 0x2801;
const GL_TEXTURE_MAG_FILTER: GLenum = 0x2800;
const GL_LINEAR: GLint = 0x2601;
const GL_READ_FRAMEBUFFER: GLenum = 0x8CA8;
const GL_DRAW_FRAMEBUFFER: GLenum = 0x8CA9;
const GL_COLOR_ATTACHMENT0: GLenum = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: GLenum = 0x8CD5;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_RENDERER: GLenum = 0x1F01;

/// Loads a function pointer type from a library by name; None if missing.
macro_rules! sym {
    ($lib:expr, $name:literal) => {{
        // SAFETY: the symbol is looked up with the exact C signature declared by the field it is assigned to.
        unsafe { $lib.get(concat!($name, "\0").as_bytes()).ok().map(|s: libloading::Symbol<_>| *s) }
    }};
}

struct Egl {
    _lib: Library,
    get_proc_address: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    initialize: unsafe extern "C" fn(EGLDisplay, *mut EGLint, *mut EGLint) -> EGLBoolean,
    terminate: unsafe extern "C" fn(EGLDisplay) -> EGLBoolean,
    query_string: unsafe extern "C" fn(EGLDisplay, EGLint) -> *const c_char,
    bind_api: unsafe extern "C" fn(EGLenum) -> EGLBoolean,
    create_context: unsafe extern "C" fn(EGLDisplay, *mut c_void, EGLContext, *const EGLint) -> EGLContext,
    destroy_context: unsafe extern "C" fn(EGLDisplay, EGLContext) -> EGLBoolean,
    make_current: unsafe extern "C" fn(EGLDisplay, *mut c_void, *mut c_void, EGLContext) -> EGLBoolean,
    get_error: unsafe extern "C" fn() -> EGLint,
    // extensions, from eglGetProcAddress
    query_devices: unsafe extern "C" fn(EGLint, *mut EGLDeviceEXT, *mut EGLint) -> EGLBoolean,
    query_device_string: unsafe extern "C" fn(EGLDeviceEXT, EGLint) -> *const c_char,
    get_platform_display: unsafe extern "C" fn(EGLenum, *mut c_void, *const EGLint) -> EGLDisplay,
    create_image: unsafe extern "C" fn(EGLDisplay, EGLContext, EGLenum, *mut c_void, *const EGLint) -> EGLImage,
    destroy_image: unsafe extern "C" fn(EGLDisplay, EGLImage) -> EGLBoolean,
    query_dmabuf_modifiers: Option<unsafe extern "C" fn(EGLDisplay, EGLint, EGLint, *mut u64, *mut EGLBoolean, *mut EGLint) -> EGLBoolean>,
}

#[allow(non_snake_case)]
struct Gl {
    GenTextures: unsafe extern "C" fn(GLsizei, *mut GLuint),
    DeleteTextures: unsafe extern "C" fn(GLsizei, *const GLuint),
    BindTexture: unsafe extern "C" fn(GLenum, GLuint),
    TexParameteri: unsafe extern "C" fn(GLenum, GLenum, GLint),
    TexStorage2D: unsafe extern "C" fn(GLenum, GLsizei, GLenum, GLsizei, GLsizei),
    TexSubImage2D: unsafe extern "C" fn(GLenum, GLint, GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, *const c_void),
    PixelStorei: unsafe extern "C" fn(GLenum, GLint),
    GenFramebuffers: unsafe extern "C" fn(GLsizei, *mut GLuint),
    DeleteFramebuffers: unsafe extern "C" fn(GLsizei, *const GLuint),
    BindFramebuffer: unsafe extern "C" fn(GLenum, GLuint),
    FramebufferTexture2D: unsafe extern "C" fn(GLenum, GLenum, GLenum, GLuint, GLint),
    CheckFramebufferStatus: unsafe extern "C" fn(GLenum) -> GLenum,
    BlitFramebuffer: unsafe extern "C" fn(GLint, GLint, GLint, GLint, GLint, GLint, GLint, GLint, u32, GLenum),
    ReadPixels: unsafe extern "C" fn(GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, *mut c_void),
    Finish: unsafe extern "C" fn(),
    Flush: unsafe extern "C" fn(),
    GetError: unsafe extern "C" fn() -> GLenum,
    GetString: unsafe extern "C" fn(GLenum) -> *const u8,
    EGLImageTargetTexture2DOES: unsafe extern "C" fn(GLenum, EGLImage),
}

/// A texture imported from a DMA-BUF, kept while its buffer comes back (PipeWire cycles a few buffers).
struct Import {
    fd: i32,
    inode: u64,
    width: u32,
    height: u32,
    modifier: u64,
    image: EGLImage,
    texture: GLuint,
}

/// The GL context and the frame operations on it.
pub struct GlContext {
    egl: Egl,
    gl: Gl,
    display: EGLDisplay,
    context: EGLContext,
    /// The render node of the GPU (`/dev/dri/renderD128`).
    pub render_node: String,
    pub renderer: String,
    imports: Vec<Import>,
    upload: Option<(GLuint, u32, u32)>,
    read_fbo: GLuint,
    draw_fbo: GLuint,
}

/// GL could not be set up, or a frame could not be imported.
#[derive(Debug)]
pub struct GlError(pub String);

impl std::fmt::Display for GlError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn err(s: impl Into<String>) -> GlError {
    GlError(s.into())
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: EGL/GL return NUL-terminated static strings; checked for null above.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

impl Egl {
    fn load() -> Result<Egl, GlError> {
        // SAFETY: loading libEGL runs its initialisers, which are designed to be loaded this way (glvnd).
        let lib = unsafe { Library::new("libEGL.so.1") }.map_err(|e| err(format!("libEGL: {e}")))?;
        let get_proc_address: unsafe extern "C" fn(*const c_char) -> *mut c_void =
            sym!(lib, "eglGetProcAddress").ok_or_else(|| err("eglGetProcAddress"))?;
        macro_rules! ext {
            ($name:literal) => {{
                // SAFETY: eglGetProcAddress takes a NUL-terminated name.
                let p = unsafe { get_proc_address(concat!($name, "\0").as_ptr().cast()) };
                if p.is_null() {
                    None
                } else {
                    // SAFETY: the pointer is the extension function named, with the signature of the field it fills.
                    Some(unsafe { std::mem::transmute::<*mut c_void, _>(p) })
                }
            }};
        }
        Ok(Egl {
            initialize: sym!(lib, "eglInitialize").ok_or_else(|| err("eglInitialize"))?,
            terminate: sym!(lib, "eglTerminate").ok_or_else(|| err("eglTerminate"))?,
            query_string: sym!(lib, "eglQueryString").ok_or_else(|| err("eglQueryString"))?,
            bind_api: sym!(lib, "eglBindAPI").ok_or_else(|| err("eglBindAPI"))?,
            create_context: sym!(lib, "eglCreateContext").ok_or_else(|| err("eglCreateContext"))?,
            destroy_context: sym!(lib, "eglDestroyContext").ok_or_else(|| err("eglDestroyContext"))?,
            make_current: sym!(lib, "eglMakeCurrent").ok_or_else(|| err("eglMakeCurrent"))?,
            get_error: sym!(lib, "eglGetError").ok_or_else(|| err("eglGetError"))?,
            query_devices: ext!("eglQueryDevicesEXT").ok_or_else(|| err("EGL_EXT_device_enumeration"))?,
            query_device_string: ext!("eglQueryDeviceStringEXT").ok_or_else(|| err("EGL_EXT_device_query"))?,
            get_platform_display: ext!("eglGetPlatformDisplayEXT").ok_or_else(|| err("EGL_EXT_platform_base"))?,
            create_image: ext!("eglCreateImageKHR").ok_or_else(|| err("EGL_KHR_image_base"))?,
            destroy_image: ext!("eglDestroyImageKHR").ok_or_else(|| err("EGL_KHR_image_base"))?,
            query_dmabuf_modifiers: ext!("eglQueryDmaBufModifiersEXT"),
            get_proc_address,
            _lib: lib,
        })
    }
}

impl Gl {
    fn load(egl: &Egl) -> Result<Gl, GlError> {
        macro_rules! f {
            ($name:literal) => {{
                // SAFETY: eglGetProcAddress takes a NUL-terminated name.
                let p = unsafe { (egl.get_proc_address)(concat!($name, "\0").as_ptr().cast()) };
                if p.is_null() {
                    return Err(err(concat!("GL: ", $name)));
                }
                // SAFETY: the pointer is the GL function named, with the signature of the field it fills.
                unsafe { std::mem::transmute::<*mut c_void, _>(p) }
            }};
        }
        Ok(Gl {
            GenTextures: f!("glGenTextures"),
            DeleteTextures: f!("glDeleteTextures"),
            BindTexture: f!("glBindTexture"),
            TexParameteri: f!("glTexParameteri"),
            TexStorage2D: f!("glTexStorage2D"),
            TexSubImage2D: f!("glTexSubImage2D"),
            PixelStorei: f!("glPixelStorei"),
            GenFramebuffers: f!("glGenFramebuffers"),
            DeleteFramebuffers: f!("glDeleteFramebuffers"),
            BindFramebuffer: f!("glBindFramebuffer"),
            FramebufferTexture2D: f!("glFramebufferTexture2D"),
            CheckFramebufferStatus: f!("glCheckFramebufferStatus"),
            BlitFramebuffer: f!("glBlitFramebuffer"),
            ReadPixels: f!("glReadPixels"),
            Finish: f!("glFinish"),
            Flush: f!("glFlush"),
            GetError: f!("glGetError"),
            GetString: f!("glGetString"),
            EGLImageTargetTexture2DOES: f!("glEGLImageTargetTexture2DOES"),
        })
    }
}

/// The inode of an fd: PipeWire may hand the same buffer under another fd number, and a closed fd number may be
/// reused by another buffer; (fd, inode) identifies a buffer.
fn inode(fd: i32) -> u64 {
    // SAFETY: fstat writes only the stat struct we pass; an invalid fd just fails.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::fstat(fd, &mut st) } == 0 {
        st.st_ino
    } else {
        0
    }
}

impl GlContext {
    /// A desktop GL 4.5 core context on the GPU whose render node is `render_node` (or the first GPU with one), made
    /// current on this thread.
    pub fn new(render_node: Option<&str>) -> Result<GlContext, GlError> {
        let egl = Egl::load()?;
        let mut devices = [ptr::null_mut(); 16];
        let mut n = 0;
        // SAFETY: the array has room for 16 devices and n receives how many were written.
        if unsafe { (egl.query_devices)(16, devices.as_mut_ptr(), &mut n) } == 0 || n == 0 {
            return Err(err("no EGL device"));
        }
        let mut chosen = None;
        for &dev in &devices[..n as usize] {
            // SAFETY: dev came from eglQueryDevicesEXT; the strings are static or null.
            let node = cstr(unsafe { (egl.query_device_string)(dev, EGL_DRM_RENDER_NODE_FILE_EXT) });
            if node.is_empty() {
                continue;
            }
            if render_node.is_none_or(|r| r == node) {
                chosen = Some((dev, node));
                break;
            }
        }
        let Some((device, node)) = chosen else { return Err(err("no EGL device with a render node")) };
        // SAFETY: a valid device handle and an empty attribute list.
        let display = unsafe { (egl.get_platform_display)(EGL_PLATFORM_DEVICE_EXT, device, [EGL_NONE].as_ptr()) };
        if display.is_null() {
            return Err(err("eglGetPlatformDisplayEXT"));
        }
        // SAFETY: display is valid; version pointers may be null.
        if unsafe { (egl.initialize)(display, ptr::null_mut(), ptr::null_mut()) } == 0 {
            // SAFETY: eglGetError has no preconditions.
            let e = unsafe { (egl.get_error)() };
            return Err(err(format!("eglInitialize: {e:#x}")));
        }
        // SAFETY: display is initialised.
        let exts = cstr(unsafe { (egl.query_string)(display, EGL_EXTENSIONS) });
        for needed in ["EGL_KHR_surfaceless_context", "EGL_EXT_image_dma_buf_import"] {
            if !exts.split(' ').any(|e| e == needed) {
                // SAFETY: display is initialised.
                unsafe { (egl.terminate)(display) };
                return Err(err(format!("{needed} missing")));
            }
        }
        // SAFETY: a plain API switch.
        unsafe { (egl.bind_api)(EGL_OPENGL_API) };
        let attribs = [
            EGL_CONTEXT_MAJOR_VERSION,
            4,
            EGL_CONTEXT_MINOR_VERSION,
            5,
            EGL_CONTEXT_OPENGL_PROFILE_MASK,
            EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,
            EGL_NONE,
        ];
        // SAFETY: no config (EGL_KHR_no_config_context, needed for surfaceless), no share context.
        let context = unsafe { (egl.create_context)(display, ptr::null_mut(), ptr::null_mut(), attribs.as_ptr()) };
        if context.is_null() {
            // SAFETY: display is initialised.
            let e = unsafe { (egl.get_error)() };
            // SAFETY: display is initialised and not used again.
            unsafe { (egl.terminate)(display) };
            return Err(err(format!("eglCreateContext: {e:#x}")));
        }
        // SAFETY: surfaceless make-current (EGL_KHR_surfaceless_context, checked above).
        if unsafe { (egl.make_current)(display, ptr::null_mut(), ptr::null_mut(), context) } == 0 {
            // SAFETY: both handles are valid.
            unsafe {
                (egl.destroy_context)(display, context);
                (egl.terminate)(display);
            }
            return Err(err("eglMakeCurrent"));
        }
        let gl = Gl::load(&egl)?;
        // SAFETY: the context is current; GetString returns a static string.
        let renderer = cstr(unsafe { (gl.GetString)(GL_RENDERER) }.cast());
        let (mut read_fbo, mut draw_fbo) = (0, 0);
        // SAFETY: the context is current; each call writes one name.
        unsafe {
            (gl.GenFramebuffers)(1, &mut read_fbo);
            (gl.GenFramebuffers)(1, &mut draw_fbo);
        }
        Ok(GlContext { egl, gl, display, context, render_node: node, renderer, imports: Vec::new(), upload: None, read_fbo, draw_fbo })
    }

    /// The DMA-BUF modifiers this GPU imports for `fourcc` (and can render from, not external-only).
    pub fn dmabuf_modifiers(&self, fourcc: u32) -> Vec<u64> {
        let Some(query) = self.egl.query_dmabuf_modifiers else { return vec![modifier::LINEAR] };
        let mut n = 0;
        // SAFETY: first call asks only for the count.
        if unsafe { query(self.display, fourcc as EGLint, 0, ptr::null_mut(), ptr::null_mut(), &mut n) } == 0 || n <= 0 {
            return vec![];
        }
        let mut mods = vec![0u64; n as usize];
        let mut external = vec![0u32; n as usize];
        // SAFETY: both arrays have room for n entries.
        unsafe { query(self.display, fourcc as EGLint, n, mods.as_mut_ptr(), external.as_mut_ptr(), &mut n) };
        mods.iter().zip(&external).take(n.max(0) as usize).filter(|(_, ext)| **ext == 0).map(|(m, _)| *m).collect()
    }

    /// A new RGBA8 texture of this size (for the encoder's input).
    pub fn create_texture(&self, width: u32, height: u32) -> GLuint {
        let mut tex = 0;
        // SAFETY: the context is current on this thread (all GlContext methods require it).
        unsafe {
            (self.gl.GenTextures)(1, &mut tex);
            (self.gl.BindTexture)(GL_TEXTURE_2D, tex);
            (self.gl.TexStorage2D)(GL_TEXTURE_2D, 1, GL_RGBA8, width as GLsizei, height as GLsizei);
            (self.gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            (self.gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            (self.gl.BindTexture)(GL_TEXTURE_2D, 0);
        }
        tex
    }

    pub fn delete_texture(&self, tex: GLuint) {
        // SAFETY: the context is current; tex is one of ours.
        unsafe { (self.gl.DeleteTextures)(1, &tex) };
    }

    /// The texture of a DMA-BUF frame (imported on first sight, then reused while the buffer cycles back).
    fn import(&mut self, f: &DmaBufFrame) -> Result<GLuint, GlError> {
        let plane0 = f.planes.first().ok_or_else(|| err("DMA-BUF without planes"))?;
        let ino = inode(plane0.fd);
        if let Some(i) = self
            .imports
            .iter()
            .find(|i| i.fd == plane0.fd && i.inode == ino && i.width == f.width && i.height == f.height && i.modifier == f.modifier)
        {
            return Ok(i.texture);
        }
        let mut attrs: Vec<EGLint> = vec![
            EGL_WIDTH,
            f.width as EGLint,
            EGL_HEIGHT,
            f.height as EGLint,
            EGL_LINUX_DRM_FOURCC_EXT,
            f.fourcc as EGLint,
            EGL_IMAGE_PRESERVED_KHR,
            1,
        ];
        for (p, names) in f.planes.iter().zip(EGL_PLANE_ATTRIBS) {
            attrs.extend_from_slice(&[names[0], p.fd, names[1], p.offset as EGLint, names[2], p.stride as EGLint]);
            if f.modifier != modifier::INVALID {
                attrs.extend_from_slice(&[names[3], (f.modifier & 0xffff_ffff) as u32 as EGLint, names[4], (f.modifier >> 32) as u32 as EGLint]);
            }
        }
        attrs.push(EGL_NONE);
        // SAFETY: the attribute list is NONE-terminated; the fds stay open during the call (the capture owns them).
        let image = unsafe { (self.egl.create_image)(self.display, ptr::null_mut(), EGL_LINUX_DMA_BUF_EXT, ptr::null_mut(), attrs.as_ptr()) };
        if image.is_null() {
            // SAFETY: plain error query.
            return Err(err(format!("eglCreateImage(DMA-BUF): {:#x}", unsafe { (self.egl.get_error)() })));
        }
        let mut tex = 0;
        // SAFETY: the context is current; the image is valid.
        unsafe {
            (self.gl.GenTextures)(1, &mut tex);
            (self.gl.BindTexture)(GL_TEXTURE_2D, tex);
            (self.gl.EGLImageTargetTexture2DOES)(GL_TEXTURE_2D, image);
            (self.gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            (self.gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            (self.gl.BindTexture)(GL_TEXTURE_2D, 0);
        }
        // PipeWire keeps a small pool; past it, forget the oldest (a renegotiation made new buffers)
        if self.imports.len() >= 16 {
            let old = self.imports.remove(0);
            self.drop_import(old);
        }
        self.imports.push(Import { fd: plane0.fd, inode: ino, width: f.width, height: f.height, modifier: f.modifier, image, texture: tex });
        Ok(tex)
    }

    fn drop_import(&self, i: Import) {
        // SAFETY: the context is current; texture and image are ours.
        unsafe {
            (self.gl.DeleteTextures)(1, &i.texture);
            (self.egl.destroy_image)(self.display, i.image);
        }
    }

    /// Forgets every imported buffer (the capture renegotiated: the old fds are gone).
    pub fn forget_imports(&mut self) {
        for i in std::mem::take(&mut self.imports) {
            self.drop_import(i);
        }
    }

    /// Uploads a shared-memory BGRx/BGRA frame into the upload texture (the driver copies it to the GPU).
    fn upload(&mut self, data: &[u8], width: u32, height: u32, stride: u32, fourcc_code: u32) -> Result<GLuint, GlError> {
        if self.upload.is_none_or(|(_, w, h)| w != width || h != height) {
            if let Some((t, _, _)) = self.upload.take() {
                self.delete_texture(t);
            }
            self.upload = Some((self.create_texture(width, height), width, height));
        }
        let (tex, _, _) = self.upload.unwrap_or_default();
        if (stride as usize) * (height as usize) > data.len() || stride < width * 4 {
            return Err(err("short shared-memory frame"));
        }
        let format = match fourcc_code {
            fourcc::XRGB8888 | fourcc::ARGB8888 => GL_BGRA,
            fourcc::XBGR8888 | fourcc::ABGR8888 => GL_RGBA,
            _ => return Err(err("unsupported pixel format")),
        };
        // SAFETY: the context is current; data holds stride × height bytes (checked above), read with that row length.
        unsafe {
            (self.gl.BindTexture)(GL_TEXTURE_2D, tex);
            (self.gl.PixelStorei)(GL_UNPACK_ALIGNMENT, 4);
            (self.gl.PixelStorei)(GL_UNPACK_ROW_LENGTH, (stride / 4) as GLint);
            (self.gl.TexSubImage2D)(GL_TEXTURE_2D, 0, 0, 0, width as GLsizei, height as GLsizei, format, GL_UNSIGNED_BYTE, data.as_ptr().cast());
            (self.gl.PixelStorei)(GL_UNPACK_ROW_LENGTH, 0);
            (self.gl.BindTexture)(GL_TEXTURE_2D, 0);
        }
        Ok(tex)
    }

    /// The GL texture of a frame: a DMA-BUF imported in place, or shared memory uploaded.
    pub fn frame_texture(&mut self, frame: &cast_core::VideoFrame) -> Result<(GLuint, u32, u32), GlError> {
        match frame {
            cast_core::VideoFrame::DmaBuf(f) => Ok((self.import(f)?, f.width, f.height)),
            cast_core::VideoFrame::Shm(f) => Ok((self.upload(f.data, f.width, f.height, f.stride, f.fourcc)?, f.width, f.height)),
        }
    }

    /// Copies `src` (a `src_w` × `src_h` texture), or its `crop` rectangle, scaled into `dst` (`dst_w` × `dst_h`), on
    /// the GPU. GL's origin is bottom-left and the frames' is top-left: both are textures read and written the same
    /// way, so the image is not flipped.
    pub fn blit(&self, src: GLuint, src_w: u32, src_h: u32, crop: Option<Rect>, dst: GLuint, dst_w: u32, dst_h: u32) -> Result<(), GlError> {
        let r = crop.and_then(|c| c.clipped(src_w, src_h)).unwrap_or(Rect { x: 0, y: 0, width: src_w, height: src_h });
        // keep the aspect ratio: letterbox inside the destination (a window that changed size)
        let scale = (dst_w as f64 / r.width as f64).min(dst_h as f64 / r.height as f64);
        let (w, h) = ((r.width as f64 * scale).round() as i32, (r.height as f64 * scale).round() as i32);
        let (x0, y0) = ((dst_w as i32 - w) / 2, (dst_h as i32 - h) / 2);
        // SAFETY: the context is current; textures and framebuffers are ours.
        unsafe {
            let gl = &self.gl;
            (gl.BindFramebuffer)(GL_READ_FRAMEBUFFER, self.read_fbo);
            (gl.FramebufferTexture2D)(GL_READ_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, src, 0);
            (gl.BindFramebuffer)(GL_DRAW_FRAMEBUFFER, self.draw_fbo);
            (gl.FramebufferTexture2D)(GL_DRAW_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, dst, 0);
            if (gl.CheckFramebufferStatus)(GL_READ_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE
                || (gl.CheckFramebufferStatus)(GL_DRAW_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE
            {
                return Err(err("incomplete framebuffer"));
            }
            let (sx0, sy0, sx1, sy1) = (r.x as i32, r.y as i32, (r.x + r.width) as i32, (r.y + r.height) as i32);
            let filter = if w == r.width as i32 && h == r.height as i32 { 0x2600 } else { GL_LINEAR as GLenum };
            (gl.BlitFramebuffer)(sx0, sy0, sx1, sy1, x0, y0, x0 + w, y0 + h, GL_COLOR_BUFFER_BIT, filter);
            (gl.BindFramebuffer)(GL_READ_FRAMEBUFFER, 0);
            (gl.BindFramebuffer)(GL_DRAW_FRAMEBUFFER, 0);
            let e = (gl.GetError)();
            if e != 0 {
                return Err(err(format!("GL error {e:#x}")));
            }
        }
        Ok(())
    }

    /// Reads a texture back to memory as BGRA rows of `width * 4` bytes (for the software encoder).
    pub fn read_bgra(&self, tex: GLuint, width: u32, height: u32, out: &mut Vec<u8>) {
        out.resize(width as usize * height as usize * 4, 0);
        // SAFETY: the context is current; out has room for width × height × 4 bytes.
        unsafe {
            let gl = &self.gl;
            (gl.BindFramebuffer)(GL_READ_FRAMEBUFFER, self.read_fbo);
            (gl.FramebufferTexture2D)(GL_READ_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
            (gl.PixelStorei)(GL_PACK_ALIGNMENT, 4);
            (gl.PixelStorei)(GL_PACK_ROW_LENGTH, 0);
            (gl.ReadPixels)(0, 0, width as GLsizei, height as GLsizei, GL_BGRA, GL_UNSIGNED_BYTE, out.as_mut_ptr().cast());
            (gl.BindFramebuffer)(GL_READ_FRAMEBUFFER, 0);
        }
    }

    /// Submits the queued GL work (the encoder reads the result through its own interop, which waits for it).
    pub fn flush(&self) {
        // SAFETY: the context is current.
        unsafe { (self.gl.Flush)() };
    }

    pub fn finish(&self) {
        // SAFETY: the context is current.
        unsafe { (self.gl.Finish)() };
    }
}

impl Drop for GlContext {
    fn drop(&mut self) {
        self.forget_imports();
        if let Some((t, _, _)) = self.upload.take() {
            self.delete_texture(t);
        }
        // SAFETY: the handles are ours and current on this thread; nothing uses them after this.
        unsafe {
            (self.gl.DeleteFramebuffers)(1, &self.read_fbo);
            (self.gl.DeleteFramebuffers)(1, &self.draw_fbo);
            (self.egl.make_current)(self.display, ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
            (self.egl.destroy_context)(self.display, self.context);
            (self.egl.terminate)(self.display);
        }
    }
}
