/// Snapshot of system statistics at a point in time.
#[derive(Debug, Clone, Default)]
pub struct SystemSnapshot {
    pub cpu: CpuInfo,
    pub gpu: GpuInfo,
    pub memory: MemoryInfo,
    pub battery: BatteryInfo,
}

#[derive(Debug, Clone, Default)]
pub struct CpuInfo {
    /// Overall CPU usage as a percentage (0.0–100.0).
    pub usage_percent: f32,
    /// Current CPU temperature in °C, if available.
    pub temp_celsius: Option<f32>,
}

#[derive(Debug, Clone, Default)]
pub struct GpuInfo {
    /// GPU usage as a percentage (0.0–100.0), if available.
    pub usage_percent: Option<f32>,
    /// GPU temperature in °C, if available.
    pub temp_celsius: Option<f32>,
}

#[derive(Debug, Clone, Default)]
pub struct MemoryInfo {
    /// Used memory in bytes.
    pub used_bytes: u64,
    /// Total memory in bytes.
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct BatteryInfo {
    /// Battery charge as a percentage (0.0–100.0), if available.
    pub charge_percent: Option<f32>,
    /// Whether the battery is currently charging.
    pub charging: bool,
}
