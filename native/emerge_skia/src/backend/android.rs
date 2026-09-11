//! Android backend for emerge_skia.
//!
//! Architecture (mirrors ios.rs):
//!   - Runs in-process as a NIF thread
//!   - EGL display/context/surface created from a JNI-provided ANativeWindow
//!   - Skia renders to the EGL window surface via the GL backend
//!   - Touch events arrive via JNI callbacks from the Kotlin activity and are
//!     forwarded through a global channel to the event actor
//!   - The NIF thread handles the render loop; Kotlin manages the UI thread

#![allow(dead_code)]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, LazyLock, Mutex, Once,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};

use crate::actors::{EventMsg, TreeMsg};
use crate::backend::wake::{BackendWake, BackendWakeHandle, WindowBackendStartupInfo};
use crate::events::registry_builder::ElixirEventPayload;
use crate::events::{
    ElementEventKind, HostEventSink, TextInputSession, TextInputSessionCommand, TextInputState,
};
use crate::input::{ACTION_PRESS, ACTION_RELEASE, InputEvent};
use crate::keys::CanonicalKey;
use crate::render_scene::RenderScene;
use crate::renderer::{RenderFrame, RenderState, SceneRenderer};
use crate::stats::RendererStatsCollector;
use crate::tree::element::NodeId;

#[cfg(target_os = "android")]
unsafe extern "C" {
    fn __android_log_write(prio: i32, tag: *const c_char, text: *const c_char) -> i32;
}

#[cfg(target_os = "android")]
#[repr(C)]
struct DlInfo {
    dli_fname: *const c_char,
    dli_fbase: *mut c_void,
    dli_sname: *const c_char,
    dli_saddr: *mut c_void,
}

#[cfg(target_os = "android")]
unsafe extern "C" {
    fn dladdr(addr: *const c_void, info: *mut DlInfo) -> c_int;
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlerror() -> *const c_char;
}

#[cfg(target_os = "android")]
fn promote_self_to_global_scope() {
    static ONCE: Once = Once::new();

    ONCE.call_once(|| {
        const RTLD_NOW: c_int = 2;
        const RTLD_GLOBAL: c_int = 0x100;

        let mut info = DlInfo {
            dli_fname: std::ptr::null(),
            dli_fbase: std::ptr::null_mut(),
            dli_sname: std::ptr::null(),
            dli_saddr: std::ptr::null_mut(),
        };

        let symbol = Java_com_emerge_android_EmergeBridge_nativeStart as *const () as *const c_void;
        let found = unsafe { dladdr(symbol, &mut info) != 0 };

        if !found || info.dli_fname.is_null() {
            android_log("nativeStart: dladdr failed; NIF API symbols may not be globally visible");
            return;
        }

        let path = unsafe { CStr::from_ptr(info.dli_fname) }
            .to_string_lossy()
            .into_owned();
        // Rustler 0.38+ checks this path on Android so it can resolve
        // `enif_*` symbols from the already-loaded app library.
        unsafe {
            std::env::set_var("RUSTLER_BEAM_LIBRARY_PATH", path.as_str());
        }
        let handle = unsafe { dlopen(info.dli_fname, RTLD_NOW | RTLD_GLOBAL) };

        if handle.is_null() {
            let error = unsafe {
                let ptr = dlerror();
                if ptr.is_null() {
                    "unknown dlopen error".to_string()
                } else {
                    CStr::from_ptr(ptr).to_string_lossy().into_owned()
                }
            };
            android_log(&format!(
                "nativeStart: dlopen RTLD_GLOBAL failed for {path}: {error}"
            ));
        } else {
            android_log(&format!(
                "nativeStart: promoted {path} to RTLD_GLOBAL and set RUSTLER_BEAM_LIBRARY_PATH"
            ));
        }
    });
}

#[cfg(target_os = "android")]
fn android_log(message: &str) {
    let tag = CString::new("emerge_skia").unwrap();
    let msg =
        CString::new(message).unwrap_or_else(|_| CString::new("<invalid log message>").unwrap());
    unsafe {
        __android_log_write(4, tag.as_ptr(), msg.as_ptr());
    }
}

#[cfg(not(target_os = "android"))]
fn android_log(message: &str) {
    eprintln!("{message}");
}

#[cfg(target_os = "android")]
fn android_log_with_tag(tag: &str, message: &str) {
    let tag = CString::new(tag).unwrap_or_else(|_| CString::new("emerge_skia").unwrap());
    let msg = CString::new(message.replace('\0', "\\0"))
        .unwrap_or_else(|_| CString::new("<invalid log message>").unwrap());
    unsafe {
        __android_log_write(4, tag.as_ptr(), msg.as_ptr());
    }
}

#[cfg(target_os = "android")]
fn redirect_stdio_to_logcat() {
    static ONCE: Once = Once::new();

    ONCE.call_once(|| {
        redirect_fd_to_logcat(1, "BEAM-stdout");
        redirect_fd_to_logcat(2, "BEAM-stderr");
    });
}

#[cfg(target_os = "android")]
fn redirect_fd_to_logcat(fd: i32, tag: &'static str) {
    let mut fds = [0; 2];
    let pipe_ok = unsafe { libc::pipe(fds.as_mut_ptr()) == 0 };
    if !pipe_ok {
        android_log(&format!("failed to create stdio pipe for fd={fd}"));
        return;
    }

    let dup_ok = unsafe { libc::dup2(fds[1], fd) >= 0 };
    if !dup_ok {
        android_log(&format!("failed to redirect fd={fd}"));
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        return;
    }

    unsafe {
        libc::close(fds[1]);
    }

    std::thread::spawn(move || {
        let read_fd = fds[0];
        let mut buf = [0u8; 512];
        let mut line = Vec::new();

        loop {
            let n =
                unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };

            if n <= 0 {
                if !line.is_empty() {
                    log_stdio_line(tag, &line);
                }
                unsafe {
                    libc::close(read_fd);
                }
                break;
            }

            for byte in &buf[..n as usize] {
                if *byte == b'\n' {
                    log_stdio_line(tag, &line);
                    line.clear();
                } else {
                    line.push(*byte);
                }
            }
        }
    });
}

