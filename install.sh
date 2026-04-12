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
#   ./install.sh              Install everything
#   ./install.sh --uninstall  Remove everything and restore stock gamescope
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

# Where this script lives (if running from a git checkout)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

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

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------

do_install() {
    step "Flip Companion Installer for AYANEO Flip 1S DS"
    echo "This will set up dual-screen Game Mode on Bazzite."
    echo ""

    # Resolve versions
    step "1/6  Resolving release versions..."
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

    # Download binaries
    step "2/6  Downloading binaries..."
    mkdir -p "$INSTALL_DIR"
    download_binary "$COMPANION_REPO" "$comp_tag" "flip-companion" "$INSTALL_DIR/flip-companion"
    download_binary "$GAMESCOPE_REPO" "$game_tag"  "gamescope"      "$INSTALL_DIR/gamescope"

    # Install wrapper script
    step "3/6  Installing gamescope wrapper..."
    local deploy_dir="$SCRIPT_DIR/deploy/gamemode"
    if [[ -f "$deploy_dir/gamescope-lease-wrapper" ]]; then
        cp "$deploy_dir/gamescope-lease-wrapper" "$INSTALL_DIR/gamescope-lease-wrapper"
    else
        # Inline fallback if running from curl|bash (no git checkout)
        cat > "$INSTALL_DIR/gamescope-lease-wrapper" << 'WRAPPER'
#!/bin/bash
exec "$HOME/.local/bin/gamescope" --lease-connector DP-1 --ignore-touch-device Goodix "$@"
WRAPPER
    fi
    chmod +x "$INSTALL_DIR/gamescope-lease-wrapper"
    info "Installed gamescope-lease-wrapper"

    # Install session configs
    step "4/6  Configuring Bazzite Game Mode..."
    mkdir -p "$ENV_DIR"
    mkdir -p "$SESSION_DIR"

    # environment.d
    cat > "$ENV_DIR/10-gamescope-session.conf" << 'EOF'
OUTPUT_CONNECTOR=eDP-1
EOF
    info "Wrote $ENV_DIR/10-gamescope-session.conf"

    cat > "$ENV_DIR/20-gamescope-lease.conf" << EOF
GAMESCOPE_BIN=\${HOME}/.local/bin/gamescope-lease-wrapper
EOF
    info "Wrote $ENV_DIR/20-gamescope-lease.conf"

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
    step "5/6  Setting up touchscreen input routing..."
    echo "The bottom touchscreen must be hidden from libinput during Game Mode"
    echo "so flip-companion can read it directly. This requires sudo."
    echo ""

    local touch_dir="/etc/flip-companion"
    local sudoers_file="/etc/sudoers.d/flip-companion-touch"

    # Prepare the files in a temp directory first, then install atomically
    local staging
    staging="$(mktemp -d)"
    trap "rm -rf '$staging'" EXIT

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

    rm -rf "$staging"
    trap - EXIT

    # Summary
    step "6/6  Done!"
    echo ""
    echo -e "  ${GREEN}gamescope${NC}          → $INSTALL_DIR/gamescope"
    echo -e "  ${GREEN}flip-companion${NC}     → $INSTALL_DIR/flip-companion"
    echo -e "  ${GREEN}lease wrapper${NC}      → $INSTALL_DIR/gamescope-lease-wrapper"
    echo -e "  ${GREEN}session config${NC}     → $ENV_DIR/10-gamescope-session.conf"
    echo -e "  ${GREEN}session config${NC}     → $ENV_DIR/20-gamescope-lease.conf"
    echo -e "  ${GREEN}startup hook${NC}       → $SESSION_DIR/steam"
    echo -e "  ${GREEN}touch toggle${NC}       → /etc/flip-companion/touch-toggle"
    echo -e "  ${GREEN}touch udev rule${NC}    → /etc/flip-companion/99-flip-companion-touch.rules"
    echo -e "  ${GREEN}touch sudoers${NC}      → /etc/sudoers.d/flip-companion-touch"
    echo ""
    warn "Your user must be in the 'input' group (check with: id)"
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
        "$SESSION_DIR/steam"
    )

    for f in "${files[@]}"; do
        if [[ -f "$f" ]]; then
            rm -f "$f"
            info "Removed $f"
        fi
    done

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
    # Reload udev so libinput picks up the touchscreen again
    sudo udevadm control --reload-rules 2>/dev/null || true
    sudo udevadm trigger --subsystem-match=input 2>/dev/null || true

    echo ""
    info "Uninstall complete. Reboot to restore stock Game Mode."
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

case "${1:-}" in
    --uninstall|-u)
        do_uninstall
        ;;
    --help|-h)
        echo "Usage: $0 [--uninstall]"
        echo ""
        echo "Installs dual-screen Game Mode for AYANEO Flip 1S DS on Bazzite."
        echo ""
        echo "Options:"
        echo "  --uninstall  Remove all installed files and restore stock gamescope"
        echo "  --help       Show this help"
        ;;
    *)
        do_install
        ;;
esac
