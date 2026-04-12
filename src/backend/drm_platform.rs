//! DRM KMS platform for Slint — renders the Slint UI into DRM dumb buffers
//! on a leased CRTC. Uses Slint's software renderer (no GPU acceleration).
//!
//! The bottom panel (DP-1) is 1080×1620 portrait native. We tell Slint the
//! window is 1620×1080 (landscape) and rotate 90° CW when writing to the
//! DRM framebuffer.
//!
//! Touch input is read from the Goodix evdev device and dispatched to Slint.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::rc::Rc;
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Instant;

use crate::types::stats::SystemSnapshot;

/// Shared stats snapshot: written by the stats thread, read by the render loop.
static STATS_SNAPSHOT: OnceLock<Arc<Mutex<Option<SystemSnapshot>>>> = OnceLock::new();

/// Called from main.rs after creating the App to register the shared snapshot.
pub fn set_stats_snapshot(snap: Arc<Mutex<Option<SystemSnapshot>>>) {
    let _ = STATS_SNAPSHOT.set(snap);
}

/// Callback to apply a snapshot to the Slint UI. Set from main.rs.
/// Uses thread_local because App/StatsStore are !Send (Rc-based).
thread_local! {
    static STATS_APPLY: std::cell::RefCell<Option<Box<dyn Fn(SystemSnapshot)>>> =
        std::cell::RefCell::new(None);
}

/// Register the callback that applies stats to Slint globals.
/// Must be called from the same thread that runs the event loop.
pub fn set_stats_callback(cb: Box<dyn Fn(SystemSnapshot)>) {
    STATS_APPLY.with(|slot| *slot.borrow_mut() = Some(cb));
}

use drm::buffer::Buffer;
use drm::control::Device as ControlDevice;

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, TargetPixel,
};
use slint::platform::{Platform, WindowAdapter};
use slint::PhysicalSize;

// ── DRM wrapper ─────────────────────────────────────────────────────────

struct LeaseCard(OwnedFd);

impl AsFd for LeaseCard {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl AsRawFd for LeaseCard {
    fn as_raw_fd(&self) -> i32 {
        self.0.as_raw_fd()
    }
}
impl drm::Device for LeaseCard {}
impl drm::control::Device for LeaseCard {}

// ── Pixel type for Slint → DRM XRGB8888 ────────────────────────────────

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct Xrgb8888Pixel {
    b: u8,
    g: u8,
    r: u8,
    x: u8,
}

impl TargetPixel for Xrgb8888Pixel {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        let a = color.alpha as u16;
        let inv = 255 - a;
        self.r = ((self.r as u16 * inv + color.red as u16 * 255) / 255) as u8;
        self.g = ((self.g as u16 * inv + color.green as u16 * 255) / 255) as u8;
        self.b = ((self.b as u16 * inv + color.blue as u16 * 255) / 255) as u8;
        self.x = 0xFF;
    }

    fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self { b, g, r, x: 0xFF }
    }
}

// ── Framebuffer wrapper ─────────────────────────────────────────────────

struct DrmFb {
    db: drm::control::dumbbuffer::DumbBuffer,
    fb: drm::control::framebuffer::Handle,
}

// ── Touch events from evdev thread ──────────────────────────────────────

enum TouchEvent {
    Down { x: f32, y: f32 },
    Move { x: f32, y: f32 },
    Up,
}