#[cfg(target_os = "android")]
fn log_stdio_line(tag: &str, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }

    android_log_with_tag(tag, &String::from_utf8_lossy(bytes));
}

#[cfg(not(target_os = "android"))]
fn redirect_stdio_to_logcat() {}

fn find_erts_bin(erl_root: &str) -> Option<PathBuf> {
    let root = Path::new(erl_root);
    let entries = std::fs::read_dir(root).ok()?;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("erts-") {
            let bin = entry.path().join("bin");
            if bin.is_dir() {
                return Some(bin);
            }
        }
    }

    None
}

fn find_latest_release(erl_root: &str) -> Option<PathBuf> {
    let releases = Path::new(erl_root).join("releases");
    let entries = std::fs::read_dir(&releases).ok()?;
    let mut best: Option<(u32, PathBuf)> = None;

    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }

        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(version) = name.parse::<u32>() else {
            continue;
        };

        if best
            .as_ref()
            .map_or(true, |(best_version, _)| version > *best_version)
        {
            best = Some((version, entry.path()));
        }
    }

    best.map(|(_, path)| path)
}

fn collect_pa_paths(erl_root: &str) -> Vec<PathBuf> {
    let lib = Path::new(erl_root).join("lib");
    let Ok(entries) = std::fs::read_dir(lib) else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let ebin = entry.path().join("ebin");
            ebin.is_dir().then_some(ebin)
        })
        .collect()
}

fn cstring_from_path(path: &Path) -> CString {
    CString::new(path.to_string_lossy().as_bytes()).unwrap()
}

#[cfg(target_os = "android")]
fn set_otp_env(root: &str, bindir: &Path) {
    unsafe {
        std::env::set_var("BINDIR", bindir);
        std::env::set_var("ROOTDIR", root);
        std::env::set_var("EMU", "beam.smp");
        std::env::set_var("PROGNAME", "beam.smp");
    }
}

#[cfg(not(target_os = "android"))]
fn set_otp_env(_root: &str, _bindir: &Path) {}

// ============================================================================
// EGL type aliases
// ============================================================================

type EGLDisplay = *mut std::ffi::c_void;
type EGLConfig = *mut std::ffi::c_void;
type EGLContext = *mut std::ffi::c_void;
type EGLSurface = *mut std::ffi::c_void;
type EGLNativeWindowType = *mut std::ffi::c_void;
type EGLint = i32;
type EGLBoolean = i32;
type EGLenum = u32;

// ============================================================================
// EGL constants
// ============================================================================

const EGL_DEFAULT_DISPLAY: *mut std::ffi::c_void = std::ptr::null_mut();
const EGL_NO_DISPLAY: EGLDisplay = std::ptr::null_mut();
const EGL_NO_CONTEXT: EGLContext = std::ptr::null_mut();
const EGL_NO_SURFACE: EGLSurface = std::ptr::null_mut();
const EGL_TRUE: EGLBoolean = 1;
const EGL_FALSE: EGLBoolean = 0;

const EGL_RED_SIZE: EGLint = 0x3024;
const EGL_GREEN_SIZE: EGLint = 0x3023;
const EGL_BLUE_SIZE: EGLint = 0x3022;
const EGL_ALPHA_SIZE: EGLint = 0x3021;
const EGL_DEPTH_SIZE: EGLint = 0x3025;
const EGL_STENCIL_SIZE: EGLint = 0x3026;
const EGL_RENDERABLE_TYPE: EGLint = 0x3040;
const EGL_OPENGL_ES2_BIT: EGLint = 0x0004;
const EGL_SURFACE_TYPE: EGLint = 0x3033;
const EGL_WINDOW_BIT: EGLint = 0x0004;
const EGL_NONE: EGLint = 0x3038;
const EGL_CONTEXT_CLIENT_VERSION: EGLint = 0x3098;
const EGL_SUCCESS: EGLenum = 0x3000;

// ============================================================================
// JNI type aliases
// ============================================================================

type JNIEnvPtr = *mut std::ffi::c_void;
type JClass = *mut std::ffi::c_void;
type JObject = *mut std::ffi::c_void;
type JString = *mut std::ffi::c_void;

// ============================================================================
// EGL + NDK FFI declarations
// ============================================================================

