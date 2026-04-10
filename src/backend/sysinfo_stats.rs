use async_trait::async_trait;
use sysinfo::{Components, System};

use crate::platform::stats::StatsProvider;
use crate::types::stats::{BatteryInfo, CpuInfo, GpuInfo, MemoryInfo, SystemSnapshot};

/// Real system stats provider using sysinfo + async sysfs reads.
pub struct SysinfoStatsProvider {
    system: System,
    components: Components,
    gpu_busy_path: Option<String>,
    gpu_temp_path: Option<String>,
    bat_capacity_path: Option<String>,
    bat_status_path: Option<String>,
}

impl Default for SysinfoStatsProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl SysinfoStatsProvider {
    pub fn new() -> Self {
        let mut system = System::new();
        // First refresh to seed CPU usage — first reading will be inaccurate
        // but subsequent 1s-interval polls will be fine.
        system.refresh_cpu_usage();
        system.refresh_memory();

        let components = Components::new_with_refreshed_list();

        let gpu_busy_path = find_sysfs_glob("/sys/class/drm/card*/device/gpu_busy_percent");
        let gpu_temp_path = find_sysfs_glob("/sys/class/drm/card*/device/hwmon/hwmon*/temp1_input");
        let bat_capacity_path = find_sysfs_glob("/sys/class/power_supply/BAT*/capacity");
        let bat_status_path = find_sysfs_glob("/sys/class/power_supply/BAT*/status");

        Self {
            system,
            components,
            gpu_busy_path,
            gpu_temp_path,
            bat_capacity_path,
            bat_status_path,
        }
    }
}

#[async_trait]
impl StatsProvider for SysinfoStatsProvider {
    async fn snapshot(&mut self) -> anyhow::Result<SystemSnapshot> {
        // --- CPU (sysinfo — reads /proc, which is memory-mapped) ---
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.components.refresh();

        let cpu_usage = self.system.global_cpu_usage();
        let cpu_temp = self
            .components
            .iter()
            .find(|c| {
                let label = c.label();
                label.contains("Tctl") || label.contains("k10temp")
            })
            .map(|c| c.temperature());

        // --- Memory ---
        let used_bytes = self.system.used_memory();
        let total_bytes = self.system.total_memory();

        // --- GPU (async sysfs reads) ---
        let gpu_usage = match &self.gpu_busy_path {
            Some(path) => read_sysfs_f32(path).await,
            None => None,
        };
        let gpu_temp = match &self.gpu_temp_path {
            Some(path) => read_sysfs_f32(path).await.map(|v| v / 1000.0),
            None => None,
        };

        // --- Battery (async sysfs reads) ---
        let charge_percent = match &self.bat_capacity_path {
            Some(path) => read_sysfs_f32(path).await,
            None => None,
        };
        let charging = match &self.bat_status_path {
            Some(path) => read_sysfs_string(path)
                .await
                .map(|s| s == "Charging")
                .unwrap_or(false),
            None => false,
        };

        Ok(SystemSnapshot {
            cpu: CpuInfo {
                usage_percent: cpu_usage,
                temp_celsius: cpu_temp,
            },
            gpu: GpuInfo {
                usage_percent: gpu_usage,
                temp_celsius: gpu_temp,
            },
            memory: MemoryInfo {
                used_bytes,
                total_bytes,
            },
            battery: BatteryInfo {
                charge_percent,
                charging,
            },
        })
    }
}

/// Read a sysfs file and parse as f32, returning None on any failure.
async fn read_sysfs_f32(path: &str) -> Option<f32> {
    tokio::fs::read_to_string(path)
        .await
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Read a sysfs file and return the trimmed string, returning None on failure.
async fn read_sysfs_string(path: &str) -> Option<String> {
    tokio::fs::read_to_string(path)
        .await
        .ok()
        .map(|s| s.trim().to_string())
}

/// Find the first path matching a glob pattern, or None.
fn find_sysfs_glob(pattern: &str) -> Option<String> {
    glob::glob(pattern)
        .ok()?
        .filter_map(|r| r.ok())
        .next()
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_sysfs_glob_returns_none_for_nonexistent() {
        assert!(find_sysfs_glob("/sys/class/drm/card*/device/does_not_exist_xyz").is_none());
    }

    #[tokio::test]
    async fn read_sysfs_f32_returns_none_for_missing_file() {
        assert!(read_sysfs_f32("/nonexistent/path").await.is_none());
    }

    #[tokio::test]
    async fn read_sysfs_string_returns_none_for_missing_file() {
        assert!(read_sysfs_string("/nonexistent/path").await.is_none());
    }

    #[tokio::test]
    async fn snapshot_returns_valid_data() {
        let mut provider = SysinfoStatsProvider::new();
        let snap = provider.snapshot().await.expect("snapshot failed");
        // CPU usage should be in valid range (may be 0.0 on first call)
        assert!(snap.cpu.usage_percent >= 0.0 && snap.cpu.usage_percent <= 100.0);
        // Memory total should be non-zero
        assert!(snap.memory.total_bytes > 0);
    }
}
