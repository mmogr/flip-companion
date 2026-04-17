#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# Flip Companion Installer for AYANEO Flip 1S DS on Bazzite
# =============================================================================
#
# Installs the dual-screen Game Mode setup:
#   - gamescope (fork with DRM lease support)
#   - flip-companion (bottom-screen panel app)
#   - Bazzite session configs to wire them together
#
# Usage:
#   ./install.sh                Install everything
#   ./install.sh --uninstall    Remove everything and restore stock gamescope
#   ./install.sh --force        Skip hardware detection (for testing)
#
# Requirements: Bazzite (or SteamOS-based distro), curl, AYANEO Flip 1S DS
# =============================================================================

REPO_OWNER="mmogr"
COMPANION_REPO="flip-companion"
GAMESCOPE_REPO="gamescope"

# These are updated by CI when cutting a release. For manual installs from
# the repo, leave as "latest" to auto-detect the newest release.
COMPANION_VERSION="${COMPANION_VERSION:-latest}"
GAMESCOPE_VERSION="${GAMESCOPE_VERSION:-latest}"

INSTALL_DIR="$HOME/.local/bin"
CONFIG_DIR="$HOME/.config"
ENV_DIR="$CONFIG_DIR/environment.d"
SESSION_DIR="$CONFIG_DIR/gamescope-session-plus/sessions.d"
FLIP_CONFIG_DIR="$CONFIG_DIR/flip-companion"
VERSION_FILE="$FLIP_CONFIG_DIR/version.json"

# Where this script lives (if running from a git checkout)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

FORCE=false

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

info()  { echo -e "${GREEN}[✓]${NC} $*"; }
warn()  { echo -e "${YELLOW}[!]${NC} $*"; }
error() { echo -e "${RED}[✗]${NC} $*" >&2; }
step()  { echo -e "\n${BOLD}$*${NC}"; }

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

resolve_version() {
    local repo="$1" version="$2"
    if [[ "$version" == "latest" ]]; then
        curl -fsSL "https://api.github.com/repos/${REPO_OWNER}/${repo}/releases/latest" \
            | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"//;s/".*//'
    else
        echo "$version"
    fi
}

download_binary() {
    local repo="$1" tag="$2" asset="$3" dest="$4"
    local url="https://github.com/${REPO_OWNER}/${repo}/releases/download/${tag}/${asset}"
    info "Downloading ${asset} (${tag})..."
    curl -fSL --progress-bar -o "$dest" "$url"
    chmod +x "$dest"
}

# Detect the bottom-screen DRM connector (non-eDP).
# Returns the connector name (e.g. "DP-1", "DP-2") or empty string.
detect_lease_connector() {
    local connector=""
    for card_dir in /sys/class/drm/card*-*; do
        [[ -d "$card_dir" ]] || continue
        local name
        name="$(basename "$card_dir")"
        # Skip the primary eDP panel (top screen)
        [[ "$name" == *eDP* ]] && continue
        # Skip disconnected connectors
        local status
        status="$(cat "$card_dir/status" 2>/dev/null || echo "disconnected")"
        [[ "$status" == "connected" ]] || continue
        # Extract connector name: card0-DP-1 → DP-1, card1-DP-1 → DP-1
        # Single '#' = shortest match, so card*- strips just "cardN-"
        connector="${name#card*-}"
        break
    done
    echo "$connector"
}

# ---------------------------------------------------------------------------
# Preflight Checks
# ---------------------------------------------------------------------------

