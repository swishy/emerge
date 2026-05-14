//! iOS backend for emerge_skia.
//!
//! Architecture:
//!   - Runs in-process as a NIF thread (unlike macOS which uses an external
//!     binary).
//!   - UIKit objects (UIWindow, UIView, CAMetalLayer) are created on the
//!     main thread once at startup via `dispatch_sync`.
//!   - Rendering (CAMetalLayer.nextDrawable + Skia Metal) runs on the NIF
//!     thread.  CAMetalLayer is explicitly documented as thread-safe on iOS.
//!   - Touch events arrive via UIView callbacks on the main thread and are
//!     forwarded through a global channel to the event actor.
//!   - Keyboard input uses a hidden UITextField whose delegate callbacks
//!     forward text changes as InputEvent::TextCommit.

#![allow(
    deprecated,
    dead_code,
    clippy::nonminimal_bool,
)]

use std::ffi::c_void;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crossbeam_channel::RecvTimeoutError;

use crossbeam_channel::{Receiver, Sender, bounded};

use crate::actors::{EventMsg, TreeMsg};
use crate::events::{ElementEventKind, HostEventSink};
use crate::events::registry_builder::ElixirEventPayload;
use crate::input::InputEvent;
use crate::renderer::{RenderFrame, RenderState, SceneRenderer};
use crate::stats::RendererStatsCollector;
use crate::tree::element::NodeId;
use crate::backend::wake::{BackendWake, BackendWakeHandle, WindowBackendStartupInfo};

