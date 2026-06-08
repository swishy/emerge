//! SH-4A fbdev backend for the Clarion QY8605 head unit.
//!
//! Opens /dev/fb0, retrieves screen info via ioctl, mmap's the framebuffer,
//! renders offscreen via Skia raster surface, then blits to the fb mmap
//! on present (with RGB565 conversion for 16bpp framebuffers).
//!
//! This backend:
//! - Does NOT use KMS/DRM (not available on SH-4A)
//! - Does NOT do page flipping (fbdev doesn't support it)
//! - Does NOT handle input (evdev handled by Elixir side)
//! - Renders to an offscreen Skia raster surface, copies to fbdev on present
//! - Supports 16bpp (RGB565) and 32bpp (XRGB8888) framebuffers

use std::ffi::CString;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc::Sender as StartupSender};
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, TrySendError};
use libc;
use skia_safe::{ColorType, ImageInfo, Surface, surfaces};

use crate::actors::{EventMsg, RenderMsg};
use crate::native_log::NativeLogRelay;
use crate::renderer::{RenderFrame, RenderState, SceneRenderer};
use crate::stats::RendererStatsCollector;

// ============================================================================
// Constants
// ============================================================================

/// FBIOGET_VSCREENINFO ioctl command
const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;
/// FBIOGET_FSCREENINFO ioctl command
const FBIOGET_FSCREENINFO: libc::c_ulong = 0x4602;

// ============================================================================
// Configuration
// ============================================================================

#[derive(Clone, Debug)]
pub struct FbdevConfig {
    pub device_path: String,
    pub width: u32,
    pub height: u32,
    pub renderer_cache_config: crate::renderer::RendererCacheConfig,
}

impl Default for FbdevConfig {
    fn default() -> Self {
        Self {
            device_path: "/dev/fb0".to_string(),
            width: 800,
            height: 480,
            renderer_cache_config: crate::renderer::RendererCacheConfig::default(),
        }
    }
}

// ============================================================================
// Screen Info
// ============================================================================

#[derive(Clone, Debug)]
struct FbScreenInfo {
    xres: u32,
    yres: u32,
    bits_per_pixel: u32,
    line_length: u32,
    smem_len: u32,
}

// ============================================================================
// Fbdev Backend
// ============================================================================

pub struct FbdevBackend {
    renderer: SceneRenderer,
    surface: Surface,
    fb_mmap: *mut libc::c_void,
    fb_size: usize,
    screen: FbScreenInfo,
    width: u32,
    height: u32,
}

// SAFETY: FbdevBackend owns raw pointers to mmap'd memory,
// but only uses them from a single thread at a time.
unsafe impl Send for FbdevBackend {}