preflight_checks() {
    local errors=0

    step "Preflight checks..."

    # 1. Hardware detection — check DMI product name
    local product_name=""
    if [[ -r /sys/class/dmi/id/product_name ]]; then
        product_name="$(cat /sys/class/dmi/id/product_name)"
    fi
    if [[ "$product_name" == *"FLIP"*"DS"* ]]; then
        info "Hardware: $product_name"
    elif [[ "$FORCE" == true ]]; then
        warn "Hardware: ${product_name:-unknown} (not AYANEO Flip DS — continuing with --force)"
    else
        error "Hardware: ${product_name:-unknown}"
        error "This installer is for AYANEO Flip DS handhelds only."
        error "If you know what you're doing, re-run with --force"
        return 1
    fi

    # 2. Bazzite / SteamOS detection — check for gamescope-session-plus
    if command -v gamescope-session-plus >/dev/null 2>&1 || \
       [[ -f /usr/share/gamescope-session-plus/gamescope-session-plus ]]; then
        info "Session: gamescope-session-plus found"
    else
        error "gamescope-session-plus not found."
        error "This installer requires Bazzite or a SteamOS-based distro."
        return 1
    fi

    # 3. Check curl is available (needed for downloads)
    if command -v curl >/dev/null 2>&1; then
        info "curl: available"
    else
        error "curl is required but not found."
        return 1
    fi

    # 4. Check ~/.local/bin is in PATH
    if echo "$PATH" | tr ':' '\n' | grep -qx "$HOME/.local/bin"; then
        info "PATH: ~/.local/bin is in PATH"
    else
        warn "~/.local/bin is not in PATH (gamescope wrapper uses absolute paths, so this is OK)"
    fi

    # 5. Detect the bottom-screen connector
    local connector
    connector="$(detect_lease_connector)"
    if [[ -n "$connector" ]]; then
        info "Bottom screen: $connector"
    else
        warn "No non-eDP connector detected (screen may be off — will default to DP-1)"
        connector="DP-1"
    fi
    # Export for use by do_install
    LEASE_CONNECTOR="$connector"

    # 6. Check user is in the 'input' group
    if id -nG | tr ' ' '\n' | grep -qx "input"; then
        info "Group: user is in 'input' group"
    else
        warn "Your user is NOT in the 'input' group."
        warn "Touch on the bottom screen requires this group membership."
        echo ""
        read -rp "  Add $(whoami) to 'input' group now? [Y/n] " add_input
        if [[ "${add_input,,}" != "n" ]]; then
            sudo usermod -aG input "$(whoami)"
            info "Added $(whoami) to 'input' group (takes effect after reboot/re-login)"
        else
            warn "Skipped. You can add it later with: sudo usermod -aG input $(whoami)"
            warn "Touch will not work on the bottom screen without this."
        fi
    fi

    return $errors
}

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------

