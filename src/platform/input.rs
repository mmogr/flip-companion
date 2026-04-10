use async_trait::async_trait;

/// Injects keyboard input into the focused window (via uinput/evdev).
#[async_trait]
pub trait InputInjector: Send + Sync {
    /// Type a string into the currently focused window.
    async fn type_text(&self, text: &str) -> anyhow::Result<()>;

    /// Press and release a single key by name (e.g. "Return", "BackSpace").
    async fn press_key(&self, key: &str) -> anyhow::Result<()>;
}