use objc2::{
    ClassType, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::{Allocated, Retained},
    runtime::{AnyObject, NSObjectProtocol, ProtocolObject},
};
use objc2_foundation::{NSString, NSRect};
use objc2_ui_kit::{
    UIEvent, UIScreen, UITextField, UITextFieldDelegate,
    UITouch, UIView, UIViewController, UIWindow,
};
use objc2_metal::{
    MTLCommandBuffer, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLDrawable,
    MTLPixelFormat,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use skia_safe::{
    ColorType,
    gpu::{self, SurfaceOrigin, backend_render_targets, mtl},
};

// ============================================================================
// GCD dispatch helpers — trampoline-based (no block support in Rust)
// ============================================================================

/// Concrete job struct that avoids trait objects entirely.
/// The closure type `F` is monomorphized at the call site.
struct SyncJob<F, R> {
    f: Option<F>,
    result: Arc<Mutex<Option<R>>>,
    done: Arc<AtomicBool>,
}

// `_dispatch_main_q` is a struct (dispatch_queue_s), not a pointer.
// In C: `#define dispatch_main_q (&_dispatch_main_q)`.
// We must take its address to get the queue pointer.
unsafe extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(queue: *mut c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

extern "C" fn run_sync_job<F, R>(context: *mut c_void)
where
    F: FnOnce() -> R + Send,
    R: Send,
{
    let job: Box<SyncJob<F, R>> = unsafe { Box::from_raw(context as *mut SyncJob<F, R>) };
    eprintln!("[emerge_skia] run_sync_job: executing");
    if let Some(f) = job.f {
        let r = f();
        *job.result.lock().unwrap() = Some(r);
        eprintln!("[emerge_skia] run_sync_job: f() completed");
    }
    job.done.store(true, Ordering::SeqCst);
    eprintln!("[emerge_skia] run_sync_job: done flag set");
}

fn run_on_main_sync<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    eprintln!("[emerge_skia] run_on_main_sync: entering");
    let result = Arc::new(Mutex::new(None::<R>));
    let done = Arc::new(AtomicBool::new(false));

    let job = Box::new(SyncJob {
        f: Some(f),
        result: result.clone(),
        done: done.clone(),
    });

    eprintln!("[emerge_skia] run_on_main_sync: dispatching async");
    let queue = unsafe { &_dispatch_main_q as *const _ as *mut c_void };
    eprintln!("[emerge_skia] run_on_main_sync: got main queue {:p}", queue);
    unsafe {
        dispatch_async_f(
            queue,
            Box::into_raw(job) as *mut c_void,
            run_sync_job::<F, R>,
        );
    }
    eprintln!("[emerge_skia] run_on_main_sync: waiting for done flag");
    while !done.load(Ordering::SeqCst) {
        std::hint::spin_loop();
    }
    eprintln!("[emerge_skia] run_on_main_sync: done, returning result");
    result.lock().unwrap().take().unwrap()
}

// ============================================================================
// Constants
// ============================================================================

const ACTION_PRESS: u8 = 1;
const ACTION_RELEASE: u8 = 0;

// ============================================================================
// Global state — set up on the main thread on startup, read from the NIF
// thread for the lifetime of the session.  ObjC ref-counting is atomic so
// retaining/releasing across threads is safe.
// ============================================================================

struct IosGlobalState {
    _window: Retained<UIWindow>,
    _view_controller: Retained<UIViewController>,
    content_view: Retained<IosContentView>,
    metal_layer: Retained<CAMetalLayer>,
    metal_device: Retained<ProtocolObject<dyn MTLDevice>>,
    command_queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    skia_context: Mutex<gpu::DirectContext>,
    scale: f32,
}

unsafe impl Send for IosGlobalState {}
unsafe impl Sync for IosGlobalState {}

static IOS_STATE: Mutex<Option<IosGlobalState>> = Mutex::new(None);

fn set_ios_state(state: IosGlobalState) {
    *IOS_STATE.lock().unwrap() = Some(state);
}

fn with_ios_state<R>(f: impl FnOnce(&IosGlobalState) -> R) -> Option<R> {
    IOS_STATE.lock().ok().and_then(|guard| {
        guard.as_ref().map(f)
    })
}

fn with_ios_state_mut<R>(f: impl FnOnce(&mut IosGlobalState) -> R) -> Option<R> {
    IOS_STATE.lock().ok().and_then(|mut guard| {
        guard.as_mut().map(f)
    })
}

// ============================================================================
// Event channel — written to from main-thread UIKit callbacks, read by the
// event actor.
// ============================================================================

static IOS_EVENT_TX: Mutex<Option<Sender<EventMsg>>> = Mutex::new(None);

fn set_event_tx(tx: Sender<EventMsg>) {
    if let Ok(mut guard) = IOS_EVENT_TX.lock() {
        *guard = Some(tx);
    }
}

fn send_event(event: EventMsg) {
    if let Ok(guard) = IOS_EVENT_TX.lock()
        && let Some(ref tx) = *guard {
            let _ = tx.try_send(event);
        }
}

// ============================================================================
// Session state — owned by the NIF thread
// ============================================================================

struct IosSession {
    render_state: RenderState,
    renderer: SceneRenderer,
}

// ============================================================================
// ObjC classes
// ============================================================================

// ---- IosContentView: UIView with CAMetalLayer backing, handles touches ---

define_class!(
    #[unsafe(super(UIView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = IosContentViewIvars]
    struct IosContentView;

    impl IosContentView {
        #[unsafe(method_id(initWithFrame:))]
        fn init_with_frame(this: Allocated<Self>, frame: NSRect) -> Retained<Self> {
            let this = this.set_ivars(IosContentViewIvars::default());
            unsafe { msg_send![super(this), initWithFrame: frame] }
        }

        #[unsafe(method(touchesBegan:withEvent:))]
        unsafe fn touches_began(&self, touches: &AnyObject, _event: &UIEvent) {
            unsafe { Self::forward_touches(touches, ACTION_PRESS) };
        }

        #[unsafe(method(touchesMoved:withEvent:))]
        unsafe fn touches_moved(&self, touches: &AnyObject, _event: &UIEvent) {
            unsafe { Self::forward_touch_moves(touches) };
        }

        #[unsafe(method(touchesEnded:withEvent:))]
        unsafe fn touches_ended(&self, touches: &AnyObject, _event: &UIEvent) {
            unsafe { Self::forward_touches(touches, ACTION_RELEASE) };
        }

        #[unsafe(method(touchesCancelled:withEvent:))]
        unsafe fn touches_cancelled(&self, touches: &AnyObject, _event: &UIEvent) {
            unsafe { Self::forward_touches(touches, ACTION_RELEASE) };
        }
    }

    unsafe impl NSObjectProtocol for IosContentView {}
);

#[derive(Default)]
struct IosContentViewIvars {}


impl IosContentView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let view: Retained<Self> = unsafe { msg_send![Self::alloc(mtm), initWithFrame: frame] };
        view.setMultipleTouchEnabled(true);
        view.setOpaque(true);
        view
    }

    /// Forward touch-down / touch-up as CursorButton events.
    unsafe fn forward_touches(touches: &AnyObject, action: u8) {
        let scale = with_ios_state(|s| s.scale).unwrap_or(1.0);
        let enumerator: *mut AnyObject = msg_send![touches, objectEnumerator];
        loop {
            let touch: Option<Retained<UITouch>> = msg_send![enumerator, nextObject];
            let Some(touch) = touch else { break };
            let pt = touch.preciseLocationInView(None);
            let x = pt.x as f32 * scale;
            let y = pt.y as f32 * scale;
            send_event(EventMsg::InputEvent(InputEvent::CursorButton {
                button: "left".to_string(),
                action,
                mods: 0,
                x,
                y,
            }));
            send_event(EventMsg::InputEvent(InputEvent::CursorPos { x, y }));
        }
    }

    /// Forward touch-move as CursorPos events (multi-touch, first touch).
    unsafe fn forward_touch_moves(touches: &AnyObject) {
        let scale = with_ios_state(|s| s.scale).unwrap_or(1.0);
        let enumerator: *mut AnyObject = msg_send![touches, objectEnumerator];
        loop {
            let touch: Option<Retained<UITouch>> = msg_send![enumerator, nextObject];
            let Some(touch) = touch else { break };
            let pt = touch.preciseLocationInView(None);
            send_event(EventMsg::InputEvent(InputEvent::CursorPos {
                x: pt.x as f32 * scale,
                y: pt.y as f32 * scale,
            }));
        }
    }
}