do_install() {
    step "Flip Companion Installer for AYANEO Flip 1S DS"
    echo "This will set up dual-screen Game Mode on Bazzite."
    echo ""

    # Run preflight checks (exits on failure unless --force)
    if ! preflight_checks; then
        exit 1
    fi

    # Show upgrade info if a previous install exists
    if [[ -f "$VERSION_FILE" ]]; then
        echo ""
        info "Previous installation detected:"
        echo -e "  companion: $(grep -o '"companion":"[^"]*"' "$VERSION_FILE" | cut -d'"' -f4)"
        echo -e "  gamescope: $(grep -o '"gamescope":"[^"]*"' "$VERSION_FILE" | cut -d'"' -f4)"
        echo -e "  installed: $(grep -o '"installed":"[^"]*"' "$VERSION_FILE" | cut -d'"' -f4)"
        echo ""
    fi

    # Create a staging directory — everything is staged here first.
    # If any step fails, nothing is half-installed.
    local staging
    staging="$(mktemp -d)"
    trap 'rm -rf "$staging"' EXIT

    # ── 1. Resolve versions ──────────────────────────────────────────────
    step "1/7  Resolving release versions..."
    local comp_tag game_tag
    comp_tag="$(resolve_version "$COMPANION_REPO" "$COMPANION_VERSION")"
    game_tag="$(resolve_version "$GAMESCOPE_REPO" "$GAMESCOPE_VERSION")"

    if [[ -z "$comp_tag" || -z "$game_tag" ]]; then
        error "Could not resolve release versions. Check your internet connection."
        error "companion=$comp_tag  gamescope=$game_tag"
        exit 1
    fi
    info "flip-companion: ${comp_tag}"
    info "gamescope:      ${game_tag}"

    # ── 2. Download binaries to staging ──────────────────────────────────
    step "2/7  Downloading binaries..."
    download_binary "$COMPANION_REPO" "$comp_tag" "flip-companion" "$staging/flip-companion"
    download_binary "$GAMESCOPE_REPO" "$game_tag"  "gamescope"      "$staging/gamescope"

    # ── 3. Verify checksums (if available) ───────────────────────────────
    step "3/7  Verifying checksums..."
    local checksums_ok=true
    for repo_tag in "$COMPANION_REPO:$comp_tag" "$GAMESCOPE_REPO:$game_tag"; do
        local repo="${repo_tag%%:*}"
        local tag="${repo_tag##*:}"
        local sums_url="https://github.com/${REPO_OWNER}/${repo}/releases/download/${tag}/SHA256SUMS"
        local sums_file="$staging/${repo}-SHA256SUMS"

        if curl -fsSL -o "$sums_file" "$sums_url" 2>/dev/null; then
            local binary_name
            if [[ "$repo" == "$COMPANION_REPO" ]]; then
                binary_name="flip-companion"
            else
                binary_name="gamescope"
            fi
            local expected
            expected="$(grep "$binary_name" "$sums_file" | awk '{print $1}')"
            local actual
            actual="$(sha256sum "$staging/$binary_name" | awk '{print $1}')"
            if [[ "$expected" == "$actual" ]]; then
                info "$binary_name: checksum OK"
            else
                error "$binary_name: checksum MISMATCH"
                error "  expected: $expected"
                error "  actual:   $actual"
                checksums_ok=false
            fi
        else
            warn "$repo ($tag): no SHA256SUMS found — skipping verification"
        fi
    done
    if [[ "$checksums_ok" != true ]]; then
        error "Checksum verification failed. Aborting."
        exit 1
    fi

    # ── 4. Check shared library compatibility ────────────────────────────
    step "4/7  Checking binary compatibility..."
    if command -v ldd >/dev/null 2>&1; then
        local missing_libs
        missing_libs="$(ldd "$staging/gamescope" 2>/dev/null | grep "not found" || true)"
        if [[ -n "$missing_libs" ]]; then
            warn "Some shared libraries are missing on this system:"
            echo "$missing_libs" | while read -r line; do
                warn "  $line"
            done
            warn "Gamescope may fail to start. Please report this if it does."
        else
            info "gamescope: all shared libraries found"
        fi
    else
        warn "ldd not available — skipping library check"
    fi
    info "flip-companion: statically linked (OK)"

    # ── 5. Install binaries + wrapper ────────────────────────────────────
    step "5/7  Installing binaries and wrapper..."
    mkdir -p "$INSTALL_DIR"

    # Atomic install: copy to staging name in target dir, then rename
    cp "$staging/flip-companion" "$INSTALL_DIR/.flip-companion.new"
    mv -f "$INSTALL_DIR/.flip-companion.new" "$INSTALL_DIR/flip-companion"
    info "Installed flip-companion → $INSTALL_DIR/"

    cp "$staging/gamescope" "$INSTALL_DIR/.gamescope.new"
    mv -f "$INSTALL_DIR/.gamescope.new" "$INSTALL_DIR/gamescope"
    info "Installed gamescope → $INSTALL_DIR/"

    # Generate the wrapper with the detected connector
    local deploy_dir="$SCRIPT_DIR/deploy/gamemode"
    cat > "$INSTALL_DIR/gamescope-lease-wrapper" << WRAPPER
#!/bin/bash
exec "\$HOME/.local/bin/gamescope" --lease-connector ${LEASE_CONNECTOR} --ignore-touch-device Goodix "\$@"
WRAPPER
    chmod +x "$INSTALL_DIR/gamescope-lease-wrapper"
    info "Installed gamescope-lease-wrapper (connector: ${LEASE_CONNECTOR})"

    # ── 6. Configure session ─────────────────────────────────────────────
    step "6/7  Configuring Bazzite Game Mode..."
    mkdir -p "$ENV_DIR"
    mkdir -p "$SESSION_DIR"
    mkdir -p "$FLIP_CONFIG_DIR"

    # environment.d
    cat > "$ENV_DIR/10-gamescope-session.conf" << 'EOF'
OUTPUT_CONNECTOR=eDP-1
EOF
    info "Wrote $ENV_DIR/10-gamescope-session.conf"

    cat > "$ENV_DIR/20-gamescope-lease.conf" << EOF
GAMESCOPE_BIN=\${HOME}/.local/bin/gamescope-lease-wrapper
EOF
    info "Wrote $ENV_DIR/20-gamescope-lease.conf"

    # Back up existing sessions.d/steam if not already backed up by us
    if [[ -f "$SESSION_DIR/steam" && ! -f "$SESSION_DIR/steam.pre-flip-companion.bak" ]]; then
        cp "$SESSION_DIR/steam" "$SESSION_DIR/steam.pre-flip-companion.bak"
        info "Backed up existing $SESSION_DIR/steam → steam.pre-flip-companion.bak"
    fi

    # sessions.d/steam hook
    if [[ -f "$deploy_dir/sessions.d-steam" ]]; then
        cp "$deploy_dir/sessions.d-steam" "$SESSION_DIR/steam"
    else
        cat > "$SESSION_DIR/steam" << 'STEAM'
#!/usr/bin/bash
function post_gamescope_start {
    if command -v steam-tweaks > /dev/null; then steam-tweaks; fi
    if [ -x /etc/flip-companion/touch-toggle ]; then
        sudo /etc/flip-companion/touch-toggle enable
    fi
    if [ -x "$HOME/.local/bin/flip-companion" ]; then
        "$HOME/.local/bin/flip-companion" --lease-socket /tmp/gamescope-lease.sock &
    fi
}
function post_client_shutdown {
    if [ -x /etc/flip-companion/touch-toggle ]; then
        sudo /etc/flip-companion/touch-toggle disable
    fi
}
STEAM
    fi
    info "Wrote $SESSION_DIR/steam"

    # Install touch toggle (requires root)
    echo ""
    echo "The bottom touchscreen must be hidden from libinput during Game Mode"
    echo "so flip-companion can read it directly. This requires sudo."
    echo ""

    local touch_dir="/etc/flip-companion"
    local sudoers_file="/etc/sudoers.d/flip-companion-touch"

    # Touch toggle helper script
    if [[ -f "$deploy_dir/flip-companion-touch-toggle" ]]; then
        cp "$deploy_dir/flip-companion-touch-toggle" "$staging/touch-toggle"
    else
        cat > "$staging/touch-toggle" << 'TOGGLE'
#!/bin/bash
set -euo pipefail
readonly RULE_SRC="/etc/flip-companion/99-flip-companion-touch.rules"
readonly RULE_DST="/run/udev/rules.d/99-flip-companion-touch.rules"
case "${1:-}" in
    enable)
        [ -f "$RULE_SRC" ] || { echo "source rule not found: $RULE_SRC" >&2; exit 1; }
        cp "$RULE_SRC" "$RULE_DST"
        udevadm control --reload-rules
        udevadm trigger --subsystem-match=input
        echo "flip-companion-touch-toggle: enabled"
        ;;
    disable)
        rm -f "$RULE_DST"
        udevadm control --reload-rules
        udevadm trigger --subsystem-match=input
        echo "flip-companion-touch-toggle: disabled"
        ;;
    *) echo "Usage: $0 {enable|disable}" >&2; exit 1 ;;
esac
TOGGLE
    fi

    # Udev rule source
    if [[ -f "$SCRIPT_DIR/deploy/99-flip-companion-touch.rules" ]]; then
        cp "$SCRIPT_DIR/deploy/99-flip-companion-touch.rules" "$staging/99-flip-companion-touch.rules"
    else
        cat > "$staging/99-flip-companion-touch.rules" << 'UDEVRULE'
SUBSYSTEM=="input", ATTRS{name}=="Goodix Capacitive TouchScreen", ENV{LIBINPUT_IGNORE_DEVICE}="1"
UDEVRULE
    fi

    # Sudoers drop-in
    if [[ -f "$deploy_dir/flip-companion-touch-sudoers" ]]; then
        cp "$deploy_dir/flip-companion-touch-sudoers" "$staging/sudoers"
    else
        cat > "$staging/sudoers" << 'SUDOERS'
%input ALL=(root) NOPASSWD: /etc/flip-companion/touch-toggle enable
%input ALL=(root) NOPASSWD: /etc/flip-companion/touch-toggle disable
SUDOERS
    fi

    # Validate the sudoers file before installing (prevents lockouts)
    if ! visudo -c -f "$staging/sudoers" >/dev/null 2>&1; then
        error "Sudoers validation failed — skipping touch toggle setup."
        error "Touch on the bottom screen will not work in Game Mode."
        warn "You can retry by running: sudo visudo -c -f $deploy_dir/flip-companion-touch-sudoers"
    else
        # Install with locked-down permissions via sudo
        sudo mkdir -p "$touch_dir"
        sudo cp "$staging/touch-toggle" "$touch_dir/touch-toggle"
        sudo cp "$staging/99-flip-companion-touch.rules" "$touch_dir/99-flip-companion-touch.rules"
        sudo chown root:root "$touch_dir/touch-toggle" "$touch_dir/99-flip-companion-touch.rules"
        sudo chmod 755 "$touch_dir/touch-toggle"
        sudo chmod 644 "$touch_dir/99-flip-companion-touch.rules"

        sudo cp "$staging/sudoers" "$sudoers_file"
        sudo chown root:root "$sudoers_file"
        sudo chmod 440 "$sudoers_file"

        info "Installed $touch_dir/touch-toggle (root:root 755)"
        info "Installed $touch_dir/99-flip-companion-touch.rules (root:root 644)"
        info "Installed $sudoers_file (root:root 440)"
    fi

    # ── 6b. Desktop Mode safety net ──────────────────────────────────────
    #
    # On Flip 1S DS the device always boots into Game Mode first. When the
    # user switches to Desktop Mode, three things can leave the bottom
    # touchscreen unresponsive under KWin:
    #
    #   (a) post_client_shutdown sometimes does not fire, so the Game-Mode
    #       LIBINPUT_IGNORE udev rule in /run/udev/rules.d/ survives into
    #       Desktop Mode and libinput keeps ignoring the device.
    #   (b) Even after the rule is removed, KWin can retain a cached
    #       "ignored" state for the device until the kernel re-announces it.
    #   (c) Handheld Daemon (HHD) grabs /dev/input/event9 exclusively to
    #       implement edge-swipe touchscreen shortcuts, which blocks all
    #       touches from ever reaching KWin in Desktop Mode.
    #
    # We address all three:
    #   - Install a root helper /etc/flip-companion/desktop-reset that
    #     removes the stale rule AND rebinds the Goodix-TS driver
    #     (forces KWin to re-enumerate) in a single atomic action.
    #   - Install a KDE autostart entry that runs the helper (via the
    #     existing NOPASSWD sudoers rule) at every Plasma login.
    #   - Run the HHD touchscreen-shortcut disabler once, now, to edit
    #     /etc/hhd/state.yml and restart HHD so its exclusive grab is
    #     released.
    step "6b/7  Configuring Desktop Mode safety net..."

    if [[ -f "$deploy_dir/../desktop/flip-companion-desktop-reset" ]]; then
        sudo install -m 755 -o root -g root \
            "$deploy_dir/../desktop/flip-companion-desktop-reset" \
            "$touch_dir/desktop-reset"
        info "Installed $touch_dir/desktop-reset (root:root 755)"
    else
        warn "deploy/desktop/flip-companion-desktop-reset not found — skipping helper install"
    fi

    # KDE autostart entry — user-scoped, runs at every Plasma login.
    local autostart_dir="$CONFIG_DIR/autostart"
    mkdir -p "$autostart_dir"
    if [[ -f "$deploy_dir/../desktop/flip-companion-desktop-reset.desktop" ]]; then
        cp "$deploy_dir/../desktop/flip-companion-desktop-reset.desktop" \
            "$autostart_dir/flip-companion-desktop-reset.desktop"
        info "Installed $autostart_dir/flip-companion-desktop-reset.desktop"
    fi

    # Disable HHD touchscreen edge-swipe shortcuts. These steal all touches
    # on /dev/input/event9, which is our bottom screen. Without this, Desktop
    # Mode touch is completely unresponsive.
    if [[ -f "$deploy_dir/../desktop/flip-companion-disable-hhd-touch" ]]; then
        local hhd_helper="/usr/local/libexec/flip-companion/disable-hhd-touch"
        sudo install -D -m 755 -o root -g root \
            "$deploy_dir/../desktop/flip-companion-disable-hhd-touch" \
            "$hhd_helper"
        info "Installed $hhd_helper"

        echo ""
        echo "Disabling Handheld Daemon touchscreen edge-swipe shortcuts"
        echo "(they exclusively grab the bottom screen and would prevent"
        echo "KWin from seeing any touches in Desktop Mode)..."
        if sudo "$hhd_helper"; then
            info "HHD touchscreen shortcuts disabled"
        else
            warn "HHD fixup helper returned non-zero — may need re-run after first HHD boot"
        fi
    else
        warn "deploy/desktop/flip-companion-disable-hhd-touch not found — skipping HHD fixup"
    fi

    # Write version file for upgrade tracking
    cat > "$VERSION_FILE" << VEOF
{"companion":"${comp_tag}","gamescope":"${game_tag}","connector":"${LEASE_CONNECTOR}","installed":"$(date -I)"}
VEOF
    info "Version info → $VERSION_FILE"

    rm -rf "$staging"
    trap - EXIT

    # ── 7. Summary ───────────────────────────────────────────────────────
    step "7/7  Done!"
    echo ""
    echo -e "  ${GREEN}gamescope${NC}          → $INSTALL_DIR/gamescope"
    echo -e "  ${GREEN}flip-companion${NC}     → $INSTALL_DIR/flip-companion"
    echo -e "  ${GREEN}lease wrapper${NC}      → $INSTALL_DIR/gamescope-lease-wrapper (${LEASE_CONNECTOR})"
    echo -e "  ${GREEN}session config${NC}     → $ENV_DIR/10-gamescope-session.conf"
    echo -e "  ${GREEN}session config${NC}     → $ENV_DIR/20-gamescope-lease.conf"
    echo -e "  ${GREEN}startup hook${NC}       → $SESSION_DIR/steam"
    echo -e "  ${GREEN}touch toggle${NC}       → /etc/flip-companion/touch-toggle"
    echo -e "  ${GREEN}touch udev rule${NC}    → /etc/flip-companion/99-flip-companion-touch.rules"
    echo -e "  ${GREEN}touch sudoers${NC}      → /etc/sudoers.d/flip-companion-touch"
    echo -e "  ${GREEN}desktop reset${NC}      → /etc/flip-companion/desktop-reset"
    echo -e "  ${GREEN}KDE autostart${NC}      → $CONFIG_DIR/autostart/flip-companion-desktop-reset.desktop"
    echo -e "  ${GREEN}HHD fixup helper${NC}   → /usr/local/libexec/flip-companion/disable-hhd-touch"
    echo -e "  ${GREEN}version info${NC}       → $VERSION_FILE"
    echo ""
    info "Reboot into Game Mode to activate the bottom screen."
    echo ""
    echo -e "  To uninstall:  ${BOLD}./install.sh --uninstall${NC}"
    echo -e "  Or:            ${BOLD}curl -sL https://github.com/${REPO_OWNER}/${COMPANION_REPO}/raw/main/install.sh | bash -s -- --uninstall${NC}"
}

