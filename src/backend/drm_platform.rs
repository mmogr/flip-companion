//! DRM KMS platform for Slint — GPU-accelerated rendering with Wayland
//! compositor integration for the AYANEO Flip DS bottom screen.
//!
//! The bottom panel (DP-1) is 1080×1620 portrait native. We tell Slint the
//! window is 1620×1080 (landscape) and use GPU rotation (Transform::_270)
//! when rendering to the DRM framebuffer.
//!
//! Layout (landscape 1620×1080):
//!   Top 40px    — Slint status bar
//!   Middle 984px — Wayland client window area
//!   Bottom 56px  — Slint navigation bar
//!
//! Touch input Y-split: touches in the status/nav regions go to Slint,
//! touches in the middle go to the Wayland client via the compositor.

use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::compositor::{self, CompositorCommand, GrabCommand, SurfaceLayer, BufferData, RenderWaker};
use crate::types::stats::SystemSnapshot;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{Frame, ImportDma, ImportMem, Renderer, Texture};
use smithay::utils::{Buffer as BufferCoord, Physical, Rectangle, Size, Transform};

/// Shared stats snapshot: written by the stats thread, read by the render loop.
static STATS_SNAPSHOT: OnceLock<Arc<Mutex<Option<SystemSnapshot>>>> = OnceLock::new();

/// Flag set by kill_app() to force immediate clearing of cached GPU textures.
/// The render loop checks this BEFORE any GPU rendering to prevent submitting
/// draw calls with DMA-BUF textures from a dead client (which causes AMDGPU
/// context reset → abort).
static FORCE_CLEAR_TEXTURES: AtomicBool = AtomicBool::new(false);

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

/// Key-press channel: UI callback sends key names, render loop receives.
thread_local! {
    static KEY_RX: std::cell::RefCell<Option<mpsc::Receiver<String>>> =
        std::cell::RefCell::new(None);
}

/// Register the key-press receiver. Returns the sender for the UI callback.
/// Must be called from the thread that will run the event loop.
pub fn create_key_channel() -> mpsc::Sender<String> {
    let (tx, rx) = mpsc::channel();
    KEY_RX.with(|slot| *slot.borrow_mut() = Some(rx));
    tx
}

/// Keyboard grab channel: Slint focus callback sends GrabCommand,
/// render loop receives and forwards to the keyboard thread.
thread_local! {
    static GRAB_RX: std::cell::RefCell<Option<mpsc::Receiver<GrabCommand>>> =
        std::cell::RefCell::new(None);
}

/// Register the grab receiver. Returns the sender for the Slint focus callback.
/// Must be called from the thread that will run the event loop.
pub fn create_grab_channel() -> mpsc::Sender<GrabCommand> {
    let (tx, rx) = mpsc::channel();
    GRAB_RX.with(|slot| *slot.borrow_mut() = Some(rx));
    tx
}

// ── Active tab tracking ─────────────────────────────────────────────────

thread_local! {
    /// Current active tab index (0=Keyboard, 1=Stats, 2=Apps).
    /// Updated from Slint UI callbacks, read by the render loop.
    static ACTIVE_TAB: std::cell::Cell<i32> = std::cell::Cell::new(0);
}

/// Set the active tab index. Called from Slint tab-change callbacks.
pub fn set_active_tab(tab: i32) {
    let prev = ACTIVE_TAB.with(|v| v.replace(tab));
    if prev != tab {
        eprintln!("[active-tab] {} -> {}", prev, tab);
    }
}

// ── Compositor command sender ───────────────────────────────────────────

thread_local! {
    /// Clone of the calloop Sender for CompositorCommand.
    /// Stored during DrmPlatform::new() so UI callbacks can send commands.
    static COMPOSITOR_CMD_TX: std::cell::RefCell<Option<calloop::channel::Sender<CompositorCommand>>> =
        std::cell::RefCell::new(None);
}

