use clap::Parser;

/// Bottom-screen companion app for AYANEO Flip DS on Bazzite.
#[derive(Parser, Debug)]
#[command(name = "flip-companion", version)]
pub struct Config {
    /// Run with mock backends (no Wayland, D-Bus, or hardware required).
    #[arg(long)]
    pub mock: bool,

    /// Override the bottom-screen output name (e.g. "eDP-2").
    /// If not set, auto-detection is used.
    #[arg(long)]
    pub output: Option<String>,

    /// Path to Gamescope's DRM lease socket (enables Game Mode).
    /// Connects and receives a DRM lease fd via SCM_RIGHTS.
    /// Default: /tmp/gamescope-lease.sock
    #[arg(long)]
    pub lease_socket: Option<String>,
}
