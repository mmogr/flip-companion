# Flip Companion

Bottom-screen companion app for the **AYANEO Flip DS** running [Bazzite](https://bazzite.gg/).

Three touch-friendly tabs:

- **Keyboard** — full QWERTY virtual keyboard with shift, injected via evdev uinput
- **Stats** — live CPU, GPU, RAM, battery, and thermals
- **Shuttle** — move windows between the top and bottom screens via KWin

Built with [Slint](https://slint.dev/) (UI) and [Tokio](https://tokio.rs/) (async backend).

## Prerequisites

- Rust 1.75+ and Cargo
- [just](https://github.com/casey/just) (task runner)
- KDE Plasma 6 with KWin (for window shuttle)
- `kpackagetool6` (ships with Plasma 6)
- User must be in the `input` group for virtual keyboard:
  ```
  sudo usermod -aG input $USER
  ```

## Build

```bash
just build           # debug
just build-release   # optimized
```

## Run

```bash
just run-mock            # mock mode — no hardware/Wayland/D-Bus needed
cargo run -- --mock      # same thing
cargo run                # real mode — requires Plasma session
cargo run -- --output eDP-2   # pin the bottom screen to a specific output
```

## Install

Installs the release binary, KWin script, systemd user service, udev rule, and desktop entry:

```bash
just install
```

The udev rule requires `sudo`. After install, log out and back in (or start manually):

```bash
systemctl --user start flip-companion
```

## Uninstall

```bash
just uninstall
```

## Development

```bash
just check    # fmt + clippy + test
just test     # tests only
just fmt      # auto-format
```

## Deploy to Device

```bash
# Bazzite VM
just deploy-vm

# Physical device (also installs KWin script)
just device_ip=10.0.0.5 deploy-device
```

## Project Structure

```
src/
  main.rs              — entry point, wires UI ↔ backend
  app.rs               — backend loop, command handling
  config.rs            — CLI flags (--mock, --output)
  backend/
    evdev_input.rs     — real keyboard via evdev uinput
    kwin_window.rs     — real window manager via KWin D-Bus
    sysinfo_stats.rs   — real stats via sysinfo + sysfs
    mock/              — mock backends for testing
  platform/            — trait definitions (InputInjector, WindowManager, etc.)
  types/               — shared types (WindowId, ShuttleDirection, etc.)
ui/
  main.slint           — root layout with tab bar
  keyboard.slint       — QWERTY keyboard panel
  stats.slint          — system stats panel
  shuttle.slint        — window shuttle panel
  globals.slint        — StatsStore, ShuttleStore globals
kwin-script/           — KWin script for window D-Bus interface
deploy/
  desktop/             — .desktop entry
  systemd/             — systemd user service
  udev/                — uinput permission rule
```

## License

MIT
