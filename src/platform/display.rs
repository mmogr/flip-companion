use async_trait::async_trait;

use crate::types::display::OutputInfo;

/// Detects and enumerates display outputs via KScreen D-Bus or equivalent.
#[async_trait]
pub trait DisplayDetector: Send + Sync {
    /// List all connected display outputs.
    async fn list_outputs(&self) -> anyhow::Result<Vec<OutputInfo>>;
}
