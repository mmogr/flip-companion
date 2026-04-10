use crate::platform::stats::StatsProvider;
use crate::types::stats::{BatteryInfo, CpuInfo, GpuInfo, MemoryInfo, SystemSnapshot};

/// Mock stats provider returning synthetic data that slowly changes.
pub struct MockStatsProvider {
    tick: u32,
}

impl MockStatsProvider {
    pub fn new() -> Self {
        Self { tick: 0 }
    }
}

impl StatsProvider for MockStatsProvider {
    async fn snapshot(&mut self) -> anyhow::Result<SystemSnapshot> {
        self.tick = self.tick.wrapping_add(1);

        // Generate synthetic values that oscillate so the UI has something to show.
        let phase = (self.tick as f32 * 0.1).sin() * 0.5 + 0.5; // 0.0–1.0

        Ok(SystemSnapshot {
            cpu: CpuInfo {
                usage_percent: 15.0 + phase * 50.0,
                temp_celsius: Some(45.0 + phase * 20.0),
            },
            gpu: GpuInfo {
                usage_percent: Some(10.0 + phase * 40.0),
                temp_celsius: Some(40.0 + phase * 15.0),
            },
            memory: MemoryInfo {
                used_bytes: ((4.0 + phase * 4.0) * 1024.0 * 1024.0 * 1024.0) as u64,
                total_bytes: 16 * 1024 * 1024 * 1024,
            },
            battery: BatteryInfo {
                charge_percent: Some(85.0 - phase * 30.0),
                charging: false,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshot_returns_valid_data() {
        let mut provider = MockStatsProvider::new();
        let snap = provider.snapshot().await.unwrap();

        assert!(snap.cpu.usage_percent >= 0.0 && snap.cpu.usage_percent <= 100.0);
        assert!(snap.cpu.temp_celsius.unwrap() > 0.0);
        assert!(snap.memory.total_bytes > 0);
        assert!(snap.battery.charge_percent.unwrap() > 0.0);
    }

    #[tokio::test]
    async fn snapshot_changes_over_time() {
        let mut provider = MockStatsProvider::new();
        let snap1 = provider.snapshot().await.unwrap();
        // Advance several ticks to ensure the phase shifts visibly.
        for _ in 0..10 {
            let _ = provider.snapshot().await.unwrap();
        }
        let snap2 = provider.snapshot().await.unwrap();

        // Values should differ due to oscillation.
        assert_ne!(
            snap1.cpu.usage_percent.to_bits(),
            snap2.cpu.usage_percent.to_bits()
        );
    }
}
