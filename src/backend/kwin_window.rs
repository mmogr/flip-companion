use async_trait::async_trait;
use serde::Deserialize;

use crate::platform::window::WindowManager;
use crate::types::display::OutputId;
use crate::types::window::{ShuttleDirection, WindowId, WindowInfo};

// ---------------------------------------------------------------------------
// D-Bus proxy for the KWin script interface
// ---------------------------------------------------------------------------

#[zbus::proxy(
    interface = "org.kde.KWin.Script.flipCompanionShuttle",
    default_service = "org.kde.KWin",
    default_path = "/FlipCompanion"
)]
trait FlipCompanionShuttle {
    #[zbus(name = "listWindows")]
    fn list_windows(&self) -> zbus::Result<String>;

    #[zbus(name = "moveWindowToOutput")]
    fn move_window_to_output(&self, window_id: &str, output_name: &str) -> zbus::Result<String>;

    #[zbus(name = "listOutputs")]
    fn list_outputs(&self) -> zbus::Result<String>;
}

// ---------------------------------------------------------------------------
// Serde models for JSON returned by the KWin script
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct KWinWindow {
    id: String,
    caption: String,
    output: String,
}

#[derive(Debug, Deserialize)]
struct KWinOutput {
    name: String,
    #[allow(dead_code)]
    x: i32,
    y: i32,
    #[allow(dead_code)]
    width: u32,
    #[allow(dead_code)]
    height: u32,
}

// ---------------------------------------------------------------------------
// Public implementation
// ---------------------------------------------------------------------------

pub struct KWinWindowManager {
    proxy: FlipCompanionShuttleProxy<'static>,
}

impl KWinWindowManager {
    pub async fn try_new() -> anyhow::Result<Self> {
        let conn = zbus::Connection::session().await?;
        let proxy = FlipCompanionShuttleProxy::new(&conn).await?;

        // Verify the interface is reachable.
        proxy.list_outputs().await.map_err(|e| {
            anyhow::anyhow!(
                "KWin D-Bus interface not reachable: {e}. \
                 Ensure the flip-companion KWin script is installed via \
                 kpackagetool6 and enabled in KWin."
            )
        })?;

        Ok(Self { proxy })
    }
}

#[async_trait]
impl WindowManager for KWinWindowManager {
    async fn list_windows(&self) -> anyhow::Result<Vec<WindowInfo>> {
        let json = self.proxy.list_windows().await?;
        Ok(parse_windows_json(&json)?)
    }

    async fn shuttle_window(
        &self,
        window_id: &WindowId,
        direction: ShuttleDirection,
    ) -> anyhow::Result<()> {
        let outputs_json = self.proxy.list_outputs().await?;
        let target_name = pick_target_output(&outputs_json, direction)?;

        let result = self
            .proxy
            .move_window_to_output(&window_id.0, &target_name)
            .await?;

        if result != "ok" {
            anyhow::bail!("KWin moveWindowToOutput failed: {result}");
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers (testable without D-Bus)
// ---------------------------------------------------------------------------

fn parse_windows_json(json: &str) -> anyhow::Result<Vec<WindowInfo>> {
    let kwin_windows: Vec<KWinWindow> = serde_json::from_str(json)?;
    Ok(kwin_windows
        .into_iter()
        .map(|w| WindowInfo {
            id: WindowId(w.id),
            caption: w.caption,
            output: if w.output.is_empty() {
                None
            } else {
                Some(OutputId(w.output))
            },
        })
        .collect())
}

fn pick_target_output(outputs_json: &str, direction: ShuttleDirection) -> anyhow::Result<String> {
    let mut outputs: Vec<KWinOutput> = serde_json::from_str(outputs_json)?;

    if outputs.len() < 2 {
        anyhow::bail!(
            "need at least 2 outputs for shuttle, found {}",
            outputs.len()
        );
    }

    // Sort by Y: lowest Y = top screen (Up), highest Y = bottom screen (Down).
    outputs.sort_by_key(|o| o.y);

    let target = match direction {
        ShuttleDirection::Up => &outputs[0],
        ShuttleDirection::Down => outputs.last().unwrap(),
    };

    Ok(target.name.clone())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_windows_basic() {
        let json = r#"[
            {"id": "abc-123", "caption": "Firefox", "output": "HDMI-1"},
            {"id": "def-456", "caption": "Terminal", "output": ""}
        ]"#;

        let windows = parse_windows_json(json).unwrap();
        assert_eq!(windows.len(), 2);

        assert_eq!(windows[0].id, WindowId("abc-123".into()));
        assert_eq!(windows[0].caption, "Firefox");
        assert_eq!(windows[0].output, Some(OutputId("HDMI-1".into())));

        assert_eq!(windows[1].id, WindowId("def-456".into()));
        assert_eq!(windows[1].caption, "Terminal");
        assert_eq!(windows[1].output, None);
    }

    #[test]
    fn parse_windows_empty() {
        let windows = parse_windows_json("[]").unwrap();
        assert!(windows.is_empty());
    }

    #[test]
    fn parse_windows_invalid_json() {
        assert!(parse_windows_json("not json").is_err());
    }

    #[test]
    fn pick_target_output_up() {
        let json = r#"[
            {"name": "eDP-1", "x": 0, "y": 0, "width": 1920, "height": 1080},
            {"name": "eDP-2", "x": 0, "y": 1080, "width": 1920, "height": 1080}
        ]"#;

        let name = pick_target_output(json, ShuttleDirection::Up).unwrap();
        assert_eq!(name, "eDP-1");
    }

    #[test]
    fn pick_target_output_down() {
        let json = r#"[
            {"name": "eDP-1", "x": 0, "y": 0, "width": 1920, "height": 1080},
            {"name": "eDP-2", "x": 0, "y": 1080, "width": 1920, "height": 1080}
        ]"#;

        let name = pick_target_output(json, ShuttleDirection::Down).unwrap();
        assert_eq!(name, "eDP-2");
    }

    #[test]
    fn pick_target_output_unsorted_inputs() {
        // Outputs arrive in reverse order — sort should fix it.
        let json = r#"[
            {"name": "Bottom", "x": 0, "y": 1080, "width": 1920, "height": 1080},
            {"name": "Top", "x": 0, "y": 0, "width": 1920, "height": 1080}
        ]"#;

        assert_eq!(
            pick_target_output(json, ShuttleDirection::Up).unwrap(),
            "Top"
        );
        assert_eq!(
            pick_target_output(json, ShuttleDirection::Down).unwrap(),
            "Bottom"
        );
    }

    #[test]
    fn pick_target_output_single_display_fails() {
        let json = r#"[{"name": "eDP-1", "x": 0, "y": 0, "width": 1920, "height": 1080}]"#;
        assert!(pick_target_output(json, ShuttleDirection::Up).is_err());
    }
}