impl FbdevBackend {
    /// Open the framebuffer device and mmap it.
    pub fn open(config: &FbdevConfig) -> Result<Self, String> {
        let device = CString::new(config.device_path.clone())
            .map_err(|_| "invalid device path".to_string())?;

        let fb_fd = unsafe { libc::open(device.as_ptr(), libc::O_RDWR) };

        if fb_fd < 0 {
            return Err(format!(
                "failed to open {}: {}",
                config.device_path,
                std::io::Error::last_os_error()
            ));
        }

        let screen = Self::read_screen_info(fb_fd)?;
        let fb_size = screen.smem_len as usize;

        let fb_mmap = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                fb_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fb_fd,
                0,
            )
        };

        if fb_mmap == libc::MAP_FAILED {
            unsafe { libc::close(fb_fd) };
            return Err(format!("mmap failed: {}", std::io::Error::last_os_error()));
        }

        // We can close the fd after mmap; the mapping keeps the reference
        unsafe { libc::close(fb_fd) };

        let width = config.width;
        let height = config.height;

        // Create offscreen raster surface for Skia rendering
        let info = ImageInfo::new(
            (width as i32, height as i32),
            ColorType::RGBA8888,
            skia_safe::AlphaType::Premul,
            None,
        );

        let surface = surfaces::raster(&info, None, None)
            .ok_or_else(|| "failed to create fbdev raster surface".to_string())?;

        let renderer = SceneRenderer::new();

        Ok(Self {
            renderer,
            surface,
            fb_mmap,
            fb_size,
            screen,
            width,
            height,
        })
    }

    /// Read framebuffer screen info via ioctl.
    fn read_screen_info(fd: RawFd) -> Result<FbScreenInfo, String> {
        let mut var_info = [0u8; 160];
        let mut fix_info = [0u8; 68];

        let ret = unsafe {
            libc::ioctl(
                fd,
                FBIOGET_VSCREENINFO,
                var_info.as_mut_ptr() as *mut libc::c_void,
            )
        };
        if ret < 0 {
            return Err(format!(
                "FBIOGET_VSCREENINFO failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        let ret = unsafe {
            libc::ioctl(
                fd,
                FBIOGET_FSCREENINFO,
                fix_info.as_mut_ptr() as *mut libc::c_void,
            )
        };
        if ret < 0 {
            return Err(format!(
                "FBIOGET_FSCREENINFO failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        // Parse fb_var_screeninfo (32-bit LE):
        //   offset 0:  xres (u32)
        //   offset 4:  yres (u32)
        //   offset 8:  xres_virtual (u32)
        //   offset 12: yres_virtual (u32)
        //   offset 16: xoffset (u32)
        //   offset 20: yoffset (u32)
        //   offset 24: bits_per_pixel (u32)
        let xres = u32::from_ne_bytes([var_info[0], var_info[1], var_info[2], var_info[3]]);
        let yres = u32::from_ne_bytes([var_info[4], var_info[5], var_info[6], var_info[7]]);
        let bpp = u32::from_ne_bytes([var_info[24], var_info[25], var_info[26], var_info[27]]);

        // Parse fb_fix_screeninfo (32-bit LE):
        //   offset 24: smem_len (u32)
        //   offset 46: line_length (u32)
        let smem_len =
            u32::from_ne_bytes([fix_info[24], fix_info[25], fix_info[26], fix_info[27]]);
        let line_length =
            u32::from_ne_bytes([fix_info[46], fix_info[47], fix_info[48], fix_info[49]]);

        Ok(FbScreenInfo {
            xres,
            yres,
            bits_per_pixel: bpp,
            line_length,
            smem_len,
        })
    }

    /// Render the current scene and flush to framebuffer.
    fn render_frame(&mut self, msg: &RenderMsg) {
        match msg {
            RenderMsg::Scene {
                scene,
                version,
                pipeline_submitted_at,
                pipeline_render_queued_at,
                animate,
                ..
            } => {
                let state = RenderState {
                    scene: *scene.clone(),
                    clear_color: skia_safe::Color::from_argb(0xFF, 0x0A, 0x0F, 0x14), // dark bg
                    render_version: *version,
                    pipeline_submitted_at: *pipeline_submitted_at,
                    pipeline_render_queued_at: *pipeline_render_queued_at,
                    animate: *animate,
                    has_cache_candidates: false,
                };

                let mut frame = RenderFrame::new(&mut self.surface, None);
                self.renderer.render(&mut frame, &state);
                frame.flush();

                self.present();
            }
            RenderMsg::Stop => {}
        }
    }

    /// Blit the offscreen surface pixels to the framebuffer.
    fn present(&mut self) {
        let info = ImageInfo::new(
            (self.width as i32, self.height as i32),
            ColorType::RGBA8888,
            skia_safe::AlphaType::Premul,
            None,
        );

        let mut pixels = vec![0u8; (self.width * self.height * 4) as usize];

        if self
            .surface
            .read_pixels(&info, &mut pixels, (self.width * 4) as usize, (0, 0))
            .is_err()
        {
            return;
        }

        match self.screen.bits_per_pixel {
            32 => {
                // Direct RGBA copy (XRGB8888 framebuffer)
                let line_length = self.screen.line_length as usize;
                let fb = unsafe { std::slice::from_raw_parts_mut(self.fb_mmap as *mut u8, self.fb_size) };

                for y in 0..self.height as usize {
                    let src_offset = y * (self.width as usize) * 4;
                    let dst_offset = y * line_length;
                    let src_slice = &pixels[src_offset..src_offset + (self.width as usize) * 4];
                    let dst_slice = &mut fb[dst_offset..dst_offset + (self.width as usize) * 4];
                    dst_slice.copy_from_slice(src_slice);
                }
            }
            16 => {
                // RGB565 conversion
                let line_length = self.screen.line_length as usize;
                let fb = unsafe { std::slice::from_raw_parts_mut(self.fb_mmap as *mut u8, self.fb_size) };

                for y in 0..self.height as usize {
                    let src_offset = y * (self.width as usize) * 4;
                    let dst_offset = y * line_length;

                    for x in 0..self.width as usize {
                        let src_idx = src_offset + x * 4;
                        let r = pixels[src_idx] as u16;
                        let g = pixels[src_idx + 1] as u16;
                        let b = pixels[src_idx + 2] as u16;

                        // RGB565: RRRRR GGGGGG BBBBB
                        let pixel = ((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3);
                        let dst_idx = dst_offset + x * 2;

                        if dst_idx + 1 < fb.len() {
                            fb[dst_idx] = (pixel & 0xFF) as u8;
                            fb[dst_idx + 1] = ((pixel >> 8) & 0xFF) as u8;
                        }
                    }
                }
            }
            bpp => {
                log::warn!("fbdev: unsupported bpp {bpp}, skipping present");
            }
        }
    }

    /// Clean up resources.
    fn close(&mut self) {
        if !self.fb_mmap.is_null() {
            unsafe {
                libc::munmap(self.fb_mmap, self.fb_size);
            }
            self.fb_mmap = std::ptr::null_mut();
        }
    }
}

impl Drop for FbdevBackend {
    fn drop(&mut self) {
        self.close();
    }
}

// ============================================================================
// Run Loop
// ============================================================================

pub struct FbdevRunConfig {
    pub device_path: String,
    pub width: u32,
    pub height: u32,
    pub renderer_cache_config: crate::renderer::RendererCacheConfig,
}

pub struct FbdevRunContext {
    pub startup_tx: StartupSender<Result<(), String>>,
    pub stop: Arc<AtomicBool>,
    pub running_flag: Arc<AtomicBool>,
    pub tree_tx: Sender<crate::actors::TreeMsg>,
    pub render_rx: Receiver<RenderMsg>,
    pub event_tx: Sender<EventMsg>,
    pub render_counter: Arc<AtomicU64>,
    pub native_log: Arc<NativeLogRelay>,
    pub stats: Option<Arc<RendererStatsCollector>>,
}

/// Run the fbdev backend main loop.
///
/// Opens the framebuffer, maps it, and processes render messages
/// until a Stop signal is received.
pub fn run(context: FbdevRunContext, config: FbdevRunConfig) {
    let FbdevRunContext {
        startup_tx,
        stop,
        running_flag,
        tree_tx: _,
        render_rx,
        event_tx: _,
        render_counter: _,
        native_log,
        stats: _,
    } = context;

    let fbdev_config = FbdevConfig {
        device_path: config.device_path.clone(),
        width: config.width,
        height: config.height,
        renderer_cache_config: config.renderer_cache_config,
    };

    let mut backend = match FbdevBackend::open(&fbdev_config) {
        Ok(b) => b,
        Err(e) => {
            let _ = startup_tx.send(Err(e));
            running_flag.store(false, Ordering::SeqCst);
            return;
        }
    };

    let _ = startup_tx.send(Ok(()));
    running_flag.store(true, Ordering::SeqCst);

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }

        match render_rx.recv_timeout(std::time::Duration::from_millis(16)) {
            Ok(msg) => {
                backend.render_frame(&msg);

                if matches!(msg, RenderMsg::Stop) {
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // No frame to render — continue
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                break;
            }
        }
    }

    backend.close();
    running_flag.store(false, Ordering::SeqCst);
}
