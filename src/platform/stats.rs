use crate::types::stats::SystemSnapshot;

/// Collects system statistics (CPU, GPU, RAM, battery, thermals).
#[allow(async_fn_in_trait)]
pub trait StatsProvider: Send + Sync {
    /// Take a snapshot of current system stats.
    async fn snapshot(&mut self) -> anyhow::Result<SystemSnapshot>;
}