// ---- IosTextInputField: hidden UITextField for keyboard capture ----------

define_class!(
    #[unsafe(super(UITextField))]
    #[thread_kind = MainThreadOnly]
    #[ivars = IosTextInputIvars]
    struct IosTextInputField;

    impl IosTextInputField {
        #[unsafe(method_id(initWithFrame:))]
        fn init_with_frame(this: Allocated<Self>, frame: NSRect) -> Retained<Self> {
            let this = this.set_ivars(IosTextInputIvars {});
            unsafe { msg_send![super(this), initWithFrame: frame] }
        }
    }

    unsafe impl NSObjectProtocol for IosTextInputField {}

    unsafe impl UITextFieldDelegate for IosTextInputField {
        #[unsafe(method(textField:shouldChangeCharactersInRange:replacementString:))]
        unsafe fn text_field_should_change_characters(
            &self,
            _text_field: &UITextField,
            _range: objc2_foundation::NSRange,
            string: &NSString,
        ) -> bool {
            let text = string.to_string();
            if !text.is_empty() {
                send_event(EventMsg::InputEvent(InputEvent::TextCommit { text, mods: 0 }));
            }
            true
        }
    }
);

struct IosTextInputIvars {}

// ============================================================================
// Backend wake — signals the NIF thread
// ============================================================================

struct IosBackendWake {
    redraw_tx: Sender<()>,
    stop_flag: Arc<AtomicBool>,
}

impl BackendWake for IosBackendWake {
    fn request_stop(&self) { self.stop_flag.store(true, Ordering::Relaxed); }
    fn request_redraw(&self) { let _ = self.redraw_tx.try_send(()); }
    fn notify_video_frame(&self) { let _ = self.redraw_tx.try_send(()); }
}

