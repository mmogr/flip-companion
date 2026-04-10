use crate::types::display::OutputInfo;

/// Detects and enumerates display outputs via KScreen D-Bus or equivalent.
#[allow(async_fn_in_trait)]
pub trait DisplayDetector: Send + Sync {
    /// List all connected display outputs.
    async fn list_outputs(&self) -> anyhow::Result<Vec<OutputInfo>>;
}
