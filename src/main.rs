// Flip Companion — bottom-screen panel for AYANEO Flip DS on Bazzite
//
// Architecture:
//   UI thread  ←→  tokio::sync::mpsc channels  ←→  async backend (tokio)
//   Backend pushes updates to UI via slint::invoke_from_event_loop()
//   UI sends commands to backend via mpsc::Sender
//   Never block the Slint event loop with async calls.

mod app;
pub mod backend;
pub mod compositor;
mod config;
pub mod input;
pub mod platform;
mod stats_history;
pub mod types;

use app::{App, AppEntryData, AppStore, StatsStore, UICommand};
use clap::Parser;
use config::Config;
use platform::stats::StatsProvider;
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::sync::mpsc;

fn main() {
    // Install a custom panic hook that logs thread name + message.
    // This helps diagnose which thread crashed when the DRM buffer
    // freezes on screen after a panic.
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        let location = info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())).unwrap_or_default();
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Box<dyn Any>".to_string()
        };
        eprintln!("[PANIC] thread '{name}' at {location}: {payload}");
    }));

    let config = Config::parse();

    // Game Mode: receive DRM lease fd from Gamescope and render directly.
    if let Some(ref socket_path) = config.lease_socket {
        let path = if socket_path.is_empty() {
            backend::drm_lease::DEFAULT_SOCKET_PATH
        } else {
            socket_path.as_str()
        };
        run_game_mode(path, &config);
        return;
    }

    // Desktop Mode: Slint UI on a Wayland/X11 window.
    let app = App::new().expect("failed to create Slint app");
    let app_weak = app.as_weak();

    // Load apps config and populate the AppStore.
    let apps = config::load_apps(&config);
    populate_app_store(&app, &apps);

    // Channel: UI → backend commands.
    let (tx, rx) = mpsc::channel::<UICommand>(100);

    // Wire UI callbacks to send commands via the channel.
    wire_callbacks(&app, tx);

    // Spawn the async backend in a background thread.
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
        rt.block_on(async {
            let (stats, input, windows) = create_backends(&config).await;
            app::backend_loop(rx, app_weak, stats, input, windows, apps).await;
        });
    });

    // Run the Slint event loop (blocks until window closes).
    app.run().expect("Slint event loop failed");
}

fn wire_callbacks(app: &App, tx: mpsc::Sender<UICommand>) {
    let tx_key = tx.clone();
    app.on_key_pressed(move |key| {
        let _ = tx_key.try_send(UICommand::KeyPressed(key.to_string()));
    });

    let tx_shuttle = tx.clone();
    app.on_shuttle_window(move |id, direction| {
        let _ = tx_shuttle.try_send(UICommand::ShuttleWindow {
            id: id.to_string(),
            direction: direction.to_string(),
        });
    });

    let tx_refresh = tx.clone();
    app.on_request_refresh(move || {
        let _ = tx_refresh.try_send(UICommand::RefreshWindows);
    });

    let tx_launch = tx.clone();
    app.on_launch_app(move |index| {
        let _ = tx_launch.try_send(UICommand::LaunchApp { index });
    });

    app.on_close_app(move || {
        let _ = tx.try_send(UICommand::CloseApp);
    });
}

/// Populate the Slint AppStore global with entries from the config.
fn populate_app_store(app: &App, apps: &[config::AppEntry]) {
    let entries: Vec<AppEntryData> = apps
        .iter()
        .map(|a| AppEntryData {
            name: slint::SharedString::from(&a.name),
            icon: slint::SharedString::from(&a.icon),
            exec: slint::SharedString::from(&a.exec),
            url: slint::SharedString::from(a.url.as_deref().unwrap_or("")),
        })
        .collect();
    let store = app.global::<AppStore>();
    store.set_apps(ModelRc::new(VecModel::from(entries)));
}

async fn create_backends(
    config: &Config,
) -> (
    Box<dyn platform::stats::StatsProvider>,
    Box<dyn platform::input::InputInjector>,
    Box<dyn platform::window::WindowManager>,
) {
    if config.mock {
        (
            Box::new(backend::mock::stats::MockStatsProvider::new()),
            Box::new(backend::mock::input::MockInputInjector),
            Box::new(backend::mock::window::MockWindowManager::new()),
        )
    } else {
        let input: Box<dyn platform::input::InputInjector> =
            match backend::evdev_input::EvdevInputInjector::try_new() {
                Ok(injector) => Box::new(injector),
                Err(e) => {
                    eprintln!("[warn] evdev init failed: {e}, falling back to mock input");
                    Box::new(backend::mock::input::MockInputInjector)
                }
            };

        let windows: Box<dyn platform::window::WindowManager> =
            match backend::kwin_window::KWinWindowManager::try_new(config.output.clone()).await {
                Ok(wm) => Box::new(wm),
                Err(e) => {
                    eprintln!(
                        "[warn] KWin D-Bus init failed: {e}. \
                         Ensure the flip-companion KWin script is installed via \
                         kpackagetool6 and enabled in KWin. \
                         Falling back to mock windows."
                    );
                    Box::new(backend::mock::window::MockWindowManager::new())
                }
            };

        (
            Box::new(backend::sysinfo_stats::SysinfoStatsProvider::new()),
            input,
            windows,
        )
    }
}

