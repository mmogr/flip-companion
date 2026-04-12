use std::collections::VecDeque;

const HISTORY_LEN: usize = 30;

/// Rolling history of stats values with SVG sparkline path generation.
pub struct StatsHistory {
    cpu: VecDeque<f32>,
    gpu: VecDeque<f32>,
    mem: VecDeque<f32>,
    tick_count: usize,
}

impl StatsHistory {
    pub fn new() -> Self {
        Self {
            cpu: VecDeque::with_capacity(HISTORY_LEN),
            gpu: VecDeque::with_capacity(HISTORY_LEN),
            mem: VecDeque::with_capacity(HISTORY_LEN),
            tick_count: 0,
        }
    }

    /// Push new percentage values (0–100) into the ring buffers.
    pub fn push(&mut self, cpu_pct: f32, gpu_pct: f32, mem_pct: f32) {
        push_ring(&mut self.cpu, cpu_pct);
        push_ring(&mut self.gpu, gpu_pct);
        push_ring(&mut self.mem, mem_pct);
        self.tick_count += 1;
    }

    pub fn tick_count(&self) -> usize {
        self.tick_count
    }

    pub fn cpu_history(&self) -> Vec<f32> {
        self.cpu.iter().copied().collect()
    }

    pub fn gpu_history(&self) -> Vec<f32> {
        self.gpu.iter().copied().collect()
    }

    pub fn mem_history(&self) -> Vec<f32> {
        self.mem.iter().copied().collect()
    }
}

fn push_ring(buf: &mut VecDeque<f32>, val: f32) {
    if buf.len() >= HISTORY_LEN {
        buf.pop_front();
    }
    buf.push_back(val);
}

/// Get the current local time as "HH:MM".
pub fn clock_text() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_history_gives_empty_vec() {
        let h = StatsHistory::new();
        assert!(h.cpu_history().is_empty());
    }

    #[test]
    fn single_point() {
        let mut h = StatsHistory::new();
        h.push(50.0, 0.0, 0.0);
        assert_eq!(h.cpu_history(), vec![50.0]);
    }

    #[test]
    fn ring_buffer_capped_at_30() {
        let mut h = StatsHistory::new();
        for i in 0..50 {
            h.push(i as f32, 0.0, 0.0);
        }
        let hist = h.cpu_history();
        assert_eq!(hist.len(), 30);
        assert_eq!(hist[0] as u32, 20);
    }

    #[test]
    fn clock_text_format() {
        let t = clock_text();
        assert_eq!(t.len(), 5);
        assert_eq!(t.as_bytes()[2], b':');
    }
}
