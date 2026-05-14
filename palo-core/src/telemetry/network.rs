use tracing::debug;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NetworkUsage {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

#[derive(Debug)]
pub(super) struct PlatformNetworkTelemetry {
    collector: platform::Collector,
    unsupported_logged: bool,
}

impl PlatformNetworkTelemetry {
    pub fn new() -> Self {
        Self {
            collector: platform::Collector::new(),
            unsupported_logged: false,
        }
    }

    pub fn sample_process_group(&mut self, root_pid: u32, pids: &[u32]) -> Option<NetworkUsage> {
        let usage = self.collector.sample_process_group(root_pid, pids);
        if usage.is_none() && !self.unsupported_logged {
            self.unsupported_logged = true;
            debug!(
                root_pid,
                pids = ?pids,
                platform = std::env::consts::OS,
                "process-level network telemetry is unavailable on this host"
            );
        }
        usage
    }
}

impl Default for PlatformNetworkTelemetry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "linux")]
#[path = "network/linux.rs"]
mod platform;

#[cfg(target_os = "macos")]
#[path = "network/macos.rs"]
mod platform;

#[cfg(target_os = "windows")]
#[path = "network/windows.rs"]
mod platform;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
#[path = "network/unsupported.rs"]
mod platform;
