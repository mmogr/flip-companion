//! Smithay-based Wayland compositor for the AYANEO Flip DS bottom screen.
//!
//! Single-client compositor that accepts one XDG toplevel at a time and
//! forwards its committed surface layers to the render thread via a shared
//! `Arc<Mutex<Vec<SurfaceLayer>>>`. The compositor runs on a dedicated
//! calloop thread (`wayland_thread`).

pub mod wayland_thread;

use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::protocol::wl_buffer;
use smithay::reexports::wayland_server::protocol::wl_seat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Resource};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    with_states, CompositorClientState, CompositorHandler, CompositorState,
    SurfaceAttributes, SubsurfaceCachedState, TraversalAction,
};
use smithay::wayland::dmabuf::{
    DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
};
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::viewporter::ViewporterState;
use smithay::utils::Serial;
use drm_fourcc::DrmFourcc;

use std::cell::RefCell;

/// Per-surface cached wl_buffer. Stored in the surface's `data_map`
/// so it persists across commits and is cleaned up when the surface dies.
struct CachedBuffer(RefCell<Option<wl_buffer::WlBuffer>>);

// ── Render waker ────────────────────────────────────────────────────────

/// Condvar-based wakeup primitive shared between threads.
///
/// The compositor thread calls `wake()` when a new buffer is committed.
/// The render thread sleeps on `wait_timeout()` instead of `thread::sleep()`.
pub struct RenderWaker {
    mutex: Mutex<()>,
    condvar: Condvar,
}

impl RenderWaker {
    pub fn new() -> Self {
        Self {
            mutex: Mutex::new(()),
            condvar: Condvar::new(),
        }
    }

    /// Wake the render thread (non-blocking).
    pub fn wake(&self) {
        // Lock briefly just to pair with the condvar correctly.
        // Tolerate a poisoned mutex so a panic on one thread doesn't
        // cascade-crash the other.
        let _guard = self.mutex.lock().unwrap_or_else(|e| e.into_inner());
        self.condvar.notify_one();
    }

    /// Sleep until woken or `timeout` elapses. Returns the mutex guard
    /// (caller should drop it).
    pub fn wait_timeout(&self, timeout: std::time::Duration) {
        let guard = self.mutex.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self.condvar.wait_timeout(guard, timeout);
    }
}

// ── Surface layer data ──────────────────────────────────────────────────

/// Buffer committed by a Wayland client, ready for GPU import on the
/// render thread.
pub enum BufferData {
    /// Client used wl_shm: pixel data copied into a Vec.
    Shm {
        data: Vec<u8>,
        width: u32,
        height: u32,
        stride: u32,
        format: DrmFourcc,
    },
    /// Client used linux-dmabuf: reference-counted fd handles.
    Dma(Dmabuf),
}

/// A single surface in the flattened Wayland surface tree, with its
/// buffer and absolute offset relative to the toplevel origin.
pub struct SurfaceLayer {
    pub buffer: BufferData,
    pub x: i32,
    pub y: i32,
}

// ── Channel message types ───────────────────────────────────────────────

/// Touch event forwarded from the render thread to the compositor thread
/// for injection into `wl_touch`.
#[derive(Debug, Clone, Copy)]
pub struct TouchEvent {
    pub slot: i32,
    pub x: f64,
    pub y: f64,
    pub kind: TouchEventKind,
}

#[derive(Debug, Clone, Copy)]
pub enum TouchEventKind {
    Down,
    Up,
    Motion,
}

/// Key event forwarded from the keyboard thread to the compositor thread
/// for injection into `wl_keyboard`.
#[derive(Debug, Clone, Copy)]
pub struct KeyEvent {
    pub keycode: u32,
    pub pressed: bool,
}

/// Commands from the render thread to the compositor thread.
pub enum CompositorCommand {
    /// Notify the client that a frame was presented (sends wl_surface.frame callback).
    FrameDone,
    /// Launch an app: the compositor should expect a new client.
    LaunchApp { exec: String, url: Option<String> },
    /// Close the current app.
    CloseApp,
}

/// Keyboard grab commands from the render thread to the keyboard thread.
#[derive(Debug, Clone, Copy)]
pub enum GrabCommand {
    Grab,
    Release,
}

// ── Per-client data ─────────────────────────────────────────────────────

/// Stored in each Wayland client's data slot for automatic cleanup.
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    /// Shared flag: cleared on disconnect so render thread knows client is gone.
    pub has_toplevel: Arc<AtomicBool>,
}

impl wayland_server::backend::ClientData for ClientState {
    fn initialized(&self, _client_id: wayland_server::backend::ClientId) {}
    fn disconnected(
        &self,
        _client_id: wayland_server::backend::ClientId,
        reason: wayland_server::backend::DisconnectReason,
    ) {
        eprintln!("[compositor] client disconnected: {reason:?}");
        self.has_toplevel.store(false, Ordering::Relaxed);
    }
}

// ── Compositor state ────────────────────────────────────────────────────