#[cfg(target_os = "android")]
#[link(name = "EGL")]
#[link(name = "android")]
unsafe extern "C" {
    fn eglGetDisplay(display_id: *mut std::ffi::c_void) -> EGLDisplay;
    fn eglInitialize(dpy: EGLDisplay, major: *mut EGLint, minor: *mut EGLint) -> EGLBoolean;
    fn eglChooseConfig(
        dpy: EGLDisplay,
        attrib_list: *const EGLint,
        configs: *mut EGLConfig,
        config_size: EGLint,
        num_config: *mut EGLint,
    ) -> EGLBoolean;
    fn eglCreateWindowSurface(
        dpy: EGLDisplay,
        config: EGLConfig,
        win: EGLNativeWindowType,
        attrib_list: *const EGLint,
    ) -> EGLSurface;
    fn eglCreateContext(
        dpy: EGLDisplay,
        config: EGLConfig,
        share_context: EGLContext,
        attrib_list: *const EGLint,
    ) -> EGLContext;
    fn eglMakeCurrent(
        dpy: EGLDisplay,
        draw: EGLSurface,
        read: EGLSurface,
        ctx: EGLContext,
    ) -> EGLBoolean;
    fn eglSwapBuffers(dpy: EGLDisplay, surface: EGLSurface) -> EGLBoolean;
    fn eglDestroySurface(dpy: EGLDisplay, surface: EGLSurface) -> EGLBoolean;
    fn eglDestroyContext(dpy: EGLDisplay, ctx: EGLContext) -> EGLBoolean;
    fn eglTerminate(dpy: EGLDisplay) -> EGLBoolean;
    fn eglGetError() -> EGLenum;

    fn ANativeWindow_fromSurface(
        env: *mut std::ffi::c_void,
        surface: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void;
    fn ANativeWindow_release(window: *mut std::ffi::c_void);
    fn ANativeWindow_setBuffersGeometry(
        window: *mut std::ffi::c_void,
        width: i32,
        height: i32,
        format: i32,
    ) -> i32;
}

// ============================================================================
// BEAM (OTP) FFI — erl_start boots the Erlang VM
// ============================================================================

unsafe extern "C" {
    pub fn erl_start(argc: i32, argv: *mut *mut c_char) -> i32;
}

// ============================================================================
// Global state — set up once from JNI, read from the NIF thread
// ============================================================================

struct AndroidGlobalState {
    native_window: *mut std::ffi::c_void,
    egl_display: EGLDisplay,
    egl_context: EGLContext,
    egl_surface: EGLSurface,
    skia_context: Mutex<skia_safe::gpu::DirectContext>,
    width: u32,
    height: u32,
    scale: f32,
}

unsafe impl Send for AndroidGlobalState {}
unsafe impl Sync for AndroidGlobalState {}

impl Drop for AndroidGlobalState {
    fn drop(&mut self) {
        unsafe {
            if !self.egl_surface.is_null() {
                eglDestroySurface(self.egl_display, self.egl_surface);
            }
            if !self.egl_context.is_null() {
                eglDestroyContext(self.egl_display, self.egl_context);
            }
            if !self.egl_display.is_null() {
                eglTerminate(self.egl_display);
            }
            if !self.native_window.is_null() {
                ANativeWindow_release(self.native_window);
            }
        }
    }
}

static ANDROID_STATE: Mutex<Option<AndroidGlobalState>> = Mutex::new(None);

fn set_android_state(state: AndroidGlobalState) {
    if let Ok(mut guard) = ANDROID_STATE.lock() {
        *guard = Some(state);
    }
}

fn with_android_state<R>(f: impl FnOnce(&AndroidGlobalState) -> R) -> Option<R> {
    ANDROID_STATE
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(f))
}

fn with_android_state_mut<R>(f: impl FnOnce(&mut AndroidGlobalState) -> R) -> Option<R> {
    ANDROID_STATE
        .lock()
        .ok()
        .and_then(|mut guard| guard.as_mut().map(f))
}

fn take_android_state() -> Option<AndroidGlobalState> {
    ANDROID_STATE.lock().ok().and_then(|mut guard| guard.take())
}

// ============================================================================
// Event channel — written to from main-thread JNI callbacks, read by the
// event actor
// ============================================================================

static ANDROID_EVENT_TX: Mutex<Option<Sender<EventMsg>>> = Mutex::new(None);

fn set_event_tx(tx: Sender<EventMsg>) {
    if let Ok(mut guard) = ANDROID_EVENT_TX.lock() {
        *guard = Some(tx);
    }
}

fn send_event(event: EventMsg) {
    if let Ok(guard) = ANDROID_EVENT_TX.lock()
        && let Some(ref tx) = *guard
    {
        let _ = tx.try_send(event);
    }
}

// ============================================================================
// Soft keyboard / IME — render loop publishes state; UI thread polls via JNI
// ============================================================================

const ANDROID_IME_OP_NONE: i32 = 0;
const ANDROID_IME_OP_SHOW: i32 = 1;
const ANDROID_IME_OP_HIDE: i32 = 2;
const ANDROID_IME_OP_UPDATE: i32 = 3;

/// Key codes shared with Kotlin (`EmergeBridge` / `EmergeImeHost`).
const ANDROID_KEY_ENTER: i32 = 1;
const ANDROID_KEY_BACKSPACE: i32 = 2;

static ANDROID_IME_COMMAND: Mutex<Option<TextInputSessionCommand>> = Mutex::new(None);
static ANDROID_IME_SESSION: LazyLock<Mutex<TextInputSession>> =
    LazyLock::new(|| Mutex::new(TextInputSession::default()));

fn android_ime_op_code(command: &TextInputSessionCommand) -> i32 {
    match command {
        TextInputSessionCommand::Show(_) => ANDROID_IME_OP_SHOW,
        TextInputSessionCommand::Hide => ANDROID_IME_OP_HIDE,
        TextInputSessionCommand::Update(_) => ANDROID_IME_OP_UPDATE,
    }
}

fn queue_android_ime_command(command: TextInputSessionCommand) {
    let session = command.session().cloned().unwrap_or_default();

    if let Ok(mut guard) = ANDROID_IME_SESSION.lock() {
        *guard = session;
    }

    if let Ok(mut guard) = ANDROID_IME_COMMAND.lock() {
        *guard = Some(command);
    }
}

#[derive(Default)]
struct AndroidTextInputSessionSync {
    session: Option<TextInputSession>,
}

impl AndroidTextInputSessionSync {
    fn sync(
        &mut self,
        ime_enabled: bool,
        ime_cursor_area: Option<(f32, f32, f32, f32)>,
        ime_text_state: Option<TextInputState>,
    ) {
        let next_session = ime_text_state
            .as_ref()
            .filter(|state| ime_enabled && state.focused)
            .map(|state| TextInputSession::from_state(state, ime_cursor_area));

        match (&self.session, &next_session) {
            (Some(_), None) => queue_android_ime_command(TextInputSessionCommand::Hide),
            (None, Some(session)) => {
                queue_android_ime_command(TextInputSessionCommand::Show(session.clone()));
            }
            (Some(current), Some(next)) if current != next => {
                queue_android_ime_command(TextInputSessionCommand::Update(next.clone()));
            }
            _ => {}
        }

        self.session = next_session;
    }
}

fn map_android_key(code: i32) -> Option<CanonicalKey> {
    match code {
        ANDROID_KEY_ENTER => Some(CanonicalKey::Enter),
        ANDROID_KEY_BACKSPACE => Some(CanonicalKey::Backspace),
        _ => None,
    }
}