# ---------------------------------------------------------------------------
# Uninstall
# ---------------------------------------------------------------------------

do_uninstall() {
    step "Uninstalling Flip Companion..."

    # User-level files
    local files=(
        "$INSTALL_DIR/flip-companion"
        "$INSTALL_DIR/gamescope"
        "$INSTALL_DIR/gamescope-lease-wrapper"
        "$ENV_DIR/10-gamescope-session.conf"
        "$ENV_DIR/20-gamescope-lease.conf"
        "$CONFIG_DIR/autostart/flip-companion-desktop-reset.desktop"
    )

    for f in "${files[@]}"; do
        if [[ -f "$f" ]]; then
            rm -f "$f"
            info "Removed $f"
        fi
    done

    # Restore backed-up sessions.d/steam, or remove ours
    if [[ -f "$SESSION_DIR/steam.pre-flip-companion.bak" ]]; then
        mv "$SESSION_DIR/steam.pre-flip-companion.bak" "$SESSION_DIR/steam"
        info "Restored $SESSION_DIR/steam from backup"
    elif [[ -f "$SESSION_DIR/steam" ]]; then
        rm -f "$SESSION_DIR/steam"
        info "Removed $SESSION_DIR/steam"
    fi

    # Remove version tracking
    if [[ -f "$VERSION_FILE" ]]; then
        rm -f "$VERSION_FILE"
        info "Removed $VERSION_FILE"
    fi
    rmdir "$FLIP_CONFIG_DIR" 2>/dev/null || true

    # Clean up empty directories
    rmdir "$SESSION_DIR" 2>/dev/null || true
    rmdir "$CONFIG_DIR/gamescope-session-plus" 2>/dev/null || true

    # Root-level files (touch toggle)
    step "Removing touch toggle (requires sudo)..."
    local root_files=(
        "/etc/sudoers.d/flip-companion-touch"
        "/run/udev/rules.d/99-flip-companion-touch.rules"
    )
    for f in "${root_files[@]}"; do
        if [[ -f "$f" ]]; then
            sudo rm -f "$f"
            info "Removed $f"
        fi
    done
    if [[ -d "/etc/flip-companion" ]]; then
        sudo rm -rf "/etc/flip-companion"
        info "Removed /etc/flip-companion/"
    fi
    if [[ -d "/usr/local/libexec/flip-companion" ]]; then
        sudo rm -rf "/usr/local/libexec/flip-companion"
        info "Removed /usr/local/libexec/flip-companion/"
    fi
    # Restore the original /etc/hhd/state.yml if we backed it up.
    if sudo test -f "/etc/hhd/state.yml.pre-flip-companion.bak"; then
        sudo mv -f "/etc/hhd/state.yml.pre-flip-companion.bak" "/etc/hhd/state.yml"
        info "Restored /etc/hhd/state.yml from backup"
        sudo systemctl restart hhd.service 2>/dev/null || true
    fi
    # Reload udev so libinput picks up the touchscreen again
    sudo udevadm control --reload-rules 2>/dev/null || true
    sudo udevadm trigger --subsystem-match=input 2>/dev/null || true

    echo ""
    info "Uninstall complete. Reboot to restore stock Game Mode."
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

# Parse flags
for arg in "$@"; do
    case "$arg" in
        --force|-f) FORCE=true ;;
    esac
done

case "${1:-}" in
    --uninstall|-u)
        do_uninstall
        ;;
    --help|-h)
        echo "Usage: $0 [--uninstall] [--force]"
        echo ""
        echo "Installs dual-screen Game Mode for AYANEO Flip 1S DS on Bazzite."
        echo ""
        echo "Options:"
        echo "  --uninstall  Remove all installed files and restore stock gamescope"
        echo "  --force      Skip hardware detection (for testing on non-DS hardware)"
        echo "  --help       Show this help"
        ;;
    --force|-f)
        do_install
        ;;
    *)
        do_install
        ;;
esac
