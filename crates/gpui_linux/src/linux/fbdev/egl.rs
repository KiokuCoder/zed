//! Minimal EGL bindings for vendor drivers (such as ARM's Mali fbdev blob) whose default
//! native window is the Linux framebuffer itself.

use std::ffi::{CString, c_char, c_void};
use std::ptr;

use anyhow::Result;

type EglInt = i32;
type EglBoolean = u32;
type EglEnum = u32;
type EglHandle = *mut c_void;

const EGL_TRUE: EglBoolean = 1;
const EGL_NONE: EglInt = 0x3038;
const EGL_RED_SIZE: EglInt = 0x3024;
const EGL_GREEN_SIZE: EglInt = 0x3023;
const EGL_BLUE_SIZE: EglInt = 0x3022;
const EGL_ALPHA_SIZE: EglInt = 0x3021;
const EGL_SURFACE_TYPE: EglInt = 0x3033;
const EGL_WINDOW_BIT: EglInt = 0x0004;
const EGL_WIDTH: EglInt = 0x3057;
const EGL_HEIGHT: EglInt = 0x3056;
const EGL_CONTEXT_CLIENT_VERSION: EglInt = 0x3098;
const EGL_OPENGL_ES_API: EglEnum = 0x30A0;

#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetDisplay(display_id: EglHandle) -> EglHandle;
    fn eglInitialize(display: EglHandle, major: *mut EglInt, minor: *mut EglInt) -> EglBoolean;
    fn eglChooseConfig(
        display: EglHandle,
        attributes: *const EglInt,
        configs: *mut EglHandle,
        config_size: EglInt,
        config_count: *mut EglInt,
    ) -> EglBoolean;
    fn eglBindAPI(api: EglEnum) -> EglBoolean;
    fn eglCreateWindowSurface(
        display: EglHandle,
        config: EglHandle,
        window: EglHandle,
        attributes: *const EglInt,
    ) -> EglHandle;
    fn eglCreateContext(
        display: EglHandle,
        config: EglHandle,
        share_context: EglHandle,
        attributes: *const EglInt,
    ) -> EglHandle;
    fn eglMakeCurrent(
        display: EglHandle,
        draw: EglHandle,
        read: EglHandle,
        context: EglHandle,
    ) -> EglBoolean;
    fn eglQuerySurface(
        display: EglHandle,
        surface: EglHandle,
        attribute: EglInt,
        value: *mut EglInt,
    ) -> EglBoolean;
    fn eglSwapInterval(display: EglHandle, interval: EglInt) -> EglBoolean;
    fn eglSwapBuffers(display: EglHandle, surface: EglHandle) -> EglBoolean;
    fn eglDestroyContext(display: EglHandle, context: EglHandle) -> EglBoolean;
    fn eglDestroySurface(display: EglHandle, surface: EglHandle) -> EglBoolean;
    fn eglTerminate(display: EglHandle) -> EglBoolean;
    fn eglGetProcAddress(name: *const c_char) -> *const c_void;
    fn eglGetError() -> EglInt;
}

fn egl_error(call: &str) -> anyhow::Error {
    anyhow::anyhow!("{call} failed with EGL error {:#x}", unsafe {
        eglGetError()
    })
}

/// An OpenGL ES 3 context rendering to the whole framebuffer. The context is current on the
/// thread that created it, which must be the thread that renders.
pub(crate) struct EglWindow {
    display: EglHandle,
    surface: EglHandle,
    context: EglHandle,
    width: u32,
    height: u32,
}

impl EglWindow {
    pub(crate) fn new() -> Result<Self> {
        let display = unsafe { eglGetDisplay(ptr::null_mut()) };
        if display.is_null() {
            return Err(egl_error("eglGetDisplay"));
        }
        let mut window = Self {
            display,
            surface: ptr::null_mut(),
            context: ptr::null_mut(),
            width: 0,
            height: 0,
        };

        let (mut major, mut minor) = (0, 0);
        if unsafe { eglInitialize(display, &mut major, &mut minor) } != EGL_TRUE {
            return Err(egl_error("eglInitialize"));
        }
        log::info!("EGL {major}.{minor} initialized");

        let config_attributes = [
            EGL_SURFACE_TYPE,
            EGL_WINDOW_BIT,
            EGL_RED_SIZE,
            8,
            EGL_GREEN_SIZE,
            8,
            EGL_BLUE_SIZE,
            8,
            EGL_ALPHA_SIZE,
            8,
            EGL_NONE,
        ];
        let mut config = ptr::null_mut();
        let mut config_count = 0;
        let chose_config = unsafe {
            eglChooseConfig(
                display,
                config_attributes.as_ptr(),
                &mut config,
                1,
                &mut config_count,
            )
        };
        if chose_config != EGL_TRUE || config_count == 0 {
            return Err(egl_error("eglChooseConfig"));
        }

        if unsafe { eglBindAPI(EGL_OPENGL_ES_API) } != EGL_TRUE {
            return Err(egl_error("eglBindAPI"));
        }

        // Vendor fbdev drivers treat a null native window as the framebuffer itself.
        window.surface =
            unsafe { eglCreateWindowSurface(display, config, ptr::null_mut(), ptr::null()) };
        if window.surface.is_null() {
            return Err(egl_error("eglCreateWindowSurface"));
        }

        let context_attributes = [EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE];
        window.context = unsafe {
            eglCreateContext(
                display,
                config,
                ptr::null_mut(),
                context_attributes.as_ptr(),
            )
        };
        if window.context.is_null() {
            return Err(egl_error("eglCreateContext"));
        }

        if unsafe { eglMakeCurrent(display, window.surface, window.surface, window.context) }
            != EGL_TRUE
        {
            return Err(egl_error("eglMakeCurrent"));
        }

        let (mut width, mut height) = (0, 0);
        let queried_size = unsafe {
            eglQuerySurface(display, window.surface, EGL_WIDTH, &mut width) == EGL_TRUE
                && eglQuerySurface(display, window.surface, EGL_HEIGHT, &mut height) == EGL_TRUE
        };
        if !queried_size {
            return Err(egl_error("eglQuerySurface"));
        }
        window.width = width.max(1) as u32;
        window.height = height.max(1) as u32;

        if unsafe { eglSwapInterval(display, 1) } != EGL_TRUE {
            log::warn!("{}", egl_error("eglSwapInterval"));
        }

        Ok(window)
    }

    pub(crate) fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub(crate) fn swap_buffers(&self) {
        if unsafe { eglSwapBuffers(self.display, self.surface) } != EGL_TRUE {
            log::error!("{}", egl_error("eglSwapBuffers"));
        }
    }
}

pub(crate) fn get_proc_address(name: &str) -> *const c_void {
    match CString::new(name) {
        Ok(name) => unsafe { eglGetProcAddress(name.as_ptr()) },
        Err(_) => ptr::null(),
    }
}

impl Drop for EglWindow {
    fn drop(&mut self) {
        unsafe {
            eglMakeCurrent(
                self.display,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            if !self.context.is_null() {
                eglDestroyContext(self.display, self.context);
            }
            if !self.surface.is_null() {
                eglDestroySurface(self.display, self.surface);
            }
            eglTerminate(self.display);
        }
    }
}