fn publish_surface_resize(width: u32, height: u32, display_scale: f32) {
    let display_scale = if display_scale > 0.0 {
        display_scale
    } else {
        1.0
    };
    let width = width.max(1);
    let height = height.max(1);

    let ready = with_android_state_mut(|state| {
        state.width = width;
        state.height = height;
        state.scale = display_scale;
        unsafe {
            ANativeWindow_setBuffersGeometry(state.native_window, width as i32, height as i32, 1);
        }
    })
    .is_some();

    if ready {
        send_event(EventMsg::InputEvent(InputEvent::resized_physical(
            width,
            height,
            display_scale,
        )));
    }
}

// ============================================================================
// Surface setup channel — JNI writes ANativeWindow info here; run() reads it
// ============================================================================

struct SurfaceInfo {
    native_window: *mut std::ffi::c_void,
    width: u32,
    height: u32,
    scale: f32,
}

unsafe impl Send for SurfaceInfo {}

static ANDROID_SURFACE_TX: Mutex<Option<std::sync::mpsc::Sender<SurfaceInfo>>> = Mutex::new(None);
static ANDROID_PENDING_SURFACE: Mutex<Option<SurfaceInfo>> = Mutex::new(None);

fn clear_surface_tx() {
    if let Ok(mut guard) = ANDROID_SURFACE_TX.lock() {
        *guard = None;
    }
}

fn set_surface_tx(tx: std::sync::mpsc::Sender<SurfaceInfo>) {
    if let Ok(mut guard) = ANDROID_SURFACE_TX.lock() {
        *guard = Some(tx.clone());
    }

    if let Ok(mut pending) = ANDROID_PENDING_SURFACE.lock()
        && let Some(info) = pending.take()
        && let Err(err) = tx.send(info)
    {
        eprintln!("[emerge_skia] set_surface_tx: receiver dropped before pending surface delivery");
        release_surface_info(err.0);
    }
}

fn release_surface_info(info: SurfaceInfo) {
    unsafe {
        ANativeWindow_release(info.native_window);
    }
}

fn publish_surface(info: SurfaceInfo) {
    if let Ok(mut guard) = ANDROID_SURFACE_TX.lock()
        && let Some(ref tx) = *guard
    {
        if let Err(err) = tx.send(info) {
            eprintln!("[emerge_skia] publish_surface: surface receiver dropped; storing pending surface");
            *guard = None;

            if let Ok(mut pending) = ANDROID_PENDING_SURFACE.lock() {
                if let Some(old) = pending.replace(err.0) {
                    release_surface_info(old);
                }
            }
        }
        return;
    }

    eprintln!(
        "[emerge_skia] nativeOnSurfaceCreated: no surface receiver registered; storing pending surface"
    );

    if let Ok(mut pending) = ANDROID_PENDING_SURFACE.lock() {
        if let Some(old) = pending.replace(info) {
            release_surface_info(old);
        }
    }
}

// ============================================================================
// Backend wake — signals the render thread via the render channel
// ============================================================================

struct AndroidBackendWake {
    redraw_tx: Sender<()>,
    stop_flag: Arc<AtomicBool>,
}

impl BackendWake for AndroidBackendWake {
    fn request_stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }

    fn request_redraw(&self) {
        let _ = self.redraw_tx.try_send(());
    }

    fn notify_video_frame(&self) {
        let _ = self.redraw_tx.try_send(());
    }
}

// ============================================================================
// Configuration
// ============================================================================

#[derive(Clone, Debug)]
pub struct AndroidConfig {
    pub title: String,
}

impl Default for AndroidConfig {
    fn default() -> Self {
        Self {
            title: "Emerge".to_string(),
        }
    }
}

// ============================================================================
// EGL setup
// ============================================================================

fn setup_egl(
    native_window: *mut std::ffi::c_void,
    width: i32,
    height: i32,
) -> Result<(EGLDisplay, EGLContext, EGLSurface), String> {
    unsafe {
        // 1. Get EGL display
        let display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
        if display.is_null() {
            return Err("eglGetDisplay failed".to_string());
        }

        // 2. Initialize EGL
        let mut major: EGLint = 0;
        let mut minor: EGLint = 0;
        if eglInitialize(display, &mut major, &mut minor) == EGL_FALSE {
            return Err("eglInitialize failed".to_string());
        }

        // 3. Choose EGL config
        let attrib_list = [
            EGL_RENDERABLE_TYPE,
            EGL_OPENGL_ES2_BIT,
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
            EGL_DEPTH_SIZE,
            0,
            EGL_STENCIL_SIZE,
            0,
            EGL_NONE,
        ];

        let mut config: EGLConfig = std::ptr::null_mut();
        let mut num_config: EGLint = 0;
        if eglChooseConfig(
            display,
            attrib_list.as_ptr(),
            &mut config,
            1,
            &mut num_config,
        ) == EGL_FALSE
            || num_config == 0
        {
            return Err("eglChooseConfig failed".to_string());
        }

        // 4. Set buffer geometry on the native window
        ANativeWindow_setBuffersGeometry(
            native_window,
            width,
            height,
            1, // WINDOW_FORMAT_RGBA_8888
        );

        // 5. Create EGL window surface
        let surface = eglCreateWindowSurface(
            display,
            config,
            native_window as EGLNativeWindowType,
            std::ptr::null(),
        );
        if surface.is_null() {
            return Err("eglCreateWindowSurface failed".to_string());
        }

        // 6. Create EGL context
        let context_attribs = [EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE];
        let context = eglCreateContext(display, config, EGL_NO_CONTEXT, context_attribs.as_ptr());
        if context.is_null() {
            eglDestroySurface(display, surface);
            return Err("eglCreateContext failed".to_string());
        }

        // 7. Make context current
        if eglMakeCurrent(display, surface, surface, context) == EGL_FALSE {
            eglDestroyContext(display, context);
            eglDestroySurface(display, surface);
            return Err("eglMakeCurrent failed".to_string());
        }

        Ok((display, context, surface))
    }
}