/// The main Smithay compositor state struct.
///
/// Owns all protocol state objects. Lives on the compositor thread.
pub struct FlipCompositor {
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub dmabuf_state: DmabufState,
    pub dmabuf_global: DmabufGlobal,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub viewporter_state: ViewporterState,
    pub xdg_decoration_state: XdgDecorationState,
    pub seat: Seat<Self>,
    /// The wl_output we advertise to clients.
    pub output: smithay::output::Output,

    /// The single active toplevel, if any.
    pub toplevel: Option<ToplevelSurface>,
    /// Shared flag: true when a toplevel exists (for touch routing).
    pub has_toplevel: Arc<AtomicBool>,

    /// Set by the render thread (FrameDone) to signal that the compositor
    /// should fire pending frame callbacks on the next loop iteration.
    pub frame_presented: bool,

    /// Shared surface layer handoff: compositor writes, render thread reads.
    pub pending_layers: Arc<Mutex<Vec<SurfaceLayer>>>,
    /// Wakes the render thread on new buffer commit.
    pub render_waker: Arc<RenderWaker>,
}

// ── Handler trait implementations ───────────────────────────────────────

impl CompositorHandler for FlipCompositor {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        eprintln!("[compositor] commit() on surface {:?}", surface.id());
        // First, process the committed surface's buffer assignment:
        // cache the wl_buffer in the surface's data_map for later tree walks.
        with_states(surface, |data| {
            let mut attrs = data.cached_state.get::<SurfaceAttributes>();
            let attrs = attrs.current();
            if let Some(assignment) = attrs.buffer.take() {
                match assignment {
                    smithay::wayland::compositor::BufferAssignment::NewBuffer(wl_buf) => {
                        data.data_map.insert_if_missing(|| CachedBuffer(RefCell::new(None)));
                        if let Some(cached) = data.data_map.get::<CachedBuffer>() {
                            // Release the OLD buffer before storing the new one.
                            // This implements standard double-buffering: the previous
                            // buffer is released when the next commit arrives, giving
                            // the client time to finish staging.
                            let old = cached.0.borrow_mut().replace(wl_buf);
                            if let Some(old_buf) = old {
                                old_buf.release();
                            }
                        }
                    }
                    smithay::wayland::compositor::BufferAssignment::Removed => {
                        if let Some(cached) = data.data_map.get::<CachedBuffer>() {
                            if let Some(old_buf) = cached.0.borrow_mut().take() {
                                old_buf.release();
                            }
                        }
                    }
                }
            }
        });

        // Now rebuild the full surface layer list from the toplevel's tree.
        let Some(ref toplevel) = self.toplevel else {
            return;
        };
        // Don't walk the tree if the surface is dead (client disconnected).
        if !toplevel.alive() {
            return;
        }

        let root = toplevel.wl_surface().clone();
        let mut layers: Vec<SurfaceLayer> = Vec::new();

        // Read the xdg_surface geometry so we can offset layers to exclude CSD
        // decorations (e.g. Firefox's title bar). The geometry rectangle tells
        // us where the actual window content starts within the buffer.
        let geo_offset = with_states(&root, |data| {
            data.cached_state
                .get::<SurfaceCachedState>()
                .current()
                .geometry
                .map(|g| (g.loc.x, g.loc.y))
                .unwrap_or((0, 0))
        });

        // with_surface_tree_upward walks bottom-to-top (paint order).
        smithay::wayland::compositor::with_surface_tree_upward(
            &root,
            (0i32, 0i32),
            |_surface, data, &(px, py)| {
                // Compute absolute offset: parent offset + subsurface position.
                let sub_loc = data
                    .cached_state
                    .get::<SubsurfaceCachedState>()
                    .current()
                    .location;
                let abs_x = px + sub_loc.x;
                let abs_y = py + sub_loc.y;
                TraversalAction::DoChildren((abs_x, abs_y))
            },
            |_surface, data, &(abs_x, abs_y)| {
                // For each surface, try to extract its cached wl_buffer.
                if let Some(cached) = data.data_map.get::<CachedBuffer>() {
                    let borrow = cached.0.borrow();
                    if let Some(ref wl_buf) = *borrow {
                        if let Some(layer) = extract_buffer(wl_buf, abs_x, abs_y) {
                            layers.push(layer);
                        }
                    }
                }
            },
            |_, _, _| true,
        );

        if !layers.is_empty() {
            // Subtract geometry offset so layer positions are relative to the
            // window content area, not the raw buffer (hides CSD title bar).
            for layer in &mut layers {
                layer.x -= geo_offset.0;
                layer.y -= geo_offset.1;
            }
            eprintln!("[compositor] commit produced {} layers (geo_offset {:?})", layers.len(), geo_offset);
            *self.pending_layers.lock().unwrap_or_else(|e| e.into_inner()) = layers;
            self.render_waker.wake();
        }
    }
}

