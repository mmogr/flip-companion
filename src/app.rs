use std::time::Duration;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tokio::sync::mpsc;

use crate::backend::app_launcher::{AppExited, AppLauncher};
use crate::config::AppEntry;
use crate::platform::input::InputInjector;
use crate::platform::stats::StatsProvider;
use crate::platform::window::WindowManager;
use crate::stats_history::StatsHistory;
use crate::types::window::ShuttleDirection;

slint::include_modules!();

/// Commands sent from the UI thread to the async backend via mpsc.
#[derive(Debug)]
pub enum UICommand {
    KeyPressed(String),
    ShuttleWindow { id: String, direction: String },
    RefreshWindows,
    LaunchApp { index: i32 },
    CloseApp,
}

/// Run the async backend loop.
///
/// Receives commands from the UI, polls stats on a timer, and pushes
/// updates back to the Slint UI via `upgrade_in_event_loop`.
pub async fn backend_loop(
    mut rx: mpsc::Receiver<UICommand>,
    app_weak: slint::Weak<App>,
    mut stats: Box<dyn StatsProvider>,
    input: Box<dyn InputInjector>,
    windows: Box<dyn WindowManager>,
    apps: Vec<AppEntry>,
) {
    let mut stats_interval = tokio::time::interval(Duration::from_secs(1));
    let mut history = StatsHistory::new();

    // App launcher + exit notification channel.
    let (exit_tx, mut exit_rx) = mpsc::channel::<AppExited>(4);
    let mut launcher = AppLauncher::new(exit_tx);

    loop {
        tokio::select! {
            _ = stats_interval.tick() => {
                handle_stats_tick(&mut *stats, &app_weak, &mut history).await;
            }
            Some(exited) = exit_rx.recv() => {
                eprintln!("[apps] app '{}' exited (code: {:?})", exited.name, exited.status);
                launcher.on_exited();
                set_app_running(&app_weak, false, "");
            }
            cmd = rx.recv() => {
                match cmd {
                    Some(UICommand::KeyPressed(key)) => {
                        handle_key_pressed(&*input, &key).await;
                    }
                    Some(UICommand::ShuttleWindow { id, direction }) => {
                        handle_shuttle(&*windows, &id, &direction).await;
                    }
                    Some(UICommand::RefreshWindows) => {
                        handle_refresh_windows(&*windows, &app_weak).await;
                    }
                    Some(UICommand::LaunchApp { index }) => {
                        handle_launch_app(&mut launcher, &apps, index, &app_weak).await;
                    }
                    Some(UICommand::CloseApp) => {
                        launcher.close().await;
                        // The watcher task will send AppExited, which resets the UI.
                    }
                    None => break,
                }
            }
        }
    }

    // Cleanup: close any running app before exiting.
    launcher.close().await;
}

async fn handle_stats_tick(
    stats: &mut dyn StatsProvider,
    app_weak: &slint::Weak<App>,
    history: &mut StatsHistory,
) {
    match stats.snapshot().await {
        Ok(snap) => {
            let mem_pct = if snap.memory.total_bytes > 0 {
                snap.memory.used_bytes as f32 / snap.memory.total_bytes as f32 * 100.0
            } else {
                0.0
            };
            history.push(
                snap.cpu.usage_percent,
                snap.gpu.usage_percent.unwrap_or(0.0),
                mem_pct,
            );

            let clock = SharedString::from(crate::stats_history::clock_text());
            let cpu_hist = history.cpu_history();
            let gpu_hist = history.gpu_history();
            let mem_hist = history.mem_history();

            let _ = app_weak.upgrade_in_event_loop(move |app| {
                let store = app.global::<StatsStore>();
                store.set_cpu_usage(snap.cpu.usage_percent);
                store.set_cpu_temp(snap.cpu.temp_celsius.unwrap_or(0.0));
                store.set_gpu_usage(snap.gpu.usage_percent.unwrap_or(0.0));
                store.set_gpu_temp(snap.gpu.temp_celsius.unwrap_or(0.0));
                store.set_mem_used_gb(snap.memory.used_bytes as f32 / 1_073_741_824.0);
                store.set_mem_total_gb(snap.memory.total_bytes as f32 / 1_073_741_824.0);
                store.set_battery_percent(snap.battery.charge_percent.unwrap_or(0.0));
                store.set_battery_charging(snap.battery.charging);
                store.set_clock_text(clock);
                store.set_cpu_history(ModelRc::new(VecModel::from(cpu_hist)));
                store.set_gpu_history(ModelRc::new(VecModel::from(gpu_hist)));
                store.set_mem_history(ModelRc::new(VecModel::from(mem_hist)));
            });
        }
        Err(e) => eprintln!("[stats] error: {e}"),
    }
}

async fn handle_key_pressed(input: &dyn InputInjector, key: &str) {
    if let Err(e) = input.press_key(key).await {
        eprintln!("[input] error pressing key {key:?}: {e}");
    }
}

async fn handle_shuttle(windows: &dyn WindowManager, id: &str, direction: &str) {
    let dir = match direction {
        "up" => ShuttleDirection::Up,
        "down" => ShuttleDirection::Down,
        other => {
            eprintln!("[shuttle] unknown direction: {other:?}");
            return;
        }
    };
    let window_id = crate::types::window::WindowId(id.to_string());
    if let Err(e) = windows.shuttle_window(&window_id, dir).await {
        eprintln!("[shuttle] error: {e}");
    }
}

async fn handle_refresh_windows(windows: &dyn WindowManager, app_weak: &slint::Weak<App>) {
    match windows.list_windows().await {
        Ok(window_list) => {
            let entries: Vec<WindowEntry> = window_list
                .into_iter()
                .map(|w| WindowEntry {
                    id: SharedString::from(&w.id.0),
                    caption: SharedString::from(&w.caption),
                    output: SharedString::from(w.output.as_ref().map_or("", |o| o.0.as_str())),
                })
                .collect();

            let _ = app_weak.upgrade_in_event_loop(move |app| {
                let store = app.global::<ShuttleStore>();
                store.set_windows(ModelRc::new(VecModel::from(entries)));
            });
        }
        Err(e) => eprintln!("[shuttle] refresh error: {e}"),
    }
}

async fn handle_launch_app(
    launcher: &mut AppLauncher,
    apps: &[AppEntry],
    index: i32,
    app_weak: &slint::Weak<App>,
) {
    let idx = index as usize;
    let Some(entry) = apps.get(idx) else {
        eprintln!("[apps] invalid app index: {index}");
        return;
    };

    let url = entry.url.as_deref();
    launcher.launch(&entry.name, &entry.exec, url).await;

    if launcher.is_running() {
        set_app_running(app_weak, true, &entry.name);
    }
}

fn set_app_running(app_weak: &slint::Weak<App>, running: bool, name: &str) {
    let name = SharedString::from(name);
    let _ = app_weak.upgrade_in_event_loop(move |app| {
        let store = app.global::<AppStore>();
        store.set_is_running(running);
        store.set_running_app_name(name);
    });
}