// ============================================================================
// Skia GL context creation — must be called with EGL context current
// ============================================================================

fn create_skia_context() -> Result<skia_safe::gpu::DirectContext, String> {
    let interface = skia_safe::gpu::gl::Interface::new_native()
        .ok_or_else(|| "failed to create Skia GL interface".to_string())?;
    skia_safe::gpu::direct_contexts::make_gl(interface, None)
        .ok_or_else(|| "make_gl returned None".to_string())
}

// ============================================================================
// Rendering — runs on the NIF thread
// ============================================================================

fn create_android_state(surface_info: SurfaceInfo) -> Result<AndroidGlobalState, String> {
    let (egl_display, egl_context, egl_surface) = setup_egl(
        surface_info.native_window,
        surface_info.width as i32,
        surface_info.height as i32,
    )?;

    let skia_context = match create_skia_context() {
        Ok(ctx) => ctx,
        Err(reason) => {
            unsafe {
                eglDestroyContext(egl_display, egl_context);
                eglDestroySurface(egl_display, egl_surface);
                eglTerminate(egl_display);
            }
            return Err(reason);
        }
    };

    Ok(AndroidGlobalState {
        native_window: surface_info.native_window,
        egl_display,
        egl_context,
        egl_surface,
        skia_context: Mutex::new(skia_context),
        width: surface_info.width,
        height: surface_info.height,
        scale: surface_info.scale,
    })
}

fn take_pending_surface() -> Option<SurfaceInfo> {
    ANDROID_PENDING_SURFACE
        .lock()
        .ok()
        .and_then(|mut guard| guard.take())
}

fn render_and_present(renderer: &mut SceneRenderer, state: &RenderState) -> Result<(), String> {
    with_android_state_mut(|android_state| {
        let w = android_state.width.max(1) as i32;
        let h = android_state.height.max(1) as i32;

        // Make EGL context current on this thread
        let result = unsafe {
            eglMakeCurrent(
                android_state.egl_display,
                android_state.egl_surface,
                android_state.egl_surface,
                android_state.egl_context,
            )
        };
        if result == EGL_FALSE {
            return Err("eglMakeCurrent failed in render_and_present".to_string());
        }

        // Create GL framebuffer info (FBO 0 = default framebuffer)
        use skia_safe::gpu::gl::{Format, FramebufferInfo};
        let fb_info = FramebufferInfo {
            fboid: 0,
            format: Format::RGBA8.into(),
            protected: skia_safe::gpu::Protected::No,
        };

        // Create backend render target wrapping FBO 0
        use skia_safe::gpu::{SurfaceOrigin, backend_render_targets, surfaces};
        let brt = backend_render_targets::make_gl((w, h), 0, 0, fb_info);

        let mut skia = android_state.skia_context.lock().unwrap();

        let mut surface = surfaces::wrap_backend_render_target(
            &mut skia,
            &brt,
            SurfaceOrigin::BottomLeft,
            skia_safe::ColorType::RGBA8888,
            None,
            None,
        )
        .ok_or_else(|| "wrap_backend_render_target failed".to_string())?;

        let mut frame = RenderFrame::new(&mut surface, Some(&mut skia));
        renderer.render(&mut frame, state);

        // Present
        let result =
            unsafe { eglSwapBuffers(android_state.egl_display, android_state.egl_surface) };
        if result == EGL_FALSE {
            return Err("eglSwapBuffers failed".to_string());
        }

        Ok(())
    })
    .unwrap_or(Err("Android backend not initialized".to_string()))
}

// ============================================================================
// Public entry point — called from lib.rs when backend == android
// ============================================================================

pub(crate) struct AndroidRunArgs {
    pub config: AndroidConfig,
    pub running_flag: Arc<AtomicBool>,
    pub tree_tx: Sender<TreeMsg>,
    pub event_tx: Sender<EventMsg>,
    pub render_rx: Receiver<crate::actors::RenderMsg>,
    pub close_signal_log: bool,
    pub stats: Option<Arc<RendererStatsCollector>>,
    pub proxy_tx: std::sync::mpsc::Sender<Result<WindowBackendStartupInfo, String>>,
}

