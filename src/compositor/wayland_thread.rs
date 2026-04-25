//! Dedicated compositor thread running a calloop event loop.
//!
//! Listens on `wayland-flip` socket, dispatches Wayland protocol,
//! and forwards committed buffers to the render thread.

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use calloop::channel::Channel;
use calloop::generic::Generic;
use calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::backend::allocator::format::FormatSet;
use smithay::input::keyboard::XkbConfig;
use smithay::input::SeatState;
use smithay::reexports::wayland_server::Display;
use smithay::wayland::compositor::CompositorState;
use smithay::wayland::dmabuf::DmabufState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::viewporter::ViewporterState;

use super::{
    ClientState, CompositorCommand, FlipCompositor, KeyEvent, SurfaceLayer,
    RenderWaker, TouchEvent,
};

/// Configuration for the compositor thread.
pub struct CompositorConfig {
    /// Shared surface layer list (compositor writes, render thread reads).
    pub pending_layers: Arc<Mutex<Vec<SurfaceLayer>>>,
    /// Waker to notify the render thread of new buffers.
    pub render_waker: Arc<RenderWaker>,
    /// Shared flag: true when a toplevel exists (for touch routing).
    pub has_toplevel: Arc<AtomicBool>,
    /// Channel receiving touch events from the render thread.
    pub touch_rx: Channel<TouchEvent>,
    /// Channel receiving key events from the keyboard thread.
    pub key_rx: Channel<KeyEvent>,
    /// Channel receiving commands from the render thread.
    pub control_rx: Channel<CompositorCommand>,
    /// Supported DMA-BUF format+modifier pairs from the GPU renderer.
    pub dmabuf_formats: FormatSet,
}

/// Spawn the compositor thread. Returns immediately.
///
/// The thread creates a Wayland display listening on `wayland-flip`,
/// sets up all protocol globals, and runs the calloop event loop.
pub fn spawn(config: CompositorConfig) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("compositor".into())
        .spawn(move || run(config))
        .expect("failed to spawn compositor thread")
}

/// Data accessible inside calloop callbacks: owns both the Display and
/// the compositor state. Needed because calloop closures receive `&mut CalloopData`
/// but we also need the Display for dispatch/flush/insert_client.
struct CalloopData {
    display: Display<FlipCompositor>,
    state: FlipCompositor,
}

