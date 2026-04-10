# Flip Companion — development task runner
# https://github.com/casey/just

set shell := ["bash", "-cu"]

# Default connection settings (override on CLI: just vm_ip=10.0.0.5 deploy-vm)
vm_ip     := "192.168.122.100"
vm_user   := "bazzite"
device_ip := ""
device_user := "bazzite"

# ── Build ──────────────────────────────────────────────────────

# Build debug binary
build:
    cargo build

# Build optimized release binary
build-release:
    cargo build --release

# ── Quality ────────────────────────────────────────────────────

# Run all checks: format, lint, test
check: fmt-check clippy test

# Run clippy lints (deny all warnings)
clippy:
    cargo clippy -- -D warnings

# Run test suite
test:
    cargo test

# Verify formatting without modifying files
fmt-check:
    cargo fmt -- --check

# Auto-format all source files
fmt:
    cargo fmt

# ── Run ────────────────────────────────────────────────────────

# Run in mock mode — no Wayland, D-Bus, or hardware required
run-mock:
    cargo run -- --mock

# Run mock mode with release optimizations
run-mock-release:
    cargo run --release -- --mock

# ── Deploy ─────────────────────────────────────────────────────

# Deploy release binary to Bazzite VM
deploy-vm: build-release
    scp target/release/flip-companion {{ vm_user }}@{{ vm_ip }}:~/
    @echo "✓ Deployed to {{ vm_user }}@{{ vm_ip }}:~/flip-companion"

# Deploy release binary + KWin script to physical device
deploy-device: build-release
    @test -n "{{ device_ip }}" || { echo "ERROR: set device_ip — just device_ip=10.0.0.5 deploy-device"; exit 1; }
    scp target/release/flip-companion {{ device_user }}@{{ device_ip }}:~/
    scp -r kwin-script {{ device_user }}@{{ device_ip }}:~/flip-companion-kwin-script
    ssh {{ device_user }}@{{ device_ip }} '\
        kpackagetool6 --type KWin/Script -u ~/flip-companion-kwin-script 2>/dev/null; \
        kpackagetool6 --type KWin/Script -i ~/flip-companion-kwin-script'
    @echo "✓ Deployed to {{ device_user }}@{{ device_ip }}"

# Install KWin script on the local machine
install-kwin-script:
    kpackagetool6 --type KWin/Script -u kwin-script 2>/dev/null; \
    kpackagetool6 --type KWin/Script -i kwin-script

# ── Clean ──────────────────────────────────────────────────────

# Remove build artifacts
clean:
    cargo clean

# ── Install / Uninstall ────────────────────────────────────────

# Install everything locally (binary, KWin script, systemd, udev, desktop)
install: build-release
    mkdir -p ~/.local/bin
    cp target/release/flip-companion ~/.local/bin/
    kpackagetool6 --type KWin/Script -u kwin-script 2>/dev/null || true
    kpackagetool6 --type KWin/Script -i kwin-script
    mkdir -p ~/.config/systemd/user
    cp deploy/systemd/flip-companion.service ~/.config/systemd/user/
    systemctl --user daemon-reload
    systemctl --user enable flip-companion.service
    mkdir -p ~/.local/share/applications
    cp deploy/desktop/flip-companion.desktop ~/.local/share/applications/
    sudo cp deploy/udev/99-uinput.rules /etc/udev/rules.d/
    sudo udevadm control --reload
    @echo "✓ Installed. Log out/in or run: systemctl --user start flip-companion"

# Remove everything installed by 'just install'
uninstall:
    systemctl --user disable --now flip-companion.service 2>/dev/null || true
    rm -f ~/.local/bin/flip-companion
    rm -f ~/.config/systemd/user/flip-companion.service
    systemctl --user daemon-reload
    rm -f ~/.local/share/applications/flip-companion.desktop
    kpackagetool6 --type KWin/Script -r flip-companion-shuttle 2>/dev/null || true
    sudo rm -f /etc/udev/rules.d/99-uinput.rules
    sudo udevadm control --reload
    @echo "✓ Uninstalled."