pub(crate) fn run(args: AndroidRunArgs) {
    // Phase 1: register event channel
    set_event_tx(args.event_tx.clone());

    // Phase 2: set up surface channel and wait for JNI to provide the surface
    let (surface_tx, surface_rx) = std::sync::mpsc::channel::<SurfaceInfo>();
    set_surface_tx(surface_tx);

    // Log that we're waiting for the surface
    eprintln!("[emerge_skia] android::run: waiting for ANativeWindow from JNI");

    let surface_info = match surface_rx.recv() {
        Ok(info) => info,
        Err(_) => {
            eprintln!(
                "[emerge_skia] android::run: surface channel closed before receiving surface"
            );
            let _ = args
                .proxy_tx
                .send(Err("surface channel closed".to_string()));
            return;
        }
    };

    eprintln!(
        "[emerge_skia] android::run: received surface {}x{} scale={}",
        surface_info.width, surface_info.height, surface_info.scale
    );
    clear_surface_tx();

    // Phase 3: create EGL context and surface
    let initial_state = match create_android_state(surface_info) {
        Ok(state) => state,
        Err(reason) => {
            eprintln!("[emerge_skia] android::run: initial surface setup failed: {}", reason);
            let _ = args.proxy_tx.send(Err(reason));
            return;
        }
    };

    // Phase 5: build the wake handle
    let (redraw_tx, _) = bounded::<()>(1);
    let stop_flag = Arc::new(AtomicBool::new(false));

    // Phase 6: store global state
    let pixel_width = initial_state.width;
    let pixel_height = initial_state.height;
    let pixel_scale = initial_state.scale;
    set_android_state(initial_state);

    // Phase 7: signal startup success
    let _ = args.proxy_tx.send(Ok(WindowBackendStartupInfo {
        wake: BackendWakeHandle::new(AndroidBackendWake {
            redraw_tx,
            stop_flag,
        }),
        prime_video_supported: false,
        width: pixel_width,
        height: pixel_height,
        scale: 1.0,
    }));

    // Phase 8: send initial resize event
    let _ = args
        .event_tx
        .send(EventMsg::InputEvent(InputEvent::resized_physical(
            pixel_width,
            pixel_height,
            pixel_scale,
        )));

    // Phase 9: render loop
    let mut session = SceneRenderer::new();
    let mut render_state = RenderState::new(
        RenderScene::default(),
        skia_safe::Color::TRANSPARENT,
        0,
        false,
    );
    let mut has_scene = false;

    let mut frame_count: u64 = 0;
    let mut last_log = Instant::now();
    let mut text_input_sync = AndroidTextInputSessionSync::default();

    eprintln!("[emerge_skia] android::run: render loop started");

    while args.running_flag.load(Ordering::Relaxed) {
        let msg = if has_scene {
            match args.render_rx.recv_timeout(Duration::from_millis(16)) {
                Ok(msg) => Some(msg),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => None,
            }
        } else {
            args.render_rx.recv().ok()
        };

        if let Some(surface_info) = take_pending_surface() {
            if let Some(old_state) = take_android_state() {
                drop(old_state);
            }

            match create_android_state(surface_info) {
                Ok(state) => {
                    let width = state.width;
                    let height = state.height;
                    let scale = state.scale;
                    set_android_state(state);
                    let _ = args.event_tx.send(EventMsg::InputEvent(
                        InputEvent::resized_physical(width, height, scale),
                    ));
                    eprintln!(
                        "[emerge_skia] android::run: rebound surface {}x{} scale={}",
                        width, height, scale
                    );
                }
                Err(reason) => {
                    eprintln!("[emerge_skia] android::run: surface rebind failed: {reason}");
                }
            }
        }

        match msg {
            Some(crate::actors::RenderMsg::Scene {
                scene,
                version,
                animate,
                ime_enabled,
                ime_cursor_area,
                ime_text_state,
                ..
            }) => {
                render_state.set_scene(*scene);
                render_state.render_version = version;
                render_state.animate = animate;
                text_input_sync.sync(ime_enabled, ime_cursor_area, *ime_text_state);
                has_scene = true;
            }
            Some(crate::actors::RenderMsg::Stop) => {
                eprintln!("[emerge_skia] android::run: stop received");
                break;
            }
            None => {
                if !has_scene {
                    eprintln!(
                        "[emerge_skia] android::run: channel disconnected before first scene"
                    );
                    break;
                }
            }
        }

        if has_scene {
            if let Err(e) = render_and_present(&mut session, &render_state) {
                // Don't log on every error to avoid noise — just capture the first
                if frame_count == 0 {
                    eprintln!("[emerge_skia] android::run: render error: {e}");
                }
            } else {
                frame_count += 1;
                if frame_count % 300 == 0 {
                    let elapsed = last_log.elapsed();
                    let fps = 300.0 / elapsed.as_secs_f64();
                    eprintln!("[emerge_skia] rendered {frame_count} frames ({fps:.0} fps)");
                    last_log = Instant::now();
                }
            }
        }
    }

    eprintln!("[emerge_skia] android::run: render loop exited");

    // Cleanup: global state Drop handler frees EGL resources
    take_android_state();
}

