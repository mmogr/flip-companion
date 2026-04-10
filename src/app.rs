use std::time::Duration;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tokio::sync::mpsc;

use crate::platform::input::InputInjector;
use crate::platform::stats::StatsProvider;
use crate::platform::window::WindowManager;
use crate::types::window::ShuttleDirection;

slint::include_modules!();

/// Commands sent from the UI thread to the async backend via mpsc.
#[derive(Debug)]
pub enum UICommand {
    KeyPressed(String),
    ShuttleWindow { id: String, direction: String },
    RefreshWindows,
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
) {
    let mut stats_interval = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = stats_interval.tick() => {
                handle_stats_tick(&mut *stats, &app_weak).await;
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
                    None => break, // UI closed, channel dropped
                }
            }
        }
    }
}

async fn handle_stats_tick(stats: &mut dyn StatsProvider, app_weak: &slint::Weak<App>) {
    match stats.snapshot().await {
        Ok(snap) => {
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
