use crate::types::window::{ShuttleDirection, WindowId, WindowInfo};

/// Manages windows via the KWin Script D-Bus interface (Plasma 6).
#[allow(async_fn_in_trait)]
pub trait WindowManager: Send + Sync {
    /// List all open windows.
    async fn list_windows(&self) -> anyhow::Result<Vec<WindowInfo>>;

    /// Move a window to the top or bottom screen.
    async fn shuttle_window(
        &self,
        window_id: &WindowId,
        direction: ShuttleDirection,
    ) -> anyhow::Result<()>;
}