// ============================================================================
// JNI exported functions — called from Kotlin
// ============================================================================

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativeOnSurfaceCreated(
    env: JNIEnvPtr,
    _class: JClass,
    surface: JObject,
    width: i32,
    height: i32,
    density: f32,
) {
    eprintln!(
        "[emerge_skia] nativeOnSurfaceCreated: {}x{} density={}",
        width, height, density
    );

    // Convert the Java Surface to an ANativeWindow
    let native_window = unsafe { ANativeWindow_fromSurface(env, surface) };
    if native_window.is_null() {
        eprintln!("[emerge_skia] nativeOnSurfaceCreated: ANativeWindow_fromSurface returned null");
        return;
    }

    publish_surface(SurfaceInfo {
        native_window,
        width: width as u32,
        height: height as u32,
        scale: if density > 0.0 { density } else { 1.0 },
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativeOnSurfaceChanged(
    _env: JNIEnvPtr,
    _class: JClass,
    width: i32,
    height: i32,
    density: f32,
) {
    eprintln!(
        "[emerge_skia] nativeOnSurfaceChanged: {}x{} density={}",
        width, height, density
    );
    publish_surface_resize(width as u32, height as u32, density);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativeOnTouchEvent(
    _env: JNIEnvPtr,
    _class: JClass,
    action: i32,
    x: f32,
    y: f32,
) {
    // Android action constants: ACTION_DOWN=0, ACTION_UP=1, ACTION_MOVE=2
    // Internal constants: ACTION_PRESS=1, ACTION_RELEASE=0
    let mapped_action = match action {
        0 => 1u8, // ACTION_DOWN -> PRESS
        1 => 0u8, // ACTION_UP -> RELEASE
        _ => return,
    };

    send_event(EventMsg::InputEvent(InputEvent::CursorButton {
        button: "left".to_string(),
        action: mapped_action,
        mods: 0,
        x,
        y,
    }));

    if mapped_action == 1 {
        send_event(EventMsg::InputEvent(InputEvent::CursorPos { x, y }));
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativeOnTouchMove(
    _env: JNIEnvPtr,
    _class: JClass,
    x: f32,
    y: f32,
) {
    send_event(EventMsg::InputEvent(InputEvent::CursorPos { x, y }));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativePollImeOp(
    _env: JNIEnvPtr,
    _class: JClass,
) -> i32 {
    ANDROID_IME_COMMAND
        .lock()
        .ok()
        .and_then(|mut guard| guard.take())
        .map(|command| android_ime_op_code(&command))
        .unwrap_or(ANDROID_IME_OP_NONE)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativePollImeContent(
    env: JNIEnvPtr,
    _class: JClass,
) -> JString {
    let content = ANDROID_IME_SESSION
        .lock()
        .ok()
        .map(|guard| guard.content.clone())
        .unwrap_or_default();
    unsafe { jni_new_string_utf(env, &content) }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativePollImeCursor(
    _env: JNIEnvPtr,
    _class: JClass,
) -> i32 {
    ANDROID_IME_SESSION
        .lock()
        .ok()
        .map(|guard| guard.cursor as i32)
        .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativePollImeSelectionAnchor(
    _env: JNIEnvPtr,
    _class: JClass,
) -> i32 {
    ANDROID_IME_SESSION
        .lock()
        .ok()
        .map(|guard| {
            guard
                .selection_anchor
                .map(|anchor| anchor as i32)
                .unwrap_or(-1)
        })
        .unwrap_or(-1)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativePollImePreedit(
    env: JNIEnvPtr,
    _class: JClass,
) -> JString {
    let preedit = ANDROID_IME_SESSION
        .lock()
        .ok()
        .and_then(|guard| guard.preedit.clone())
        .unwrap_or_default();
    unsafe { jni_new_string_utf(env, &preedit) }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativePollImePreeditCursorStart(
    _env: JNIEnvPtr,
    _class: JClass,
) -> i32 {
    ANDROID_IME_SESSION
        .lock()
        .ok()
        .and_then(|guard| guard.preedit_cursor.map(|(start, _)| start as i32))
        .unwrap_or(-1)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativePollImePreeditCursorEnd(
    _env: JNIEnvPtr,
    _class: JClass,
) -> i32 {
    ANDROID_IME_SESSION
        .lock()
        .ok()
        .and_then(|guard| guard.preedit_cursor.map(|(_, ending)| ending as i32))
        .unwrap_or(-1)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativePollImeMultiline(
    _env: JNIEnvPtr,
    _class: JClass,
) -> i32 {
    ANDROID_IME_SESSION
        .lock()
        .ok()
        .map(|guard| if guard.multiline { 1 } else { 0 })
        .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativePollImeAnchorX(
    _env: JNIEnvPtr,
    _class: JClass,
) -> f32 {
    ANDROID_IME_SESSION
        .lock()
        .ok()
        .map(|guard| guard.anchor.x)
        .unwrap_or(0.0)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativePollImeAnchorY(
    _env: JNIEnvPtr,
    _class: JClass,
) -> f32 {
    ANDROID_IME_SESSION
        .lock()
        .ok()
        .map(|guard| guard.anchor.y)
        .unwrap_or(0.0)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativePollImeAnchorW(
    _env: JNIEnvPtr,
    _class: JClass,
) -> f32 {
    ANDROID_IME_SESSION
        .lock()
        .ok()
        .map(|guard| guard.anchor.width)
        .unwrap_or(0.0)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativePollImeAnchorH(
    _env: JNIEnvPtr,
    _class: JClass,
) -> f32 {
    ANDROID_IME_SESSION
        .lock()
        .ok()
        .map(|guard| guard.anchor.height)
        .unwrap_or(0.0)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativeOnTextCommit(
    env: JNIEnvPtr,
    _class: JClass,
    text: JString,
) {
    let text = unsafe { jni_string(env, text) };
    if text.is_empty() {
        return;
    }
    send_event(EventMsg::InputEvent(InputEvent::TextCommit {
        text,
        mods: 0,
    }));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativeOnTextPreedit(
    env: JNIEnvPtr,
    _class: JClass,
    text: JString,
    cursor_start: i32,
    cursor_end: i32,
) {
    let text = unsafe { jni_string(env, text) };
    if text.is_empty() {
        send_event(EventMsg::InputEvent(InputEvent::TextPreeditClear));
        return;
    }

    let cursor = if cursor_start >= 0 && cursor_end >= 0 {
        Some((cursor_start as u32, cursor_end as u32))
    } else {
        None
    };

    send_event(EventMsg::InputEvent(InputEvent::TextPreedit {
        text,
        cursor,
    }));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativeOnTextPreeditClear(
    _env: JNIEnvPtr,
    _class: JClass,
) {
    send_event(EventMsg::InputEvent(InputEvent::TextPreeditClear));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativeOnDeleteSurrounding(
    _env: JNIEnvPtr,
    _class: JClass,
    before_length: i32,
    after_length: i32,
) {
    if before_length < 0 || after_length < 0 {
        return;
    }

    send_event(EventMsg::InputEvent(InputEvent::DeleteSurrounding {
        before_length: before_length as u32,
        after_length: after_length as u32,
    }));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativeOnKey(
    _env: JNIEnvPtr,
    _class: JClass,
    key_code: i32,
    action: i32,
) {
    let Some(key) = map_android_key(key_code) else {
        return;
    };
    let action = match action {
        0 => ACTION_RELEASE,
        1 => ACTION_PRESS,
        _ => return,
    };
    send_event(EventMsg::InputEvent(InputEvent::Key {
        key,
        action,
        mods: 0,
    }));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativeOnSurfaceDestroyed(
    _env: JNIEnvPtr,
    _class: JClass,
) {
    eprintln!("[emerge_skia] nativeOnSurfaceDestroyed");
    if let Ok(mut pending) = ANDROID_PENDING_SURFACE.lock()
        && let Some(info) = pending.take()
    {
        release_surface_info(info);
    }
    take_android_state();
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativeGetRenderWidth(
    _env: JNIEnvPtr,
    _class: JClass,
) -> i32 {
    with_android_state(|s| s.width as i32).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativeGetRenderHeight(
    _env: JNIEnvPtr,
    _class: JClass,
) -> i32 {
    with_android_state(|s| s.height as i32).unwrap_or(0)
}

// ============================================================================
// ============================================================================
// JNI string extraction helper (raw JNIEnv function table access)
// ============================================================================

/// Create a Java String from a UTF-8 Rust string via JNI (NewStringUTF slot 167).
unsafe fn jni_new_string_utf(env: JNIEnvPtr, s: &str) -> JString {
    const NEW_STRING_UTF: usize = 167;

    unsafe {
        let functions: *const *const std::ffi::c_void =
            std::ptr::read_volatile(env as *const *const *const std::ffi::c_void);
        let new_string_utf_ptr: *const std::ffi::c_void =
            std::ptr::read_volatile(functions.add(NEW_STRING_UTF));
        let new_string_utf: extern "system" fn(JNIEnvPtr, *const c_char) -> JString =
            std::mem::transmute(new_string_utf_ptr);

        let c_str = CString::new(s).unwrap_or_default();
        new_string_utf(env, c_str.as_ptr())
    }
}

/// Extract a Rust String from a JNI jstring using the JNIEnv function table.
/// Uses function pointer table indices: GetStringUTFChars=169, ReleaseStringUTFChars=170.
unsafe fn jni_string(env: JNIEnvPtr, jstr: JString) -> String {
    const GET_STRING_UTF_CHARS: usize = 169;
    const RELEASE_STRING_UTF_CHARS: usize = 170;

    unsafe {
        let functions: *const *const std::ffi::c_void =
            std::ptr::read_volatile(env as *const *const *const std::ffi::c_void);

        let get_utf_ptr: *const std::ffi::c_void =
            std::ptr::read_volatile(functions.add(GET_STRING_UTF_CHARS));
        let release_utf_ptr: *const std::ffi::c_void =
            std::ptr::read_volatile(functions.add(RELEASE_STRING_UTF_CHARS));
        let get_utf: extern "system" fn(JNIEnvPtr, JString, *mut u8) -> *const c_char =
            std::mem::transmute(get_utf_ptr);
        let release_utf: extern "system" fn(JNIEnvPtr, JString, *const c_char) =
            std::mem::transmute(release_utf_ptr);

        let utf_ptr = get_utf(env, jstr, std::ptr::null_mut());
        if utf_ptr.is_null() {
            return String::new();
        }

        let s = CStr::from_ptr(utf_ptr).to_string_lossy().into_owned();
        release_utf(env, jstr, utf_ptr);
        s
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_emerge_android_EmergeBridge_nativeStart(
    env: JNIEnvPtr,
    _class: JClass,
    erl_root: JString,
    eval_expr: JString,
) {
    android_log("nativeStart: entered");
    promote_self_to_global_scope();
    let root = unsafe { jni_string(env, erl_root) };
    android_log(&format!("nativeStart: erl_root={root}"));
    let eval = unsafe { jni_string(env, eval_expr) };
    android_log(&format!("nativeStart: eval bytes={}", eval.len()));
    redirect_stdio_to_logcat();

    let Some(erts_bin) = find_erts_bin(&root) else {
        eprintln!("nativeStart: could not find erts-*/bin under {root}");
        return;
    };

    let Some(release_dir) = find_latest_release(&root) else {
        eprintln!("nativeStart: could not find releases/<vsn> under {root}");
        return;
    };

    let boot_prefix = release_dir.join("start_clean");
    let boot_file = release_dir.join("start_clean.boot");
    if !boot_file.is_file() {
        eprintln!(
            "nativeStart: boot script not found: {}",
            boot_file.to_string_lossy()
        );
        return;
    }

    set_otp_env(&root, &erts_bin);

    let home = std::env::var("HOME")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            Path::new(&root)
                .parent()
                .map(|path| path.to_string_lossy().into_owned())
        });

    let pa_paths = collect_pa_paths(&root);

    // Match the working iOS embedded-OTP launch contract:
    // set OTP env, boot the bundled release, add all app ebin paths, then eval.
    // erl_start normally never returns (runs BEAM VM forever).
    let mut args = vec![
        CString::new("beam.smp").unwrap(),
        CString::new("--").unwrap(),
        CString::new("-sbwt").unwrap(),
        CString::new("none").unwrap(),
        CString::new("-noshell").unwrap(),
        CString::new("-root").unwrap(),
        CString::new(root.as_str()).unwrap(),
        CString::new("-bindir").unwrap(),
        cstring_from_path(&erts_bin),
        CString::new("-setcookie").unwrap(),
        CString::new("emerge_ios").unwrap(),
    ];

    if let Some(home) = home {
        args.push(CString::new("-home").unwrap());
        args.push(CString::new(home).unwrap());
    }

    args.extend([
        CString::new("-no_epmd").unwrap(),
        CString::new("-dist_listen").unwrap(),
        CString::new("false").unwrap(),
        CString::new("-boot").unwrap(),
        cstring_from_path(&boot_prefix),
    ]);

    for path in &pa_paths {
        args.push(CString::new("-pa").unwrap());
        args.push(cstring_from_path(path));
    }

    if !eval.is_empty() {
        args.push(CString::new("-eval").unwrap());
        args.push(CString::new(eval.as_str()).unwrap());
    }

    let argc = args.len() as i32;
    let mut argv: Vec<*mut c_char> = args
        .iter_mut()
        .map(|arg| arg.as_ptr() as *mut c_char)
        .collect();
    argv.push(std::ptr::null_mut());

    android_log(&format!("nativeStart: calling erl_start argc={argc}"));
    eprintln!("nativeStart: {argc} args, root={root}");
    for (idx, arg) in args.iter().enumerate() {
        eprintln!("  [{idx}] {}", arg.to_string_lossy());
    }

    unsafe {
        erl_start(argc, argv.as_mut_ptr());
    }

    // If erl_start returns, something went wrong
    android_log("nativeStart: erl_start returned unexpectedly");
    eprintln!("[emerge_skia] nativeStart: erl_start returned unexpectedly");
}

// ============================================================================
// HostEventSink — element event forwarding
// ============================================================================

struct AndroidHostEventSink;

impl HostEventSink for AndroidHostEventSink {
    fn send_raw_input(&self, event: &InputEvent) {
        send_event(EventMsg::InputEvent(event.clone()));
    }

    fn send_element_event(
        &self,
        _id: &NodeId,
        _kind: ElementEventKind,
        _payload: Option<&ElixirEventPayload>,
    ) {
    }
}
