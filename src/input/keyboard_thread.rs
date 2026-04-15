//! Dedicated background thread that reads key events from the AT Translated
//! Set 2 keyboard via evdev and forwards them to the Wayland compositor.
//!
//! Supports EVIOCGRAB toggling: when a Slint TextInput gains focus, the
//! grab is released so the physical keyboard input goes to Slint instead.

use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::JoinHandle;

use crate::compositor::{GrabCommand, KeyEvent};

/// Spawn the keyboard reader thread.
///
/// Returns `None` if no AT Translated Set 2 keyboard is found.
/// The thread reads evdev events, converts them to [`KeyEvent`], and sends
/// them via `key_tx` to the Wayland compositor calloop. It also listens on
/// `grab_rx` to toggle EVIOCGRAB on and off.
pub fn spawn(
    key_tx: calloop::channel::Sender<KeyEvent>,
    grab_rx: mpsc::Receiver<GrabCommand>,
) -> Option<JoinHandle<()>> {
    let path = find_keyboard()?;
    eprintln!("[keyboard] found AT Translated Set 2 keyboard at {path:?}");

    Some(
        std::thread::Builder::new()
            .name("keyboard".into())
            .spawn(move || run(path, key_tx, grab_rx))
            .expect("failed to spawn keyboard thread"),
    )
}

/// Search `/dev/input/` for a device whose name contains "AT Translated Set 2".
fn find_keyboard() -> Option<PathBuf> {
    for entry in std::fs::read_dir("/dev/input").ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if !path.to_str().map_or(false, |s| s.contains("event")) {
            continue;
        }
        if let Ok(dev) = evdev::Device::open(&path) {
            if let Some(name) = dev.name() {
                if name.contains("AT Translated Set 2") {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// Main loop: poll the evdev fd with a 100 ms timeout so we can also service
/// grab commands from the Slint UI thread.
fn run(
    path: PathBuf,
    key_tx: calloop::channel::Sender<KeyEvent>,
    grab_rx: mpsc::Receiver<GrabCommand>,
) {
    let mut dev = match evdev::Device::open(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[keyboard] failed to open {path:?}: {e}");
            return;
        }
    };

    // Start with the keyboard grabbed — physical keys go to the Wayland client.
    if let Err(e) = dev.grab() {
        eprintln!("[keyboard] initial grab failed: {e}");
    } else {
        eprintln!("[keyboard] initial EVIOCGRAB acquired");
    }

    let fd = dev.as_raw_fd();

    loop {
        // ── Service grab commands (non-blocking) ────────────────────────
        while let Ok(cmd) = grab_rx.try_recv() {
            match cmd {
                GrabCommand::Grab => {
                    if let Err(e) = dev.grab() {
                        eprintln!("[keyboard] grab failed: {e}");
                    } else {
                        eprintln!("[keyboard] EVIOCGRAB acquired");
                    }
                }
                GrabCommand::Release => {
                    if let Err(e) = dev.ungrab() {
                        eprintln!("[keyboard] ungrab failed: {e}");
                    } else {
                        eprintln!("[keyboard] EVIOCGRAB released");
                    }
                }
            }
        }

        // ── Poll the evdev fd with a 100 ms timeout ────────────────────
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: single pollfd, valid fd, bounded timeout.
        let ret = unsafe { libc::poll(&mut pfd, 1, 100) };

        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue; // EINTR — just retry
            }
            eprintln!("[keyboard] poll error: {err}");
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }

        if ret == 0 {
            continue; // timeout — loop back to check grab commands
        }

        // ── Read events ─────────────────────────────────────────────────
        match dev.fetch_events() {
            Ok(events) => {
                for ev in events {
                    if ev.event_type() != evdev::EventType::KEY {
                        continue;
                    }
                    // value: 0 = up, 1 = down, 2 = repeat (skip repeats)
                    let pressed = match ev.value() {
                        0 => false,
                        1 => true,
                        _ => continue,
                    };
                    // evdev scancode → XKB keycode (+ 8 offset)
                    let keycode = ev.code() as u32 + 8;
                    let _ = key_tx.send(KeyEvent { keycode, pressed });
                }
            }
            Err(e) => {
                eprintln!("[keyboard] read error: {e}");
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }
}