fn run(config: CompositorConfig) {
    // ── Wayland display ─────────────────────────────────────────────
    let mut display = Display::<FlipCompositor>::new().expect("failed to create Wayland display");
    let dh = display.handle();

    // ── Protocol globals ────────────────────────────────────────────
    let compositor_state = CompositorState::new::<FlipCompositor>(&dh);
    let xdg_shell_state = XdgShellState::new::<FlipCompositor>(&dh);
    let shm_state = ShmState::new::<FlipCompositor>(&dh, []);
    let mut dmabuf_state = DmabufState::new();
    let dmabuf_global = dmabuf_state.create_global::<FlipCompositor>(&dh, config.dmabuf_formats);
    let mut seat_state = SeatState::new();
    let data_device_state = DataDeviceState::new::<FlipCompositor>(&dh);
    let viewporter_state = ViewporterState::new::<FlipCompositor>(&dh);
    let xdg_decoration_state = XdgDecorationState::new::<FlipCompositor>(&dh);

    // ── Seat with keyboard + touch + pointer ────────────────────────
    let mut seat = seat_state.new_wl_seat(&dh, "flip");
    seat.add_keyboard(XkbConfig::default(), 200, 25)
        .expect("failed to add keyboard");
    seat.add_touch();
    seat.add_pointer();

    // ── Output advertisement ────────────────────────────────────────
    let output = smithay::output::Output::new(
        "flip-bottom".into(),
        smithay::output::PhysicalProperties {
            size: (93, 62).into(),
            subpixel: smithay::output::Subpixel::Unknown,
            make: "AYANEO".into(),
            model: "FlipDS-bottom".into(),
        },
    );
    let mode = smithay::output::Mode {
        size: (1620, 1080).into(),
        refresh: 60000,
    };
    output.add_mode(mode);
    output.set_preferred(mode);
    output.change_current_state(Some(mode), None, None, None);
    output.create_global::<FlipCompositor>(&dh);

    // ── Assemble state ──────────────────────────────────────────────
    let state = FlipCompositor {
        compositor_state,
        xdg_shell_state,
        shm_state,
        dmabuf_state,
        dmabuf_global,
        seat_state,
        data_device_state,
        viewporter_state,
        xdg_decoration_state,
        seat,
        output,
        toplevel: None,
        has_toplevel: config.has_toplevel.clone(),
        frame_presented: false,
        pending_layers: config.pending_layers,
        render_waker: config.render_waker,
    };

    // ── Calloop event loop ──────────────────────────────────────────
    let mut event_loop =
        EventLoop::<CalloopData>::try_new().expect("failed to create calloop event loop");
    let loop_handle = event_loop.handle();

    // ── Listening socket: $XDG_RUNTIME_DIR/flip-wayland/wayland-0 ───
    // Place the socket in a subdirectory so Flatpak can expose it via
    // --filesystem=xdg-run/flip-wayland (individual socket files can't
    // be reliably bind-mounted by Flatpak's --filesystem option).
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .expect("XDG_RUNTIME_DIR not set");
    let socket_dir = std::path::PathBuf::from(&runtime_dir).join("flip-wayland");
    std::fs::create_dir_all(&socket_dir)
        .expect("failed to create flip-wayland socket directory");
    let listening_socket = ListeningSocketSource::with_name("flip-wayland/wayland-0")
        .expect("failed to bind flip-wayland/wayland-0 socket");
    let socket_name = listening_socket.socket_name().to_owned();
    eprintln!("[compositor] listening on {:?}", socket_name);

    // Collect new client streams; insert them in the main loop body
    // where we have access to the Display.
    let new_clients: Arc<Mutex<Vec<UnixStream>>> = Arc::new(Mutex::new(Vec::new()));
    let new_clients_writer = new_clients.clone();

    loop_handle
        .insert_source(listening_socket, move |stream: UnixStream, _, _data: &mut CalloopData| {
            new_clients_writer.lock().unwrap_or_else(|e| e.into_inner()).push(stream);
        })
        .expect("failed to insert listening socket source");

    // ── Display fd source (wakes loop when clients send requests) ───
    let display_fd = display.backend().poll_fd().as_fd().try_clone_to_owned().unwrap();
    let display_source = Generic::new(display_fd, Interest::READ, Mode::Level);
    loop_handle
        .insert_source(display_source, |_, _, _data: &mut CalloopData| {
            Ok(PostAction::Continue)
        })
        .expect("failed to insert display fd source");

    // ── Touch channel ───────────────────────────────────────────────
    loop_handle
        .insert_source(config.touch_rx, |event, _, data: &mut CalloopData| {
            if let calloop::channel::Event::Msg(touch) = event {
                handle_touch(&mut data.state, touch);
            }
        })
        .expect("failed to insert touch channel");

    // ── Key channel ─────────────────────────────────────────────────
    loop_handle
        .insert_source(config.key_rx, |event, _, data: &mut CalloopData| {
            if let calloop::channel::Event::Msg(key) = event {
                handle_key(&mut data.state, key);
            }
        })
        .expect("failed to insert key channel");

    // ── Control channel ─────────────────────────────────────────────
    loop_handle
        .insert_source(config.control_rx, |event, _, data: &mut CalloopData| {
            if let calloop::channel::Event::Msg(cmd) = event {
                handle_command(&mut data.state, cmd);
            }
        })
        .expect("failed to insert control channel");

    // ── Bundle display + state ──────────────────────────────────────
    let mut data = CalloopData { display, state };

    // ── Main loop ───────────────────────────────────────────────────
    eprintln!("[compositor] entering event loop");
    loop {
        // Accept any new clients.
        {
            let mut clients = new_clients.lock().unwrap_or_else(|e| e.into_inner());
            for stream in clients.drain(..) {
                let client_state = Arc::new(ClientState {
                    compositor_state: Default::default(),
                    has_toplevel: config.has_toplevel.clone(),
                    owns_toplevel: AtomicBool::new(false),
                });
                match data.display.handle().insert_client(stream, client_state) {
                    Ok(_) => eprintln!("[compositor] new client connected"),
                    Err(e) => eprintln!("[compositor] failed to insert client: {e}"),
                }
            }
        }

        // Dispatch Wayland client messages.
        if let Err(e) = data.display.dispatch_clients(&mut data.state) {
            eprintln!("[compositor] dispatch error: {e}");
        }

        // After dispatch, check if the toplevel surface is still alive.
        // The xdg_toplevel role can be destroyed independently of the client
        // (e.g. GTK destroys+recreates the toplevel for fullscreen role
        // transitions, or a page navigates and the window is rebuilt).
        // We MUST set toplevel = None BEFORE fire_frame_callbacks or any
        // other code that walks the surface tree — otherwise smithay's
        // internal surface data access panics on destroyed objects.
        //
        // We deliberately do NOT clear has_toplevel here. Doing so would
        // make the render thread treat the app as gone and switch back to
        // the launcher UI — which is incorrect when the client process is
        // still alive and may map a fresh toplevel a moment later.
        // has_toplevel is cleared only by:
        //   • ClientData::disconnected (real client departure), or
        //   • CompositorCommand::CloseApp (user-pressed close button).
        // The render thread keeps showing the last cached frame until one
        // of those happens or a new toplevel commits new layers.
        if let Some(ref tl) = data.state.toplevel {
            if !tl.alive() {
                eprintln!(
                    "[compositor] toplevel role destroyed (client still connected); \
                     awaiting new toplevel or client disconnect"
                );
                data.state.toplevel = None;
            }
        }

        // Fire frame callbacks right before flush — this ensures done+delete_id
        // events are flushed immediately, preventing accumulation that causes
        // client-side discards when the client reads multiple pairs at once.
        if data.state.frame_presented {
            data.state.frame_presented = false;
            fire_frame_callbacks(&mut data.state);
        }

        // Flush outgoing events to clients.
        if let Err(e) = data.display.flush_clients() {
            eprintln!("[compositor] flush error: {e}");
        }

        // Block until the next calloop event.
        if let Err(e) = event_loop.dispatch(Some(std::time::Duration::from_millis(16)), &mut data)
        {
            eprintln!("[compositor] event loop error: {e}");
        }
    }
}