/// Send a command to the compositor thread (e.g. CloseApp).
pub fn send_compositor_command(cmd: CompositorCommand) {
    COMPOSITOR_CMD_TX.with(|slot| {
        if let Some(ref tx) = *slot.borrow() {
            let _ = tx.send(cmd);
        }
    });
}

// ── App child PID tracking ──────────────────────────────────────────────

thread_local! {
    /// PID of the currently running app child process.
    static APP_PID: std::cell::Cell<Option<u32>> = std::cell::Cell::new(None);
    /// Flatpak application ID (e.g. "org.mozilla.firefox") if launched via flatpak.
    static APP_FLATPAK_ID: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
}

/// Store the PID of a launched app. Called from on_launch_app.
pub fn set_app_pid(pid: u32) {
    // Clear the force-clear flag — a new app is launching, so it's safe
    // to resume importing client textures.
    FORCE_CLEAR_TEXTURES.store(false, Ordering::Release);
    APP_PID.with(|v| v.set(Some(pid)));
}

/// Store the Flatpak app ID so kill_app() can use `flatpak kill`.
pub fn set_app_flatpak_id(id: String) {
    APP_FLATPAK_ID.with(|v| *v.borrow_mut() = Some(id));
}

/// Kill the running app process and clear tracked state.
///
/// For Flatpak apps, uses `flatpak kill <app-id>` which reaches into the
/// bwrap PID namespace and sends orderly shutdown. We do NOT also send
/// SIGTERM/SIGKILL to the process group — that would kill Firefox
/// abruptly mid-GPU-operation, causing an AMDGPU context reset that
/// propagates to our GPU context and aborts the process.
///
/// For non-Flatpak apps, uses SIGTERM → SIGKILL escalation.
pub fn kill_app() {
    // Signal the render loop to drop all cached GPU textures immediately.
    // This MUST happen before the kill, because once the client process dies
    // its DMA-BUF GPU resources are freed. If we render a frame using those
    // dead textures, the AMDGPU driver detects a GPU context reset and
    // calls abort(), killing flip-companion.
    FORCE_CLEAR_TEXTURES.store(true, Ordering::Release);

    // For Flatpak: use `flatpak kill` exclusively — it sends orderly
    // shutdown that lets Firefox clean up GPU resources gracefully.
    let flatpak_id = APP_FLATPAK_ID.with(|v| v.borrow_mut().take());
    let used_flatpak_kill = if let Some(ref app_id) = flatpak_id {
        eprintln!("[game-mode] running: flatpak kill {app_id}");
        match std::process::Command::new("flatpak")
            .arg("kill")
            .arg(app_id)
            .spawn()
        {
            Ok(_) => {
                eprintln!("[game-mode] flatpak kill sent for {app_id}");
                true
            }
            Err(e) => {
                eprintln!("[game-mode] flatpak kill failed: {e}, falling back to SIGTERM");
                false
            }
        }
    } else {
        false
    };

    // Only send SIGTERM/SIGKILL if flatpak kill was not used.
    // For flatpak apps, the host-side `flatpak run` process will exit
    // on its own once the sandboxed app is killed.
    if !used_flatpak_kill {
        APP_PID.with(|v| {
            if let Some(pid) = v.take() {
                eprintln!("[game-mode] sending SIGTERM to process group {pid}");
                unsafe { libc::kill(-(pid as i32), libc::SIGTERM); }
                // Spawn a thread to escalate to SIGKILL after 2 seconds.
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    // Check if still alive, send SIGKILL.
                    let ret = unsafe { libc::kill(-(pid as i32), 0) };
                    if ret == 0 {
                        eprintln!("[game-mode] SIGTERM timeout, sending SIGKILL to {pid}");
                        unsafe { libc::kill(-(pid as i32), libc::SIGKILL); }
                    }
                });
            }
        });
    } else {
        // Still consume the PID so it doesn't linger.
        APP_PID.with(|v| { v.take(); });
    }
}

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, TargetPixel,
};
use slint::platform::{Platform, WindowAdapter};
use slint::PhysicalSize;

// ── Layout constants (landscape coordinates) ────────────────────────────

