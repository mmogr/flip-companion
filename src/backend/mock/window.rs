use crate::platform::window::WindowManager;
use crate::types::display::OutputId;
use crate::types::window::{ShuttleDirection, WindowId, WindowInfo};

/// Mock window manager with a fixed set of fake windows.
pub struct MockWindowManager {
    windows: Vec<WindowInfo>,
}

impl MockWindowManager {
    pub fn new() -> Self {
        Self {
            windows: vec![
                WindowInfo {
                    id: WindowId("1".into()),
                    caption: "Firefox".into(),
                    output: Some(OutputId("MOCK-TOP".into())),
                },
                WindowInfo {
                    id: WindowId("2".into()),
                    caption: "Terminal".into(),
                    output: Some(OutputId("MOCK-TOP".into())),
                },
                WindowInfo {
                    id: WindowId("3".into()),
                    caption: "File Manager".into(),
                    output: Some(OutputId("MOCK-BOTTOM".into())),
                },
            ],
        }
    }
}

impl WindowManager for MockWindowManager {
    async fn list_windows(&self) -> anyhow::Result<Vec<WindowInfo>> {
        Ok(self.windows.clone())
    }

    async fn shuttle_window(
        &self,
        window_id: &WindowId,
        direction: ShuttleDirection,
    ) -> anyhow::Result<()> {
        let target = match direction {
            ShuttleDirection::Up => "MOCK-TOP",
            ShuttleDirection::Down => "MOCK-BOTTOM",
        };
        println!(
            "[mock-window] shuttle {:?} → {target}",
            window_id.0
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lists_three_windows() {
        let wm = MockWindowManager::new();
        let windows = wm.list_windows().await.unwrap();
        assert_eq!(windows.len(), 3);
    }

    #[tokio::test]
    async fn shuttle_succeeds() {
        let wm = MockWindowManager::new();
        wm.shuttle_window(&WindowId("1".into()), ShuttleDirection::Down)
            .await
            .unwrap();
    }
}
