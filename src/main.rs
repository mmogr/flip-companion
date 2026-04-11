// Flip Companion — bottom-screen panel for AYANEO Flip DS on Bazzite
//
// Architecture:
//   UI thread  ←→  tokio::sync::mpsc channels  ←→  async backend (tokio)
//   Backend pushes updates to UI via slint::invoke_from_event_loop()
//   UI sends commands to backend via mpsc::Sender
//   Never block the Slint event loop with async calls.

mod app;
pub mod backend;
mod config;
pub mod platform;
pub mod types;

use app::{App, StatsStore, UICommand};
use clap::Parser;
use config::Config;
use platform::stats::StatsProvider;
use slint::ComponentHandle;
use tokio::sync::mpsc;

fn main() {
    let config = Config::parse();

    // Game Mode: receive DRM lease fd from Gamescope and render directly.
    if let Some(ref socket_path) = config.lease_socket {
        let path = if socket_path.is_empty() {
            backend::drm_lease::DEFAULT_SOCKET_PATH
        } else {
            socket_path.as_str()
        };
        run_game_mode(path);
        return;
    }

    // Desktop Mode: Slint UI on a Wayland/X11 window.
    let app = App::new().expect("failed to create Slint app");
    let app_weak = app.as_weak();

    // Channel: UI → backend commands.
    let (tx, rx) = mpsc::channel::<UICommand>(100);

    // Wire UI callbacks to send commands via the channel.
    wire_callbacks(&app, tx);

    // Spawn the async backend in a background thread.
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
        rt.block_on(async {
            let (stats, input, windows) = create_backends(&config).await;
            app::backend_loop(rx, app_weak, stats, input, windows).await;
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

    app.on_request_refresh(move || {
        let _ = tx.try_send(UICommand::RefreshWindows);
    });
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
fn run_game_mode(socket_path: &str) {
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

    // Create the same App UI as desktop mode — Slint renders it into DRM buffers.
    let app = App::new().expect("failed to create Slint app (game mode)");

    // Default to the Stats tab (index 1) since touch isn't working yet.
    app.set_active_tab(1);

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
    backend::drm_platform::set_stats_callback(Box::new(move |snap| {
        if let Some(app) = store_app.upgrade() {
            let store = app.global::<StatsStore>();
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
