use clap::Parser;
use serde::Deserialize;

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

    /// Path to apps.toml configuration file.
    /// Default: looks for apps.toml next to the executable, then ~/.config/flip-companion/apps.toml
    #[arg(long)]
    pub apps_config: Option<String>,
}

/// A single launchable app entry from apps.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct AppEntry {
    pub name: String,
    pub icon: String,
    pub exec: String,
    pub url: Option<String>,
}

/// Top-level apps.toml structure.
#[derive(Debug, Deserialize)]
struct AppsFile {
    app: Vec<AppEntry>,
}

/// Load the apps list from the config file.
/// Searches in order: --apps-config path, ./apps.toml, ~/.config/flip-companion/apps.toml
pub fn load_apps(config: &Config) -> Vec<AppEntry> {
    let candidates: Vec<std::path::PathBuf> = if let Some(ref path) = config.apps_config {
        vec![std::path::PathBuf::from(path)]
    } else {
        let mut paths = vec![std::path::PathBuf::from("apps.toml")];
        if let Some(config_dir) = dirs_fallback() {
            paths.push(config_dir.join("apps.toml"));
        }
        paths
    };

    for path in &candidates {
        if let Ok(contents) = std::fs::read_to_string(path) {
            match toml::from_str::<AppsFile>(&contents) {
                Ok(file) => {
                    eprintln!("[config] loaded {} apps from {}", file.app.len(), path.display());
                    return file.app;
                }
                Err(e) => {
                    eprintln!("[config] failed to parse {}: {e}", path.display());
                }
            }
        }
    }

    eprintln!("[config] no apps.toml found, using empty app list");
    Vec::new()
}

fn dirs_fallback() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        })
        .map(|p| p.join("flip-companion"))
}