/// Find the Goodix touchscreen evdev device and spawn a reader thread.
/// Coordinates are rotated to landscape (logical) space.
fn spawn_touch_thread(
    landscape_w: u32,
    landscape_h: u32,
) -> Option<mpsc::Receiver<TouchEvent>> {
    // Find the Goodix device
    let mut goodix_path = None;
    for entry in std::fs::read_dir("/dev/input").ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if !path.to_str().map_or(false, |s| s.contains("event")) {
            continue;
        }
        if let Ok(dev) = evdev::Device::open(&path) {
            if let Some(name) = dev.name() {
                if name.contains("Goodix") {
                    eprintln!("[touch] found Goodix at {:?}: {}", path, name);
                    goodix_path = Some(path);
                    break;
                }
            }
        }
    }

    let path = goodix_path?;
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let dev = match evdev::Device::open(&path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[touch] failed to open {:?}: {}", path, e);
                return;
            }
        };

        // Get ABS ranges for coordinate normalization
        let mut x_max: f32 = 1080.0;
        let mut y_max: f32 = 1620.0;
        if let Ok(absinfo) = dev.get_absinfo() {
            for (code, info) in absinfo {
                match code {
                    evdev::AbsoluteAxisCode::ABS_X => {
                        x_max = info.maximum() as f32;
                        eprintln!("[touch] ABS_X max = {}", info.maximum());
                    }
                    evdev::AbsoluteAxisCode::ABS_Y => {
                        y_max = info.maximum() as f32;
                        eprintln!("[touch] ABS_Y max = {}", info.maximum());
                    }
                    _ => {}
                }
            }
        }

        eprintln!(
            "[touch] reading events (panel range: 0..{} x 0..{})",
            x_max, y_max
        );

        let mut cur_x: f32 = 0.0;
        let mut cur_y: f32 = 0.0;
        let mut is_down = false;

        // We need a mutable ref for fetch_events
        let mut dev = dev;

        loop {
            match dev.fetch_events() {
                Ok(events) => {
                    for ev in events {
                        match ev.event_type() {
                            evdev::EventType::ABSOLUTE => {
                                let code = evdev::AbsoluteAxisCode(ev.code());
                                if code == evdev::AbsoluteAxisCode::ABS_X {
                                    cur_x = ev.value() as f32;
                                } else if code == evdev::AbsoluteAxisCode::ABS_Y {
                                    cur_y = ev.value() as f32;
                                }
                            }
                            evdev::EventType::KEY => {
                                // BTN_TOUCH = 0x14a
                                if ev.code() == 0x14a {
                                    // Rotate panel portrait → landscape 90° CW:
                                    //   lx = panel_y / y_max * landscape_w
                                    //   ly = (1 - panel_x / x_max) * landscape_h
                                    let lx =
                                        (cur_y / y_max) * landscape_w as f32;
                                    let ly = (1.0 - cur_x / x_max)
                                        * landscape_h as f32;

                                    if ev.value() == 1 {
                                        is_down = true;
                                        eprintln!("[touch] DOWN raw=({},{}) logical=({:.0},{:.0})", cur_x, cur_y, lx, ly);
                                        let _ = tx.send(TouchEvent::Down {
                                            x: lx,
                                            y: ly,
                                        });
                                    } else {
                                        is_down = false;
                                        eprintln!("[touch] UP");
                                        let _ = tx.send(TouchEvent::Up);
                                    }
                                }
                            }
                            evdev::EventType::SYNCHRONIZATION => {
                                if is_down {
                                    let lx = (cur_y / y_max)
                                        * landscape_w as f32;
                                    let ly = (1.0 - cur_x / x_max)
                                        * landscape_h as f32;
                                    let _ = tx.send(TouchEvent::Move {
                                        x: lx,
                                        y: ly,
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[touch] read error: {e}");
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        }
    });

    Some(rx)
}

// ── Platform impl ───────────────────────────────────────────────────────

pub struct DrmPlatform {
    window: Rc<MinimalSoftwareWindow>,
    card: LeaseCard,
    connector: drm::control::connector::Handle,
    crtc: drm::control::crtc::Handle,
    mode: drm::control::Mode,
    /// Physical DRM framebuffer dimensions (portrait: 1080×1620)
    phys_width: u32,
    phys_height: u32,
    /// Logical landscape dimensions (1620×1080) — what Slint sees
    logical_width: u32,
    logical_height: u32,
    start: Instant,
    touch_rx: Option<mpsc::Receiver<TouchEvent>>,
}

impl DrmPlatform {
    pub fn new(lease_fd: OwnedFd) -> Result<Self, String> {
        let card = LeaseCard(lease_fd);

        let res = card
            .resource_handles()
            .map_err(|e| format!("resource_handles: {e}"))?;

        if res.connectors().is_empty() || res.crtcs().is_empty() {
            return Err("lease has no connectors or CRTCs".into());
        }

        let connector = res.connectors()[0];
        let crtc = res.crtcs()[0];

        let conn_info = card
            .get_connector(connector, false)
            .map_err(|e| format!("get_connector: {e}"))?;

        if conn_info.modes().is_empty() {
            return Err("connector has no modes".into());
        }

        let mode = conn_info.modes()[0];
        let phys_width = mode.size().0 as u32; // 1080
        let phys_height = mode.size().1 as u32; // 1620

        // Landscape: swap dimensions for Slint
        let logical_width = phys_height; // 1620
        let logical_height = phys_width; // 1080

        eprintln!(
            "[drm-platform] physical: {}x{}@{}Hz, logical (landscape): {}x{}",
            phys_width,
            phys_height,
            mode.vrefresh(),
            logical_width,
            logical_height
        );

        let window = MinimalSoftwareWindow::new(
            slint::platform::software_renderer::RepaintBufferType::ReusedBuffer,
        );
        window.set_size(PhysicalSize::new(logical_width, logical_height));

        // Spawn touch input thread
        let touch_rx = spawn_touch_thread(logical_width, logical_height);
        if touch_rx.is_some() {
            eprintln!("[drm-platform] touch input enabled");
        } else {
            eprintln!("[drm-platform] warning: no touch input device found");
        }

        Ok(Self {
            window,
            card,
            connector,
            crtc,
            mode,
            phys_width,
            phys_height,
            logical_width,
            logical_height,
            start: Instant::now(),
            touch_rx,
        })
    }

    fn create_fb(&self) -> Result<DrmFb, String> {
        let db = self
            .card
            .create_dumb_buffer(
                (self.phys_width, self.phys_height),
                drm_fourcc::DrmFourcc::Xrgb8888,
                32,
            )
            .map_err(|e| format!("create_dumb_buffer: {e}"))?;
        let fb = self
            .card
            .add_framebuffer(&db, 24, 32)
            .map_err(|e| format!("add_framebuffer: {e}"))?;
        Ok(DrmFb { db, fb })
    }
}

impl Platform for DrmPlatform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> core::time::Duration {
        self.start.elapsed()
    }

    fn run_event_loop(&self) -> Result<(), slint::PlatformError> {
        let mut fb0 = self
            .create_fb()
            .map_err(|e| slint::PlatformError::Other(e.into()))?;
        let mut fb1 = self
            .create_fb()
            .map_err(|e| slint::PlatformError::Other(e.into()))?;

        // Landscape-sized buffer for Slint to render into.
        let logical_pixels =
            (self.logical_width * self.logical_height) as usize;
        let mut render_buf = vec![Xrgb8888Pixel::default(); logical_pixels];

        // Initial mode-set
        self.card
            .set_crtc(
                self.crtc,
                Some(fb0.fb),
                (0, 0),
                &[self.connector],
                Some(self.mode),
            )
            .map_err(|e| {
                slint::PlatformError::Other(format!("set_crtc: {e}").into())
            })?;

        let mut front = false;
        let drm_stride = (fb0.db.pitch() / 4) as usize;
        let lw = self.logical_width as usize;  // 1620
        let lh = self.logical_height as usize; // 1080

        let mut is_pressed = false;

        eprintln!(
            "[drm-platform] entering render loop \
             (logical {}x{}, drm stride={})",
            lw, lh, drm_stride
        );

        loop {
            // ── Process touch events ────────────────────────────────
            if let Some(ref rx) = self.touch_rx {
                while let Ok(ev) = rx.try_recv() {
                    match ev {
                        TouchEvent::Down { x, y } => {
                            is_pressed = true;
                            self.window.dispatch_event(
                                slint::platform::WindowEvent::PointerPressed {
                                    position: slint::LogicalPosition::new(
                                        x, y,
                                    ),
                                    button:
                                        slint::platform::PointerEventButton::Left,
                                },
                            );
                        }
                        TouchEvent::Move { x, y } => {
                            if is_pressed {
                                self.window.dispatch_event(
                                    slint::platform::WindowEvent::PointerMoved {
                                        position:
                                            slint::LogicalPosition::new(x, y),
                                    },
                                );
                            }
                        }
                        TouchEvent::Up => {
                            is_pressed = false;
                            self.window.dispatch_event(
                                slint::platform::WindowEvent::PointerReleased {
                                    position: slint::LogicalPosition::new(
                                        0.0, 0.0,
                                    ),
                                    button:
                                        slint::platform::PointerEventButton::Left,
                                },
                            );
                        }
                    }
                }
            }
            // ── Poll shared stats and push to Slint via callback ────
            if let Some(shared) = STATS_SNAPSHOT.get() {
                let snap = shared.lock().unwrap().take();
                if let Some(snap) = snap {
                    STATS_APPLY.with(|slot| {
                        if let Some(ref cb) = *slot.borrow() {
                            cb(snap);
                        }
                    });
                }
            }
            // ── Render ──────────────────────────────────────────────
            slint::platform::update_timers_and_animations();

            // Always request redraw — MinimalSoftwareWindow doesn't
            // auto-invalidate when property bindings change.
            self.window.request_redraw();

            if self.window.draw_if_needed(|renderer| {
                // 1. Render into the landscape buffer
                renderer.render_by_line(LandscapeLineBuffer {
                    buf: &mut render_buf,
                    stride: lw,
                });

                // 2. Rotate 90° CW into the DRM framebuffer.
                //    Landscape (lx, ly) → Portrait (px, py):
                //      px = ly
                //      py = (lw - 1) - lx
                let target = if front { &mut fb0 } else { &mut fb1 };
                let mut map =
                    self.card.map_dumb_buffer(&mut target.db).unwrap();
                let drm_buf: &mut [Xrgb8888Pixel] = unsafe {
                    std::slice::from_raw_parts_mut(
                        map.as_mut().as_mut_ptr() as *mut Xrgb8888Pixel,
                        map.as_mut().len() / 4,
                    )
                };

                for ly in 0..lh {
                    for lx in 0..lw {
                        let px = ly;
                        let py = (lw - 1) - lx;
                        let src_idx = ly * lw + lx;
                        let dst_idx = py * drm_stride + px;
                        drm_buf[dst_idx] = render_buf[src_idx];
                    }
                }
            }) {
                let show = if front { &fb0 } else { &fb1 };
                let _ = self.card.set_crtc(
                    self.crtc,
                    Some(show.fb),
                    (0, 0),
                    &[self.connector],
                    Some(self.mode),
                );
                front = !front;
            }

            // ~60 FPS — the bottom screen is 60 Hz
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
    }
}

// ── Slint renders landscape lines into a flat buffer ────────────────────

struct LandscapeLineBuffer<'a> {
    buf: &'a mut [Xrgb8888Pixel],
    stride: usize,
}

impl<'a> slint::platform::software_renderer::LineBufferProvider
    for LandscapeLineBuffer<'a>
{
    type TargetPixel = Xrgb8888Pixel;

    fn process_line(
        &mut self,
        line: usize,
        range: core::ops::Range<usize>,
        render_fn: impl FnOnce(&mut [Self::TargetPixel]),
    ) {
        let start = line * self.stride + range.start;
        let end = line * self.stride + range.end;
        if end <= self.buf.len() {
            render_fn(&mut self.buf[start..end]);
        }
    }
}