// ── Input injection helpers ─────────────────────────────────────────────

fn handle_touch(state: &mut FlipCompositor, touch: TouchEvent) {
    use smithay::backend::input::TouchSlot;
    use smithay::input::touch::{DownEvent, MotionEvent, UpEvent};
    use smithay::utils::{Logical, Point};

    let Some(ref toplevel) = state.toplevel else {
        return;
    };
    // Don't inject input into a dead surface — the client disconnected.
    if !toplevel.alive() {
        return;
    }
    let wl_surface = toplevel.wl_surface().clone();
    let Some(touch_handle) = state.seat.get_touch() else {
        return;
    };

    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    let time = elapsed_ms();
    let location: Point<f64, Logical> = (touch.x, touch.y).into();
    let slot: TouchSlot = Some(touch.slot as u32).into();

    // Surface origin in compositor space — smithay subtracts this from
    // event.location to get surface-local coordinates.
    let surface_origin: Point<f64, Logical> = (0.0, 0.0).into();

    match touch.kind {
        super::TouchEventKind::Down => {
            touch_handle.down(
                state,
                Some((wl_surface, surface_origin)),
                &DownEvent { slot, location, serial, time },
            );
            touch_handle.frame(state);
        }
        super::TouchEventKind::Up => {
            touch_handle.up(state, &UpEvent { slot, serial, time });
            touch_handle.frame(state);
        }
        super::TouchEventKind::Motion => {
            touch_handle.motion(
                state,
                Some((wl_surface, surface_origin)),
                &MotionEvent { slot, location, time },
            );
            touch_handle.frame(state);
        }
    }
}

fn handle_key(state: &mut FlipCompositor, key: KeyEvent) {
    use smithay::backend::input::KeyState;
    use smithay::input::keyboard::{FilterResult, Keycode};

    // Don't inject keys if there's no live toplevel.
    match state.toplevel {
        Some(ref tl) if tl.alive() => {},
        _ => return,
    }

    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    let time = elapsed_ms();

    let key_state = if key.pressed {
        KeyState::Pressed
    } else {
        KeyState::Released
    };

    keyboard.input::<(), _>(
        state,
        Keycode::new(key.keycode),
        key_state,
        serial,
        time,
        |_, _, _| FilterResult::Forward,
    );
}

/// Walk the surface tree and send `done` on all pending frame callbacks.
fn fire_frame_callbacks(state: &mut FlipCompositor) {
    let time = elapsed_ms();
    let mut callback_count = 0u32;

    // Helper: fire callbacks on a single surface tree rooted at `root`.
    let mut fire_tree = |root: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface| {
        smithay::wayland::compositor::with_surface_tree_upward(
            root,
            (),
            |_, _, _| smithay::wayland::compositor::TraversalAction::DoChildren(()),
            |_, data, _| {
                let mut attrs = data.cached_state.get::<smithay::wayland::compositor::SurfaceAttributes>();
                let attrs = attrs.current();
                for callback in attrs.frame_callbacks.drain(..) {
                    callback.done(time);
                    callback_count += 1;
                }
            },
            |_, _, _| true,
        );
    };

    // Fire on the toplevel + subsurface tree (only if still alive).
    if let Some(ref toplevel) = state.toplevel {
        if toplevel.alive() {
            fire_tree(&toplevel.wl_surface().clone());
        }
    }

    // Fire on all popup surfaces so they don't starve.
    for popup in state.xdg_shell_state.popup_surfaces() {
        if popup.alive() {
            fire_tree(&popup.wl_surface().clone());
        }
    }

    if callback_count > 0 {
        eprintln!("[compositor] sent {callback_count} frame callbacks");
    }
}

fn handle_command(state: &mut FlipCompositor, cmd: CompositorCommand) {
    match cmd {
        CompositorCommand::FrameDone => {
            // Just set the flag — actual callback sending happens in the
            // main loop body right before flush_clients() to prevent
            // done+delete_id event accumulation.
            state.frame_presented = true;
        }
        CompositorCommand::LaunchApp { .. } => {
            // App lifecycle handled by the render thread / app_launcher.
        }
        CompositorCommand::CloseApp => {
            state.toplevel = None;
            state.has_toplevel.store(false, Ordering::Relaxed);
            eprintln!("[compositor] CloseApp: toplevel dropped, has_toplevel=false");
        }
    }
}

/// Monotonic milliseconds since an arbitrary epoch.
fn elapsed_ms() -> u32 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(Instant::now);
    epoch.elapsed().as_millis() as u32
}
