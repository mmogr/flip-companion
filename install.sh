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
    step "1/5  Resolving release versions..."
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
    step "2/5  Downloading binaries..."
    mkdir -p "$INSTALL_DIR"
    download_binary "$COMPANION_REPO" "$comp_tag" "flip-companion" "$INSTALL_DIR/flip-companion"
    download_binary "$GAMESCOPE_REPO" "$game_tag"  "gamescope"      "$INSTALL_DIR/gamescope"

    # Install wrapper script
    step "3/5  Installing gamescope wrapper..."
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
    step "4/5  Configuring Bazzite Game Mode..."
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
    if [ -x "$HOME/.local/bin/flip-companion" ]; then
        "$HOME/.local/bin/flip-companion" --lease-socket /tmp/gamescope-lease.sock &
    fi
}
STEAM
    fi
    info "Wrote $SESSION_DIR/steam"

    # Summary
    step "5/5  Done!"
    echo ""
    echo -e "  ${GREEN}gamescope${NC}          → $INSTALL_DIR/gamescope"
    echo -e "  ${GREEN}flip-companion${NC}     → $INSTALL_DIR/flip-companion"
    echo -e "  ${GREEN}lease wrapper${NC}      → $INSTALL_DIR/gamescope-lease-wrapper"
    echo -e "  ${GREEN}session config${NC}     → $ENV_DIR/10-gamescope-session.conf"
    echo -e "  ${GREEN}session config${NC}     → $ENV_DIR/20-gamescope-lease.conf"
    echo -e "  ${GREEN}startup hook${NC}       → $SESSION_DIR/steam"
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
