use std::time::Duration;

use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Notification sent when a launched app exits.
#[derive(Debug)]
pub struct AppExited {
    pub name: String,
    pub status: Option<i32>,
}

/// Manages a single child app process.
///
/// Design: the Child is moved into a background watcher task that calls
/// `child.wait()`. The launcher retains only the PID (for signaling) and
/// the task handle (for cancellation). This avoids the `&mut Child` split
/// problem — the watcher exclusively owns the Child, and close() uses
/// raw `kill(2)` via the stored PID.
pub struct AppLauncher {
    pid: Option<u32>,
    app_name: String,
    watcher: Option<JoinHandle<()>>,
    exit_tx: mpsc::Sender<AppExited>,
}

impl AppLauncher {
    pub fn new(exit_tx: mpsc::Sender<AppExited>) -> Self {
        Self {
            pid: None,
            app_name: String::new(),
            watcher: None,
            exit_tx,
        }
    }

    /// Whether an app is currently running.
    pub fn is_running(&self) -> bool {
        self.pid.is_some()
    }

    /// The name of the currently running app, if any.
    pub fn running_name(&self) -> Option<&str> {
        if self.is_running() {
            Some(&self.app_name)
        } else {
            None
        }
    }

    /// Spawn a new app from a shell command string.
    /// If an app is already running, it is closed first.
    pub async fn launch(&mut self, name: &str, exec: &str, url: Option<&str>) {
        if self.is_running() {
            self.close().await;
        }

        let full_cmd = if let Some(url) = url.filter(|u| !u.is_empty()) {
            format!("{exec} {url}")
        } else {
            exec.to_string()
        };

        eprintln!("[launcher] spawning: {full_cmd}");

        // Use setsid via `sh -c` + process_group(0) to create a new session,
        // so SIGTERM to the process group kills the entire tree.
        let result = Command::new("sh")
            .arg("-c")
            .arg(&full_cmd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .process_group(0)
            .kill_on_drop(false)
            .spawn();

        match result {
            Ok(mut child) => {
                let pid = child.id();
                self.pid = pid;
                self.app_name = name.to_string();

                eprintln!(
                    "[launcher] started '{name}' (pid: {})",
                    pid.map_or("unknown".to_string(), |p| p.to_string())
                );

                // Move child into a watcher task that waits for exit.
                let exit_name = name.to_string();
                let exit_tx = self.exit_tx.clone();
                self.watcher = Some(tokio::spawn(async move {
                    let status = child.wait().await;
                    let code = status.ok().and_then(|s| s.code());
                    eprintln!("[launcher] '{exit_name}' exited (code: {code:?})");
                    let _ = exit_tx
                        .send(AppExited {
                            name: exit_name,
                            status: code,
                        })
                        .await;
                }));
            }
            Err(e) => {
                eprintln!("[launcher] failed to spawn '{name}': {e}");
                let _ = self.exit_tx.try_send(AppExited {
                    name: name.to_string(),
                    status: None,
                });
            }
        }
    }

    /// Graceful shutdown: SIGTERM to process group → wait 2s → SIGKILL.
    pub async fn close(&mut self) {
        let Some(pid) = self.pid.take() else {
            return;
        };

        let name = std::mem::take(&mut self.app_name);
        eprintln!("[launcher] closing '{name}' (pid: {pid})...");

        // Send SIGTERM to the process group (negative PID).
        // SAFETY: sending a signal to a process group we created.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }

        // Wait for the watcher task (which owns the Child) to complete.
        if let Some(watcher) = self.watcher.take() {
            match tokio::time::timeout(Duration::from_secs(2), watcher).await {
                Ok(_) => {
                    eprintln!("[launcher] '{name}' exited after SIGTERM");
                    return;
                }
                Err(_timeout) => {
                    eprintln!(
                        "[launcher] '{name}' did not exit after SIGTERM, sending SIGKILL"
                    );
                }
            }

            // SIGKILL the process group.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }

            // Brief wait for the kernel to reap.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // The watcher task will observe the exit and send AppExited.
    }

    /// Called when we receive an AppExited notification — clears local state.
    pub fn on_exited(&mut self) {
        self.pid = None;
        self.app_name.clear();
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
        }
    }
}

impl Drop for AppLauncher {
    fn drop(&mut self) {
        // Best-effort cleanup: SIGTERM the process group if still running.
        if let Some(pid) = self.pid.take() {
            eprintln!("[launcher] drop: sending SIGTERM to pid {pid}");
            unsafe {
                libc::kill(-(pid as i32), libc::SIGTERM);
            }
        }
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
        }
    }
}

