use async_trait::async_trait;

use crate::types::stats::SystemSnapshot;

/// Collects system statistics (CPU, GPU, RAM, battery, thermals).
#[async_trait]
pub trait StatsProvider: Send + Sync {
    /// Take a snapshot of current system stats.
    async fn snapshot(&mut self) -> anyhow::Result<SystemSnapshot>;
}