/// Game Mode entry point: receive a DRM lease fd from Gamescope and
/// render the companion UI directly to the leased display.
fn run_game_mode(socket_path: &str, config: &Config) {
    use std::os::fd::AsRawFd;

    eprintln!("[game-mode] connecting to '{socket_path}'...");

    let lease_fd = match backend::drm_lease::receive_lease_fd(socket_path) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("[game-mode] failed to receive lease fd: {e}");
            std::process::exit(1);
        }
    };

    eprintln!("[game-mode] received DRM lease fd {}", lease_fd.as_raw_fd());

    // Install SIGABRT handler BEFORE any GPU operations. This intercepts
    // mesa's abort() on GPU context reset and kills only the worker thread.
    backend::abort_guard::install_abort_guard();

    match backend::drm_lease::verify_drm_fd(&lease_fd) {
        Ok(driver) => eprintln!("[game-mode] DRM driver: {driver}"),
        Err(e) => eprintln!("[game-mode] warning: could not verify DRM fd: {e}"),
    }

    // Set up the DRM platform for Slint's software renderer.
    let platform = match backend::drm_platform::DrmPlatform::new(lease_fd) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[game-mode] failed to create DRM platform: {e}");
            std::process::exit(1);
        }
    };

    slint::platform::set_platform(Box::new(platform))
        .expect("failed to set Slint platform");

    // Create key-press channel (must happen on this thread before run_event_loop).
    let key_tx = backend::drm_platform::create_key_channel();

    // Create keyboard grab channel for focus-based grab toggling.
    let grab_tx = backend::drm_platform::create_grab_channel();

    // Create the same App UI as desktop mode — Slint renders it into DRM buffers.
    let app = App::new().expect("failed to create Slint app (game mode)");

    // Load apps config and populate the AppStore.
    let apps = config::load_apps(config);
    populate_app_store(&app, &apps);

    // Default to the Keyboard tab (index 0) — touch is working now.
    app.set_active_tab(0);

    // Wire the tab-change callback so the render loop knows which tab is active.
    app.on_active_tab_changed(move |tab| {
        backend::drm_platform::set_active_tab(tab);
    });

    // Wire the keyboard callback to send key names through the channel.
    app.on_key_pressed(move |key| {
        let _ = key_tx.send(key.to_string());
    });

    // Wire the UI focus callback to toggle keyboard grab.
    // When a Slint TextInput gains focus, release the grab so the physical
    // keyboard sends input to Slint. When it loses focus, re-grab so
    // the Wayland client receives keyboard events.
    app.on_ui_focus_changed(move |is_slint_focused| {
        use crate::compositor::GrabCommand;
        if is_slint_focused {
            let _ = grab_tx.send(GrabCommand::Release);
        } else {
            let _ = grab_tx.send(GrabCommand::Grab);
        }
    });

    // Wire app launch callback — spawn the process directly (no async backend in game mode).
    let launch_apps = apps.clone();
    let launch_app_handle = app.as_weak();
    app.on_launch_app(move |index| {
        eprintln!("[game-mode] on_launch_app called with index={index}");
        if let Some(entry) = launch_apps.get(index as usize) {
            eprintln!("[game-mode] launching app: {} ({})", entry.name, entry.exec);

            // Update AppStore UI state
            if let Some(handle) = launch_app_handle.upgrade() {
                let store = handle.global::<AppStore>();
                store.set_is_running(true);
                store.set_running_app_name(entry.name.clone().into());
            }

            let mut parts = entry.exec.split_whitespace();
            if let Some(cmd) = parts.next() {
                let args: Vec<&str> = parts.collect();

                // For flatpak: expose our compositor socket directory and
                // redirect WAYLAND_DISPLAY so the app connects to us
                // instead of gamescope's security-context proxy.
                use std::os::unix::process::CommandExt;
                let mut command = std::process::Command::new(cmd);
                // Create a new process group so we can SIGTERM the whole tree.
                command.process_group(0);
                if cmd == "flatpak" && args.first() == Some(&"run") {
                    command.arg("run");
                    // Extract and store the Flatpak app ID (first non-flag arg after "run")
                    // so kill_app() can use `flatpak kill <app-id>`.
                    if let Some(app_id) = args[1..].iter().find(|a| !a.starts_with("--")) {
                        eprintln!("[game-mode] flatpak app id: {app_id}");
                        backend::drm_platform::set_app_flatpak_id(app_id.to_string());
                    }
                    // Expose the flip-wayland/ directory containing our
                    // compositor socket (individual socket files can't be
                    // bind-mounted reliably via --filesystem).
                    command.arg("--filesystem=xdg-run/flip-wayland");
                    // Redirect Wayland connection to our compositor.
                    // libwayland resolves: $XDG_RUNTIME_DIR/flip-wayland/wayland-0
                    command.arg("--env=WAYLAND_DISPLAY=flip-wayland/wayland-0");
                    command.arg("--nosocket=x11");
                    command.arg("--nosocket=fallback-x11");
                    command.arg("--env=GDK_BACKEND=wayland");
                    command.arg("--env=MOZ_ENABLE_WAYLAND=1");
                    command.arg("--env=WAYLAND_DEBUG=1");
                    command.arg("--unset-env=DISPLAY");
                    for arg in &args[1..] {
                        command.arg(arg);
                    }
                    eprintln!("[game-mode] flatpak command: {:?}", command);
                } else {
                    command.args(&args);
                    command.env("WAYLAND_DISPLAY", "flip-wayland/wayland-0");
                }

                match command.spawn()
                {
                    Ok(child) => {
                        let pid = child.id();
                        eprintln!("[game-mode] spawned pid {pid}");
                        backend::drm_platform::set_app_pid(pid);
                    }
                    Err(e) => eprintln!("[game-mode] failed to launch: {e}"),
                }
            }
        }
    });

    // Wire close-app callback to kill the app and reset state.
    let close_app_handle = app.as_weak();
    app.on_close_app(move || {
        eprintln!("[game-mode] on_close_app called");
        // Kill the app process (SIGTERM → SIGKILL escalation).
        backend::drm_platform::kill_app();
        // Tell the compositor to drop the toplevel.
        backend::drm_platform::send_compositor_command(
            compositor::CompositorCommand::CloseApp,
        );
        // Reset UI state.
        if let Some(handle) = close_app_handle.upgrade() {
            let store = handle.global::<AppStore>();
            store.set_is_running(false);
            store.set_running_app_name("".into());
        }
    });

    // Shared state for stats: the background thread writes, the render loop reads.
    // We can't use upgrade_in_event_loop because our custom Platform doesn't
    // implement EventLoopProxy. Instead, the render loop polls this directly.
    let shared_snap = std::sync::Arc::new(std::sync::Mutex::new(
        None::<crate::types::stats::SystemSnapshot>,
    ));

    // Spawn a background thread to poll system stats.
    let snap_writer = shared_snap.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(async {
            let mut stats = backend::sysinfo_stats::SysinfoStatsProvider::new();
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                interval.tick().await;
                match stats.snapshot().await {
                    Ok(snap) => {
                        *snap_writer.lock().unwrap() = Some(snap);
                    }
                    Err(e) => eprintln!("[game-mode] stats error: {e}"),
                }
            }
        });
    });

    // Register the shared snapshot with the DRM platform so the render loop
    // can read it and update Slint properties directly on the UI thread.
    backend::drm_platform::set_stats_snapshot(shared_snap);

    // Register a callback that the render loop calls (on this thread) to
    // push snapshot data into Slint StatsStore properties.
    let store_app = app.as_weak();
    let history = std::cell::RefCell::new(stats_history::StatsHistory::new());
    backend::drm_platform::set_stats_callback(Box::new(move |snap| {
        if let Some(app) = store_app.upgrade() {
            let store = app.global::<StatsStore>();

            let mem_pct = if snap.memory.total_bytes > 0 {
                snap.memory.used_bytes as f32 / snap.memory.total_bytes as f32 * 100.0
            } else {
                0.0
            };
            {
                let mut h = history.borrow_mut();
                h.push(
                    snap.cpu.usage_percent,
                    snap.gpu.usage_percent.unwrap_or(0.0),
                    mem_pct,
                );
                store.set_clock_text(slint::SharedString::from(stats_history::clock_text()));
                store.set_cpu_history(slint::ModelRc::new(slint::VecModel::from(h.cpu_history())));
                store.set_gpu_history(slint::ModelRc::new(slint::VecModel::from(h.gpu_history())));
                store.set_mem_history(slint::ModelRc::new(slint::VecModel::from(h.mem_history())));
            }

            store.set_cpu_usage(snap.cpu.usage_percent);
            store.set_cpu_temp(snap.cpu.temp_celsius.unwrap_or(0.0));
            store.set_gpu_usage(snap.gpu.usage_percent.unwrap_or(0.0));
            store.set_gpu_temp(snap.gpu.temp_celsius.unwrap_or(0.0));
            store.set_mem_used_gb(snap.memory.used_bytes as f32 / 1_073_741_824.0);
            store.set_mem_total_gb(snap.memory.total_bytes as f32 / 1_073_741_824.0);
            store.set_battery_percent(snap.battery.charge_percent.unwrap_or(0.0));
            store.set_battery_charging(snap.battery.charging);
        }
    }));

    eprintln!("[game-mode] Slint UI created, entering render loop...");

    app.run().expect("Slint event loop failed (game mode)");
}