impl ShmHandler for FlipCompositor {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl BufferHandler for FlipCompositor {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl DmabufHandler for FlipCompositor {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        // We always accept dmabufs — actual import happens on the render thread.
        if let Err(e) = notifier.successful::<FlipCompositor>() {
            eprintln!("[compositor] dmabuf import notification failed: {e}");
        }
    }
}

impl XdgShellHandler for FlipCompositor {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        eprintln!("[compositor] new xdg_toplevel created");
        // Tell the client it's fullscreen at the full output size.
        // Firefox hides its CSD title bar in fullscreen mode but keeps the
        // tab/address bar.  The render thread clips the surface to the
        // visible client area (CLIENT_Y_START..CLIENT_Y_END).
        surface.with_pending_state(|state| {
            state.size = Some((1620, 1080).into());
            state.states.set(
                smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen,
            );
            state.states.set(
                smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Activated,
            );
        });
        surface.send_configure();

        // Give the surface keyboard focus so GDK considers the seat valid.
        let wl_surface = surface.wl_surface().clone();
        if let Some(keyboard) = self.seat.get_keyboard() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, Some(wl_surface.clone()), serial);
            eprintln!("[compositor] keyboard focus set on toplevel");
        }

        // Tell the client which output the surface is on.
        // GDK3 needs this for scale/DPI/output awareness.
        self.output.enter(&wl_surface);
        eprintln!("[compositor] wl_surface.enter sent for output");

        // Track as the single active toplevel.
        self.has_toplevel.store(true, Ordering::Relaxed);
        self.toplevel = Some(surface);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        // Send initial configure so the client can map the popup.
        // Without this, GTK blocks waiting for configure and freezes the UI.
        match surface.send_configure() {
            Ok(serial) => eprintln!("[compositor] popup configured (serial {serial:?})"),
            Err(e) => eprintln!("[compositor] popup configure error: {e:?}"),
        }
    }

    fn grab(&mut self, surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // Deny the grab — per XDG shell spec this immediately dismisses the popup.
        // Our kiosk compositor doesn't support popup grabs; without this,
        // modal consent/permission popups block the page forever.
        eprintln!("[compositor] popup grab denied → sending popup_done");
        surface.send_popup_done();
    }

    fn reposition_request(&mut self, _surface: PopupSurface, _positioner: PositionerState, _token: u32) {}
}

impl SeatHandler for FlipCompositor {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
}

impl SelectionHandler for FlipCompositor {
    type SelectionUserData = ();
}

impl DataDeviceHandler for FlipCompositor {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for FlipCompositor {}
impl ServerDndGrabHandler for FlipCompositor {}

impl XdgDecorationHandler for FlipCompositor {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }
}

impl smithay::wayland::output::OutputHandler for FlipCompositor {
    fn output_bound(
        &mut self,
        _output: smithay::output::Output,
        _wl_output: smithay::reexports::wayland_server::protocol::wl_output::WlOutput,
    ) {}
}

// ── Delegate macros ─────────────────────────────────────────────────────

smithay::delegate_compositor!(FlipCompositor);
smithay::delegate_shm!(FlipCompositor);
smithay::delegate_dmabuf!(FlipCompositor);
smithay::delegate_xdg_shell!(FlipCompositor);
smithay::delegate_seat!(FlipCompositor);
smithay::delegate_data_device!(FlipCompositor);
smithay::delegate_output!(FlipCompositor);
smithay::delegate_viewporter!(FlipCompositor);
smithay::delegate_xdg_decoration!(FlipCompositor);
// ── Buffer extraction helper ────────────────────────────────────────────

/// Extract a buffer from a wl_buffer into a SurfaceLayer at the given offset.
fn extract_buffer(wl_buf: &wl_buffer::WlBuffer, x: i32, y: i32) -> Option<SurfaceLayer> {
    // Try SHM first.
    if let Ok(shm_buf) = smithay::wayland::shm::with_buffer_contents(
        wl_buf,
        |ptr, len, info| {
            let data = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
            BufferData::Shm {
                data,
                width: info.width as u32,
                height: info.height as u32,
                stride: info.stride as u32,
                format: shm_format_to_fourcc(info.format),
            }
        },
    ) {
        // Do NOT release here — release happens in commit() when the
        // next buffer replaces this one (standard double-buffering).
        return Some(SurfaceLayer { buffer: shm_buf, x, y });
    }

    // Try DMA-BUF.
    if let Ok(dmabuf) = smithay::wayland::dmabuf::get_dmabuf(wl_buf).cloned() {
        // Don't release dmabuf wl_buffer — it stays alive until the render thread imports.
        return Some(SurfaceLayer {
            buffer: BufferData::Dma(dmabuf),
            x,
            y,
        });
    }

    None
}

/// Map wl_shm format to DRM fourcc.
fn shm_format_to_fourcc(
    fmt: smithay::reexports::wayland_server::protocol::wl_shm::Format,
) -> DrmFourcc {
    use smithay::reexports::wayland_server::protocol::wl_shm::Format as F;
    match fmt {
        F::Argb8888 => DrmFourcc::Argb8888,
        F::Xrgb8888 => DrmFourcc::Xrgb8888,
        _ => DrmFourcc::Xrgb8888, // fallback
    }
}

// Re-export wayland_server for the ClientData impl
use smithay::reexports::wayland_server;