/// Status bar height in logical landscape pixels.
const STATUS_BAR_HEIGHT: f32 = 40.0;
/// Client area Y start (inclusive) = top of the Wayland window region.
const CLIENT_Y_START: f32 = STATUS_BAR_HEIGHT; // 40
/// Client area height (1080 - 40 - 56 = 984).
const CLIENT_HEIGHT: f32 = 984.0;
/// Client area Y end (exclusive) = bottom of the Wayland window region.
const CLIENT_Y_END: f32 = CLIENT_Y_START + CLIENT_HEIGHT; // 1024

// ── Pixel type for Slint → XRGB8888 ────────────────────────────────────

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
        let mut pending_down = false;
        let mut pending_up = false;

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
                                // Defer to SYN_REPORT so ABS_X/Y are up to date
                                if ev.code() == 0x14a {
                                    if ev.value() == 1 {
                                        pending_down = true;
                                    } else {
                                        pending_up = true;
                                    }
                                }
                            }
                            evdev::EventType::SYNCHRONIZATION => {
                                // Rotate panel portrait → landscape 90° CCW:
                                //   lx = (1 - panel_y / y_max) * landscape_w
                                //   ly = panel_x / x_max * landscape_h
                                let lx = (1.0 - cur_y / y_max)
                                    * landscape_w as f32;
                                let ly = (cur_x / x_max)
                                    * landscape_h as f32;

                                if pending_down {
                                    is_down = true;
                                    pending_down = false;
                                    eprintln!("[touch] DOWN raw=({},{}) logical=({:.0},{:.0})", cur_x, cur_y, lx, ly);
                                    let _ = tx.send(TouchEvent::Down {
                                        x: lx,
                                        y: ly,
                                    });
                                } else if pending_up {
                                    is_down = false;
                                    pending_up = false;
                                    eprintln!("[touch] UP");
                                    let _ = tx.send(TouchEvent::Up);
                                } else if is_down {
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

// ── Platform ────────────────────────────────────────────────────────────

pub struct DrmPlatform {
    window: Rc<MinimalSoftwareWindow>,
    gpu: std::cell::RefCell<super::gpu_renderer::GpuRenderer>,
    /// Spare lease fd for GPU renderer recreation after context reset.
    spare_lease_fd: std::cell::RefCell<Option<OwnedFd>>,
    /// Logical landscape dimensions (1620×1080) — what Slint sees
    logical_width: u32,
    logical_height: u32,
    start: Instant,
    touch_rx: Option<mpsc::Receiver<TouchEvent>>,
    // Compositor integration
    render_waker: Arc<RenderWaker>,
    pending_layers: Arc<Mutex<Vec<SurfaceLayer>>>,
    has_toplevel: Arc<AtomicBool>,
    client_touch_tx: calloop::channel::Sender<compositor::TouchEvent>,
    client_key_tx: calloop::channel::Sender<compositor::KeyEvent>,
    control_tx: calloop::channel::Sender<CompositorCommand>,
    _compositor_handle: std::thread::JoinHandle<()>,
    _keyboard_handle: Option<std::thread::JoinHandle<()>>,
}

impl DrmPlatform {
    pub fn new(lease_fd: OwnedFd) -> Result<Self, String> {
        use std::os::fd::AsFd;
        // Dup the lease fd before GpuRenderer consumes it — we keep a
        // spare for GPU renderer recreation after context reset.
        let spare_lease_fd = lease_fd.as_fd().try_clone_to_owned()
            .map_err(|e| format!("dup lease fd: {e}"))?;

        // Initialize GPU renderer (probes DRM, creates GBM + EGL + GLES)
        // Retry a few times — after a GPU reset + execve, the hardware may
        // need extra time to become available again.
        let mut gpu = None;
        for attempt in 1..=5 {
            match super::gpu_renderer::GpuRenderer::new(
                lease_fd.as_fd().try_clone_to_owned()
                    .map_err(|e| format!("dup lease fd for gpu init: {e}"))?,
            ) {
                Ok(g) => {
                    gpu = Some(g);
                    break;
                }
                Err(e) => {
                    eprintln!("[drm-platform] GpuRenderer init attempt {attempt}/5 failed: {e}");
                    if attempt < 5 {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
            }
        }
        let mut gpu = gpu.ok_or_else(|| "GpuRenderer: failed after 5 attempts".to_string())?;
        // Consume the original lease_fd now that we've been duping it
        drop(lease_fd);

        // Query supported DMA-BUF formats from EGL before moving gpu into RefCell.
        let dmabuf_formats = gpu.dmabuf_formats();
        eprintln!("[drm-platform] GPU advertises {} dmabuf format+modifier pairs", dmabuf_formats.iter().count());

        let output_size = gpu.output_size();
        let phys_width = output_size.w as u32; // 1080
        let phys_height = output_size.h as u32; // 1620
        let logical_width = phys_height; // 1620
        let logical_height = phys_width; // 1080

        eprintln!(
            "[drm-platform] physical: {}x{}, logical (landscape): {}x{}",
            phys_width, phys_height, logical_width, logical_height
        );

        // Slint window
        let window = MinimalSoftwareWindow::new(
            slint::platform::software_renderer::RepaintBufferType::ReusedBuffer,
        );
        window.set_size(PhysicalSize::new(logical_width, logical_height));

        // Touch input
        let touch_rx = spawn_touch_thread(logical_width, logical_height);
        if touch_rx.is_some() {
            eprintln!("[drm-platform] touch input enabled");
        } else {
            eprintln!("[drm-platform] warning: no touch input device found");
        }

        // Shared compositor state
        let render_waker = Arc::new(RenderWaker::new());
        let pending_layers: Arc<Mutex<Vec<SurfaceLayer>>> =
            Arc::new(Mutex::new(Vec::new()));
        let has_toplevel = Arc::new(AtomicBool::new(false));

        // Calloop channels for compositor communication
        let (client_touch_tx, client_touch_rx) =
            calloop::channel::channel::<compositor::TouchEvent>();
        let (client_key_tx, client_key_rx) =
            calloop::channel::channel::<compositor::KeyEvent>();
        let (control_tx, control_rx) =
            calloop::channel::channel::<CompositorCommand>();

        // Store a clone of the compositor sender in a thread_local so
        // Slint UI callbacks (close-app, etc.) can send commands.
        COMPOSITOR_CMD_TX.with(|slot| {
            *slot.borrow_mut() = Some(control_tx.clone());
        });

        // Spawn the Wayland compositor thread
        let compositor_handle = compositor::wayland_thread::spawn(
            compositor::wayland_thread::CompositorConfig {
                pending_layers: pending_layers.clone(),
                render_waker: render_waker.clone(),
                has_toplevel: has_toplevel.clone(),
                touch_rx: client_touch_rx,
                key_rx: client_key_rx,
                control_rx,
                dmabuf_formats,
            },
        );

        // Spawn the physical keyboard thread.
        // The grab channel is created later (create_grab_channel),
        // so we take GRAB_RX at the start of run_event_loop instead.
        // Here we just hold key_tx for cloning later.

        Ok(Self {
            window,
            gpu: std::cell::RefCell::new(gpu),
            spare_lease_fd: std::cell::RefCell::new(Some(spare_lease_fd)),
            logical_width,
            logical_height,
            start: Instant::now(),
            touch_rx,
            render_waker,
            pending_layers,
            has_toplevel,
            client_touch_tx,
            client_key_tx,
            control_tx,
            _compositor_handle: compositor_handle,
            _keyboard_handle: None,
        })
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
        let lw = self.logical_width as usize; // 1620
        let lh = self.logical_height as usize; // 1080

        // Landscape buffer for Slint software rendering
        let mut render_buf =
            vec![Xrgb8888Pixel::default(); lw * lh];

        // Spawn the physical keyboard reader thread.
        // GRAB_RX is set by create_grab_channel() called from main.rs before
        // slint::run_event_loop(), so it is available here on the same thread.
        let grab_rx = GRAB_RX.with(|slot| slot.borrow_mut().take());
        if let Some(grab_rx) = grab_rx {
            let handle = crate::input::keyboard_thread::spawn(
                self.client_key_tx.clone(),
                grab_rx,
            );
            if handle.is_some() {
                eprintln!("[drm-platform] physical keyboard thread started");
            } else {
                eprintln!("[drm-platform] warning: no physical keyboard found");
            }
        }

        let mut is_pressed = false;
        let mut last_touch_pos = slint::LogicalPosition::new(0.0, 0.0);
        let mut touch_in_client = false;
        // Track whether we have an active Wayland client rendering.
        // When false, all touches go to Slint (so app tiles, keyboard etc. work).
        // Uses the shared AtomicBool set by the compositor when a toplevel exists.
        let mut client_active = false;

        // Cached client textures — persist across frames so the last
        // committed content stays on screen between Wayland commits.
        let mut cached_client_textures: Vec<(smithay::backend::renderer::gles::GlesTexture, Size<i32, BufferCoord>, i32, i32)> = Vec::new();

        // Create uinput virtual keyboard for system key injection
        let key_injector =
            match super::evdev_input::SyncEvdevInputInjector::try_new() {
                Ok(inj) => Some(inj),
                Err(e) => {
                    eprintln!(
                        "[drm-platform] warning: no uinput keyboard: {e}"
                    );
                    None
                }
            };

        let mut consecutive_render_errors: u32 = 0;

        eprintln!(
            "[drm-platform] entering render loop \
             (logical {}x{}, GPU accelerated)",
            lw, lh
        );

        loop {
            // ── Process touch events with Y-split ───────────────────
            if let Some(ref rx) = self.touch_rx {
                while let Ok(ev) = rx.try_recv() {
                    match ev {
                        TouchEvent::Down { x, y } => {
                            is_pressed = true;
                            let on_app_tab = ACTIVE_TAB.with(|v| v.get()) == 2;
                            eprintln!("[touch-dispatch] DOWN at ({x:.0},{y:.0}) client_active={client_active} on_app_tab={on_app_tab}");

                            if client_active && on_app_tab && y >= CLIENT_Y_START && y < CLIENT_Y_END {
                                // Client area — forward to Wayland compositor
                                touch_in_client = true;
                                let client_y = y - CLIENT_Y_START;
                                let _ = self.client_touch_tx.send(
                                    compositor::TouchEvent {
                                        slot: 0,
                                        x: x as f64,
                                        y: client_y as f64,
                                        kind: compositor::TouchEventKind::Down,
                                    },
                                );
                            } else {
                                // Slint bar area (status or nav)
                                touch_in_client = false;
                                last_touch_pos =
                                    slint::LogicalPosition::new(x, y);
                                eprintln!("[touch-dispatch] → Slint PointerPressed at ({x:.0},{y:.0})");
                                self.window.dispatch_event(
                                    slint::platform::WindowEvent::PointerPressed {
                                        position: last_touch_pos,
                                        button: slint::platform::PointerEventButton::Left,
                                    },
                                );
                            }
                        }
                        TouchEvent::Move { x, y } => {
                            if !is_pressed {
                                continue;
                            }
                            if touch_in_client {
                                let client_y = y - CLIENT_Y_START;
                                let _ = self.client_touch_tx.send(
                                    compositor::TouchEvent {
                                        slot: 0,
                                        x: x as f64,
                                        y: client_y as f64,
                                        kind: compositor::TouchEventKind::Motion,
                                    },
                                );
                            } else {
                                last_touch_pos =
                                    slint::LogicalPosition::new(x, y);
                                self.window.dispatch_event(
                                    slint::platform::WindowEvent::PointerMoved {
                                        position: last_touch_pos,
                                    },
                                );
                            }
                        }
                        TouchEvent::Up => {
                            is_pressed = false;
                            if touch_in_client {
                                let _ = self.client_touch_tx.send(
                                    compositor::TouchEvent {
                                        slot: 0,
                                        x: 0.0,
                                        y: 0.0,
                                        kind: compositor::TouchEventKind::Up,
                                    },
                                );
                            } else {
                                self.window.dispatch_event(
                                    slint::platform::WindowEvent::PointerReleased {
                                        position: last_touch_pos,
                                        button: slint::platform::PointerEventButton::Left,
                                    },
                                );
                            }
                            touch_in_client = false;
                        }
                    }
                }
            }

            // ── Process keyboard events ─────────────────────────────
            if let Some(ref injector) = key_injector {
                KEY_RX.with(|slot| {
                    if let Some(ref rx) = *slot.borrow() {
                        while let Ok(key) = rx.try_recv() {
                            injector.press_key_sync(&key);
                        }
                    }
                });
            }

            // ── Poll shared stats and push to Slint via callback ────
            if let Some(shared) = STATS_SNAPSHOT.get() {
                let snap = shared.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Some(snap) = snap {
                    STATS_APPLY.with(|slot| {
                        if let Some(ref cb) = *slot.borrow() {
                            cb(snap);
                        }
                    });
                }
            }

            // ── Render Slint to software buffer ─────────────────────
            slint::platform::update_timers_and_animations();
            self.window.request_redraw();

            self.window.draw_if_needed(|renderer| {
                renderer.render_by_line(LandscapeLineBuffer {
                    buf: &mut render_buf,
                    stride: lw,
                });
            });

            // ── Check for GPU reset (mesa abort intercepted) ────────
            if super::abort_guard::GPU_RESET_DETECTED.load(Ordering::Acquire) {
                eprintln!("[render] GPU reset detected via abort() interposition");
                // Mesa's amdgpu_winsys singleton is poisoned (parked worker
                // thread holds internal mutexes). No new EGL context can be
                // created on this GPU without deadlocking. Re-exec the
                // process to get a completely fresh address space.
                super::abort_guard::self_restart();
            }

            // ── Import Slint pixels as a GL texture ─────────────────
            let slint_bytes = unsafe {
                std::slice::from_raw_parts(
                    render_buf.as_ptr() as *const u8,
                    render_buf.len() * 4,
                )
            };

            let mut gpu = self.gpu.borrow_mut();

            let slint_size: Size<i32, BufferCoord> =
                (lw as i32, lh as i32).into();
            let slint_tex = match gpu
                .renderer()
                .import_memory(slint_bytes, Fourcc::Xrgb8888, slint_size, false)
            {
                Ok(tex) => Some(tex),
                Err(e) => {
                    eprintln!("[render] import slint texture: {e}");
                    None
                }
            };

            // ── Import Wayland client surface layers if available ────
            // Only re-import when the compositor has new layers; otherwise
            // reuse the cached textures so the last frame stays on screen.

            // FIRST: check if the client has gone away. We must clear
            // cached GPU textures (DMA-BUFs) BEFORE any GPU rendering.
            // If we render with DMA-BUF textures from a dead client whose
            // GPU resources have been freed, the AMDGPU driver detects a
            // GPU context reset and aborts the process.
            //
            // Check FORCE_CLEAR_TEXTURES first — this is set synchronously
            // by kill_app() (called from the close button callback) BEFORE
            // the process is killed. This closes the race window between
            // kill_app() and has_toplevel becoming false.
            //
            // The flag stays set until a new app is launched (cleared in
            // set_app_pid), so we keep draining stale layers every frame
            // even if the compositor hasn't processed CloseApp yet.
            let force_cleared = FORCE_CLEAR_TEXTURES.load(Ordering::Acquire);
            if force_cleared {
                if !cached_client_textures.is_empty() {
                    eprintln!("[drm-platform] force-clear: dropping cached client textures");
                }
                cached_client_textures.clear();
                // Also drain any pending layers so we don't import dead DMA-BUFs.
                let mut lock = self.pending_layers.lock().unwrap_or_else(|e| e.into_inner());
                lock.clear();
                drop(lock);
                client_active = false;
                touch_in_client = false;
            }

            let was_active = client_active;
            // Don't re-read has_toplevel if force_cleared — it may still be
            // true and would re-enable client_active, undoing the force-clear.
            if !force_cleared {
                client_active = self.has_toplevel.load(Ordering::Relaxed);
            }

            if was_active && !client_active {
                eprintln!("[drm-platform] client deactivated, clearing cached textures");
                cached_client_textures.clear();
                touch_in_client = false;
            }

            // Only import new layers if the client is still alive.
            if client_active {
                let mut lock = self.pending_layers.lock().unwrap_or_else(|e| e.into_inner());
                if !lock.is_empty() {
                    let layers = std::mem::take(&mut *lock);
                    drop(lock);

                    let mut new_textures: Vec<(smithay::backend::renderer::gles::GlesTexture, Size<i32, BufferCoord>, i32, i32)> = Vec::new();
                    for layer in &layers {
                        match &layer.buffer {
                            BufferData::Shm {
                                data,
                                width,
                                height,
                                stride: _,
                                format,
                            } => {
                                let size: Size<i32, BufferCoord> =
                                    (*width as i32, *height as i32).into();
                                match gpu.renderer().import_memory(data, *format, size, false) {
                                    Ok(tex) => new_textures.push((tex, size, layer.x, layer.y)),
                                    Err(e) => eprintln!("[render] import shm layer at ({},{}): {e}", layer.x, layer.y),
                                }
                            }
                            BufferData::Dma(ref dmabuf) => {
                                match gpu.renderer().import_dmabuf(dmabuf, None) {
                                    Ok(tex) => {
                                        let size = tex.size();
                                        new_textures.push((tex, size, layer.x, layer.y));
                                    }
                                    Err(e) => eprintln!("[render] import dma layer at ({},{}): {e}", layer.x, layer.y),
                                }
                            }
                        }
                    }
                    if !new_textures.is_empty() {
                        cached_client_textures = new_textures;
                    }
                }
            }
            let has_client = !cached_client_textures.is_empty();

            // ── GPU composition + page flip ─────────────────────────
            if slint_tex.is_some() || has_client {
                if let Err(e) =
                    gpu.render_frame(Transform::_270, |frame| {
                        // Clear to black
                        let full_screen: Rectangle<i32, Physical> =
                            Rectangle::from_size(
                                (lw as i32, lh as i32).into(),
                            );
                        let _ = frame.clear(
                            smithay::backend::renderer::Color32F::BLACK,
                            &[full_screen],
                        );

                        // Draw full-screen Slint layer (status bar + nav + background)
                        if let Some(ref tex) = slint_tex {
                            let tex_size = tex.size();
                            let src: Rectangle<f64, BufferCoord> =
                                Rectangle::from_size(
                                    (tex_size.w as f64, tex_size.h as f64).into(),
                                );
                            let dst: Rectangle<i32, Physical> =
                                Rectangle::new(
                                    (0, 0).into(),
                                    (lw as i32, lh as i32).into(),
                                );
                            if let Err(e) = frame.render_texture_from_to(
                                tex,
                                src,
                                dst,
                                &[dst],
                                &[],
                                Transform::Normal,
                                1.0,
                                None,
                                &[],
                            ) {
                                eprintln!("[render] draw slint: {e}");
                            }
                        }

                        // Overdraw client surface layers in the middle region,
                        // but only when on the Apps tab (tab 2).
                        // Layers are clipped to the client area so CSD decorations
                        // (e.g. Firefox title bar) above CLIENT_Y_START are hidden.
                        let on_app_tab = ACTIVE_TAB.with(|v| v.get()) == 2;
                        if on_app_tab {
                            let cy_start = CLIENT_Y_START as i32;
                            let cy_end = CLIENT_Y_END as i32;
                            for (tex, tex_size, off_x, off_y) in &cached_client_textures {
                                // Position within the client area: client_area_origin + surface offset
                                let dst_x = *off_x;
                                let dst_y = cy_start + *off_y;
                                let dst_bottom = dst_y + tex_size.h;

                                // Clip to client area vertically
                                let clip_top = (cy_start - dst_y).max(0);
                                let clip_bottom = (dst_bottom - cy_end).max(0);
                                let vis_h = tex_size.h - clip_top - clip_bottom;
                                if vis_h <= 0 { continue; }

                                let src: Rectangle<f64, BufferCoord> =
                                    Rectangle::new(
                                        (0.0, clip_top as f64).into(),
                                        (tex_size.w as f64, vis_h as f64).into(),
                                    );
                                let dst: Rectangle<i32, Physical> =
                                    Rectangle::new(
                                        (dst_x, dst_y + clip_top).into(),
                                        (tex_size.w, vis_h).into(),
                                    );
                                if let Err(e) = frame.render_texture_from_to(
                                    tex,
                                    src,
                                    dst,
                                    &[dst],
                                    &[],
                                    Transform::Normal,
                                    1.0,
                                    None,
                                    &[],
                                ) {
                                    eprintln!("[render] draw layer at ({},{}): {e}", off_x, off_y);
                                }
                            }
                        }
                    })
                {
                    consecutive_render_errors += 1;
                    eprintln!("[render] render_frame error ({consecutive_render_errors}): {e}");

                    // If the abort guard detected a GPU reset (SIGUSR1
                    // interrupted us via siglongjmp), re-exec immediately.
                    if super::abort_guard::GPU_RESET_DETECTED.load(Ordering::Acquire) {
                        eprintln!("[render] GPU reset detected during render_frame, re-execing...");
                        drop(gpu);
                        super::abort_guard::self_restart();
                    }

                    // Non-reset render errors: recreate after 2 consecutive failures.
                    if consecutive_render_errors >= 2 {
                        eprintln!("[render] GPU context likely lost, attempting recreation...");
                        cached_client_textures.clear();
                        drop(gpu);

                        std::thread::sleep(std::time::Duration::from_millis(100));

                        if let Some(spare_fd) = self.spare_lease_fd.borrow_mut().take() {
                            use std::os::fd::AsFd;
                            let next_spare = spare_fd.as_fd().try_clone_to_owned().ok();
                            match super::gpu_renderer::GpuRenderer::new(spare_fd) {
                                Ok(new_gpu) => {
                                    let old_gpu = self.gpu.replace(new_gpu);
                                    std::mem::forget(old_gpu);
                                    eprintln!("[render] GPU renderer recreated (old leaked)");
                                    if let Some(fd) = next_spare {
                                        *self.spare_lease_fd.borrow_mut() = Some(fd);
                                    }
                                    consecutive_render_errors = 0;
                                }
                                Err(e) => {
                                    eprintln!("[render] GPU recreation FAILED: {e}");
                                }
                            }
                        } else {
                            eprintln!("[render] no spare lease fd for GPU recreation");
                        }

                        self.render_waker
                            .wait_timeout(std::time::Duration::from_millis(16));
                        continue;
                    }
                } else {
                    consecutive_render_errors = 0;

                    // Even on Ok, check if abort fired during render_frame
                    // (the SIGUSR1 may have arrived between GL calls without
                    // causing an error). Handle it on the next iteration.
                }

                // Notify compositor that frame is done (sends frame callbacks)
                // Send whenever client is active, not just when new layers
                // arrived — the client needs continuous frame callbacks to
                // keep its rendering loop alive.
                if client_active {
                    let _ =
                        self.control_tx.send(CompositorCommand::FrameDone);
                }
            }

            // Clean up old textures to avoid GPU memory leaks
            if let Err(e) = gpu.renderer().cleanup_texture_cache() {
                eprintln!("[render] cleanup: {e}");
            }

            // Drop the borrow before sleeping
            drop(gpu);

            // Wait for next frame — Condvar wakes us early on new
            // Wayland client commits, otherwise ~60 FPS timeout.
            self.render_waker
                .wait_timeout(std::time::Duration::from_millis(16));
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