// ============================================================================
// Configuration
// ============================================================================

#[derive(Clone, Debug)]
pub struct IosConfig {
    pub title: String,
    pub width: u32,
    pub height: u32,
}

impl Default for IosConfig {
    fn default() -> Self {
        Self { title: "Emerge".to_string(), width: 800, height: 600 }
    }
}

// ============================================================================
// Public entry point — called from lib.rs when backend == ios
// ============================================================================

pub(crate) struct IosRunArgs {
    pub config: IosConfig,
    pub running_flag: Arc<AtomicBool>,
    pub tree_tx: Sender<TreeMsg>,
    pub event_tx: Sender<EventMsg>,
    pub render_rx: Receiver<crate::actors::RenderMsg>,
    pub close_signal_log: bool,
    pub stats: Option<Arc<RendererStatsCollector>>,
    pub proxy_tx: std::sync::mpsc::Sender<Result<WindowBackendStartupInfo, String>>,
}

pub(crate) fn run(args: IosRunArgs) {
    eprintln!("[emerge_skia] ios::run: entered");
    eprintln!("[emerge_skia] ios::run: args.config title={} size={}x{}", args.config.title, args.config.width, args.config.height);
    // Phase 1: set up UIKit on the main thread
    // AppDelegate runs OTP on a background thread. If that background thread is
    // actually the main thread (unlikely), call directly. Otherwise use dispatch_sync_f
    // to bounce to the main thread — but only from a thread that can safely call it.
    // Since AppDelegate dispatches OTP launch to DispatchQueue.global, this NIF thread
    // is a plain system thread. dispatch_sync_f to the main queue is safe from here.
    let cfg = args.config.clone();
    eprintln!("[emerge_skia] ios::run: calling run_on_main_sync");
    let startup_info = match run_on_main_sync(move || setup_ui(&cfg)) {
        Ok(info) => { eprintln!("[emerge_skia] ios::run: setup_ui succeeded"); info }
        Err(reason) => { eprintln!("[emerge_skia] ios::run: setup_ui failed: {}", reason); let _ = args.proxy_tx.send(Err(reason)); return; }
    };

    // Phase 2: register event channel
    set_event_tx(args.event_tx.clone());

    // Phase 3: signal startup success
    // Extract logical_size before moving startup_info
    let logical_size = (startup_info.width, startup_info.height);
    let startup_scale = startup_info.scale;
    let _ = args.proxy_tx.send(Ok(startup_info));

    // Phase 3b: send initial resize event so tree actor uses screen dimensions
    let _ = args.event_tx.send(EventMsg::InputEvent(InputEvent::Resized {
        width: logical_size.0,
        height: logical_size.1,
        scale_factor: startup_scale,
    }));

    // Phase 4: NIF render thread — receives RenderMsg from tree actor
    let mut session = IosSession {
        render_state: RenderState::default(),
        renderer: SceneRenderer::new(),
    };
    let mut has_scene = false;

    let mut frame_count: u64 = 0;
    let mut last_log = Instant::now();

    eprintln!("[emerge_skia] render loop started");

    while args.running_flag.load(Ordering::Relaxed) {
        // Block for first scene, then poll with 16ms timeout for subsequent frames
        let msg = if has_scene {
            match args.render_rx.recv_timeout(Duration::from_millis(16)) {
                Ok(msg) => Some(msg),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => None,
            }
        } else {
            args.render_rx.recv().ok()
        };

        match msg {
            Some(crate::actors::RenderMsg::Scene { scene, version, animate, .. }) => {
                eprintln!("[emerge_skia] scene received (v{version})");
                session.render_state.scene = *scene;
                session.render_state.render_version = version;
                session.render_state.animate = animate;
                has_scene = true;
            }
            Some(crate::actors::RenderMsg::Stop) => {
                eprintln!("[emerge_skia] stop received");
                break;
            }
            None => {
                if !has_scene {
                    eprintln!("[emerge_skia] channel disconnected before first scene");
                    break;
                }
            }
        }

        if has_scene {
            if let Err(e) = render_and_present(
                logical_size,
                &mut session.renderer,
                &session.render_state,
            ) {
                eprintln!("[emerge_skia] render error: {e}");
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

    eprintln!("[emerge_skia] render loop exited");
}

// ============================================================================
// UI Setup — runs on the main thread
// ============================================================================

fn setup_ui(_config: &IosConfig) -> Result<WindowBackendStartupInfo, String> {
    eprintln!("[emerge_skia] setup_ui: starting");
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| "iOS UI setup requires the main thread".to_string())?;
    eprintln!("[emerge_skia] setup_ui: got main thread marker");
    let screen = UIScreen::mainScreen(mtm);
    if Retained::as_ptr(&screen).is_null() {
        return Err("no main screen".to_string());
    }
    eprintln!("[emerge_skia] setup_ui: got screen");
    let bounds = screen.bounds();
    eprintln!("[emerge_skia] setup_ui: bounds={:?}", bounds);
    let scale = screen.scale();
    eprintln!("[emerge_skia] setup_ui: scale={}", scale);

    // Window
    let window = UIWindow::initWithFrame(UIWindow::alloc(mtm), bounds);
    eprintln!("[emerge_skia] setup_ui: created window");
    window.setWindowLevel(0.0); // UIWindowLevelNormal
    eprintln!("[emerge_skia] setup_ui: set window level");

    // View controller
    let vc = UIViewController::initWithNibName_bundle(UIViewController::alloc(mtm), None, None);
    eprintln!("[emerge_skia] setup_ui: created view controller");
    window.setRootViewController(Some(&vc));
    eprintln!("[emerge_skia] setup_ui: set root view controller");

    // Content view with CAMetalLayer as a sublayer
    let content_view = IosContentView::new(mtm, bounds);
    eprintln!("[emerge_skia] setup_ui: created content view");
    vc.setView(Some(content_view.as_super()));
    eprintln!("[emerge_skia] setup_ui: set view");

    let metal_layer = CAMetalLayer::new();
    eprintln!("[emerge_skia] setup_ui: created metal layer");
    metal_layer.setFrame(bounds);
    let view_layer: Retained<objc2_quartz_core::CALayer> = content_view.as_super().layer();
    view_layer.addSublayer(&metal_layer);
    eprintln!("[emerge_skia] setup_ui: added metal layer sublayer");

    let device = MTLCreateSystemDefaultDevice()
        .ok_or_else(|| "no Metal device available".to_string())?;
    eprintln!("[emerge_skia] setup_ui: got Metal device");
    let command_queue = device.newCommandQueue()
        .ok_or_else(|| "unable to create Metal command queue".to_string())?;
    eprintln!("[emerge_skia] setup_ui: got command queue");

    let pixel_size = screen.nativeBounds().size;
    metal_layer.setDevice(Some(&device));
    eprintln!("[emerge_skia] setup_ui: set device");
    metal_layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
    metal_layer.setFramebufferOnly(false);
    metal_layer.setPresentsWithTransaction(false);
    metal_layer.setDrawableSize(pixel_size);
    metal_layer.setContentsScale(scale);
    eprintln!("[emerge_skia] setup_ui: configured metal layer");

    // Make window visible — matching macOS host's makeKeyAndOrderFront pattern
    let _: () = unsafe { msg_send![&window, makeKeyAndVisible] };
    eprintln!("[emerge_skia] setup_ui: made window visible");

    // Note: Skia DirectContext is created lazily on the NIF thread so we can
    // also recreate it if needed.  We store it there directly.

    // Create Skia context before moving device/command_queue into global state
    let skia_ctx = create_skia_context(&device, &command_queue)
        .map_err(|e| format!("failed to create Skia Metal context: {e}"))?;

    // Build the wake handle
    let (redraw_tx, _) = bounded::<()>(1);
    let stop_flag = Arc::new(AtomicBool::new(false));

    set_ios_state(IosGlobalState {
        _window: window,
        _view_controller: vc,
        content_view,
        metal_layer,
        metal_device: device,
        command_queue,
        skia_context: Mutex::new(skia_ctx),
        scale: scale as f32,
    });

    // Use screen bounds as logical size (fills the display on mobile)
    // Layout engine expects pixel dimensions, not points — so we scale up
    let logical_width = bounds.size.width as u32;
    let logical_height = bounds.size.height as u32;
    let screen_scale = scale as f32;
    // Tree actor constraint uses pixel dimensions, scale should be 1.0
    // when the constraint is already in pixels
    let pixel_width = (logical_width as f32 * screen_scale) as u32;
    let pixel_height = (logical_height as f32 * screen_scale) as u32;

    eprintln!("[emerge_skia] setup_ui: screen width={} height={} scale={}", logical_width, logical_height, screen_scale);

    Ok(WindowBackendStartupInfo {
        wake: BackendWakeHandle::new(IosBackendWake { redraw_tx, stop_flag }),
        prime_video_supported: false,
        width: pixel_width,
        height: pixel_height,
        scale: 1.0,
    })
}

// ============================================================================
// Skia Metal context creation
// ============================================================================

fn create_skia_context(
    device: &Retained<ProtocolObject<dyn MTLDevice>>,
    command_queue: &Retained<ProtocolObject<dyn MTLCommandQueue>>,
) -> Result<gpu::DirectContext, String> {
    let backend = unsafe {
        mtl::BackendContext::new(
            Retained::as_ptr(device) as mtl::Handle,
            Retained::as_ptr(command_queue) as mtl::Handle,
        )
    };
    gpu::direct_contexts::make_metal(&backend, None)
        .ok_or_else(|| "make_metal returned None".to_string())
}

// ============================================================================
// Metal rendering and presentation — runs on the NIF thread (CAMetalLayer is thread-safe)
// ============================================================================

fn render_and_present(
    _logical_size: (u32, u32),
    renderer: &mut SceneRenderer,
    state: &RenderState,
) -> Result<(), String> {
    with_ios_state_mut(|ios_state| {

        let drawable = match ios_state.metal_layer.nextDrawable() {
            Some(d) => d,
            None => return Ok(()),
        };

        let size = ios_state.metal_layer.drawableSize();
        let w = size.width.max(1.0);
        let h = size.height.max(1.0);

        let texture_info = unsafe {
            mtl::TextureInfo::new(Retained::as_ptr(&drawable.texture()) as mtl::Handle)
        };

        let brt = backend_render_targets::make_mtl((w as i32, h as i32), &texture_info);

        let mut skia = ios_state.skia_context.lock().unwrap();

        let mut surface = gpu::surfaces::wrap_backend_render_target(
            &mut skia,
            &brt,
            SurfaceOrigin::TopLeft,
            ColorType::BGRA8888,
            None,
            None,
        ).ok_or_else(|| "wrap_backend_render_target failed".to_string())?;

        let mut frame = RenderFrame::new(&mut surface, Some(&mut skia));
        renderer.render(&mut frame, state);

        let cmd_buffer = ios_state.command_queue.commandBuffer()
            .ok_or_else(|| "commandBuffer failed".to_string())?;

        let drawable_proto: Retained<ProtocolObject<dyn MTLDrawable>> = (&drawable).into();
        cmd_buffer.presentDrawable(&drawable_proto);
        cmd_buffer.commit();
        Ok(())
    }).unwrap_or(Err("iOS backend not initialized".to_string()))
}

// ============================================================================
// HostEventSink — element event forwarding
// ============================================================================

struct IosHostEventSink;

impl HostEventSink for IosHostEventSink {
    fn send_raw_input(&self, event: &InputEvent) {
        send_event(EventMsg::InputEvent(event.clone()));
    }

    fn send_element_event(&self, _id: &NodeId, _kind: ElementEventKind, _payload: Option<&ElixirEventPayload>) {}
}
