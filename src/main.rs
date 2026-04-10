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

use app::{App, UICommand};
use clap::Parser;
use config::Config;
use slint::ComponentHandle;
use tokio::sync::mpsc;

fn main() {
    let config = Config::parse();

    // Create Slint UI on the main thread.
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
            match backend::kwin_window::KWinWindowManager::try_new().await {
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
