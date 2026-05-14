#[cfg(unix)]
use std::collections::BTreeSet;
use std::collections::VecDeque;
#[cfg(unix)]
use std::process::Command;
use std::time::{Duration, SystemTime};

use sysinfo::{Pid, ProcessesToUpdate, System};
use tracing::debug;
#[cfg(unix)]
use tracing::warn;

mod network;

use network::PlatformNetworkTelemetry;

const DEFAULT_SNAPSHOT_LIMIT: usize = 32;
const DEFAULT_EXIT_HISTORY_LIMIT: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceTelemetry {
    pub latest: Option<TelemetrySnapshot>,
    pub recent: VecDeque<TelemetrySnapshot>,
    pub exit_history: VecDeque<ExitRecord>,
    pub snapshot_limit: usize,
    pub exit_history_limit: usize,
}

impl Default for ServiceTelemetry {
    fn default() -> Self {
        Self {
            latest: None,
            recent: VecDeque::new(),
            exit_history: VecDeque::new(),
            snapshot_limit: DEFAULT_SNAPSHOT_LIMIT,
            exit_history_limit: DEFAULT_EXIT_HISTORY_LIMIT,
        }
    }
}

impl ServiceTelemetry {
    pub fn record_snapshot(&mut self, snapshot: TelemetrySnapshot) {
        self.latest = Some(snapshot.clone());
        self.recent.push_back(snapshot);
        trim_queue(&mut self.recent, self.snapshot_limit);
    }

    pub fn record_exit(&mut self, exit: ExitRecord) {
        self.exit_history.push_back(exit);
        trim_queue(&mut self.exit_history, self.exit_history_limit);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitRecord {
    pub recorded_at: SystemTime,
    pub exit_code: Option<i32>,
    pub success: bool,
}

impl ExitRecord {
    pub fn new(exit_code: Option<i32>, success: bool) -> Self {
        Self {
            recorded_at: SystemTime::now(),
            exit_code,
            success,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetrySnapshot {
    pub collected_at: SystemTime,
    pub pid: u32,
    pub cpu_millis: u64,
    pub memory_bytes: u64,
    pub uptime: Option<Duration>,
    pub open_ports: Vec<u16>,
    pub disk_read_bytes: Option<u64>,
    pub disk_written_bytes: Option<u64>,
    pub network_rx_bytes: Option<u64>,
    pub network_tx_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessTelemetrySample {
    pub pid: u32,
    pub cpu_millis: u64,
    pub memory_bytes: u64,
    pub uptime: Option<Duration>,
    pub open_ports: Vec<u16>,
    pub disk_read_bytes: Option<u64>,
    pub disk_written_bytes: Option<u64>,
    pub network_rx_bytes: Option<u64>,
    pub network_tx_bytes: Option<u64>,
}

impl TelemetrySnapshot {
    pub fn from_sample(sample: ProcessTelemetrySample) -> Self {
        Self {
            collected_at: SystemTime::now(),
            pid: sample.pid,
            cpu_millis: sample.cpu_millis,
            memory_bytes: sample.memory_bytes,
            uptime: sample.uptime,
            open_ports: sample.open_ports,
            disk_read_bytes: sample.disk_read_bytes,
            disk_written_bytes: sample.disk_written_bytes,
            network_rx_bytes: sample.network_rx_bytes,
            network_tx_bytes: sample.network_tx_bytes,
        }
    }
}

#[derive(Debug)]
pub struct TelemetrySampler {
    system: System,
    network: PlatformNetworkTelemetry,
}

impl Default for TelemetrySampler {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetrySampler {
    pub fn new() -> Self {
        let mut system = System::new();
        system.refresh_cpu_usage();
        Self {
            system,
            network: PlatformNetworkTelemetry::new(),
        }
    }

    pub fn sample_process(&mut self, pid: u32) -> Option<TelemetrySnapshot> {
        let pid = Pid::from_u32(pid);
        let _ = self
            .system
            .refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        let process = self.system.process(pid)?;

        let disk = process.disk_usage();
        let process_group_pids = collect_process_group_pids(pid.as_u32());
        let network = self
            .network
            .sample_process_group(pid.as_u32(), &process_group_pids);
        Some(TelemetrySnapshot::from_sample(ProcessTelemetrySample {
            pid: pid.as_u32(),
            cpu_millis: normalize_cpu_usage(process.cpu_usage()),
            memory_bytes: process.memory(),
            uptime: Some(Duration::from_secs(process.run_time())),
            open_ports: collect_open_ports(pid.as_u32(), &process_group_pids),
            disk_read_bytes: Some(disk.total_read_bytes),
            disk_written_bytes: Some(disk.total_written_bytes),
            network_rx_bytes: network.map(|usage| usage.rx_bytes),
            network_tx_bytes: network.map(|usage| usage.tx_bytes),
        }))
    }
}

pub fn normalize_cpu_usage(cpu_percent: f32) -> u64 {
    if !cpu_percent.is_finite() || cpu_percent <= 0.0 {
        return 0;
    }

    (cpu_percent * 1000.0).round() as u64
}

fn trim_queue<T>(queue: &mut VecDeque<T>, limit: usize) {
    while queue.len() > limit {
        queue.pop_front();
    }
}

fn collect_process_group_pids(pid: u32) -> Vec<u32> {
    #[cfg(unix)]
    {
        process_group_pids(pid)
    }

    #[cfg(not(unix))]
    {
        vec![pid]
    }
}

fn collect_open_ports(pid: u32, pids: &[u32]) -> Vec<u16> {
    #[cfg(unix)]
    {
        let mut ports = collect_lsof_open_ports(pids);

        if ports.is_empty() {
            ports = collect_ss_open_ports(pids);
        }

        if !ports.is_empty() {
            debug!(pid, pids = ?pids, ports = ?ports, "sampled listening ports for service process group");
        }

        ports
    }

    #[cfg(not(unix))]
    {
        debug!(
            pid,
            "open port telemetry is not supported on this platform yet"
        );
        Vec::new()
    }
}

#[cfg(unix)]
fn process_group_pids(pid: u32) -> Vec<u32> {
    let output = Command::new("ps").args(["-Ao", "pid=,pgid="]).output();
    let Ok(output) = output else {
        warn!(
            pid,
            "failed to run ps while collecting service process group"
        );
        return vec![pid];
    };

    if !output.status.success() {
        debug!(
            pid,
            status = ?output.status.code(),
            "ps did not return process group data"
        );
        return vec![pid];
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut pids = parse_ps_process_group_pids(&stdout, pid);
    if pids.is_empty() {
        pids.push(pid);
    }
    pids
}

#[cfg(unix)]
fn collect_lsof_open_ports(pids: &[u32]) -> Vec<u16> {
    if pids.is_empty() {
        return Vec::new();
    }

    let pid_list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let output = Command::new("lsof")
        .args(["-nP", "-a", "-iTCP", "-sTCP:LISTEN", "-Fn", "-p", &pid_list])
        .output();

    let Ok(output) = output else {
        debug!("lsof is unavailable while collecting service listening ports");
        return Vec::new();
    };

    if !output.status.success() {
        debug!(
            status = ?output.status.code(),
            "lsof did not return service listening port data"
        );
        return Vec::new();
    }

    parse_lsof_ports(&String::from_utf8_lossy(&output.stdout), pids)
}

#[cfg(unix)]
fn collect_ss_open_ports(pids: &[u32]) -> Vec<u16> {
    if pids.is_empty() {
        return Vec::new();
    }

    let output = Command::new("ss").args(["-H", "-ltnp"]).output();
    let Ok(output) = output else {
        debug!("ss is unavailable while collecting service listening ports");
        return Vec::new();
    };

    if !output.status.success() {
        debug!(
            status = ?output.status.code(),
            "ss did not return service listening port data"
        );
        return Vec::new();
    }

    parse_ss_ports(&String::from_utf8_lossy(&output.stdout), pids)
}

#[cfg(unix)]
fn parse_ps_process_group_pids(output: &str, process_group_id: u32) -> Vec<u32> {
    let mut pids = output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let pgid = fields.next()?.parse::<u32>().ok()?;
            (pgid == process_group_id).then_some(pid)
        })
        .collect::<Vec<_>>();

    pids.sort_unstable();
    pids.dedup();
    pids
}

#[cfg(unix)]
fn parse_lsof_ports(output: &str, pids: &[u32]) -> Vec<u16> {
    let mut ports = BTreeSet::new();
    let mut current_pid = None;

    for line in output.lines() {
        if let Some(pid) = line.strip_prefix('p') {
            current_pid = pid.parse::<u32>().ok();
            continue;
        }

        if !current_pid.is_some_and(|pid| pids.contains(&pid)) {
            continue;
        }

        let Some(name) = line.strip_prefix('n') else {
            continue;
        };
        if let Some(port) = parse_port_from_socket_name(name) {
            ports.insert(port);
        }
    }

    ports.into_iter().collect()
}

#[cfg(unix)]
fn parse_ss_ports(output: &str, pids: &[u32]) -> Vec<u16> {
    let mut ports = BTreeSet::new();

    for line in output.lines() {
        if !line_contains_any_pid(line, pids) {
            continue;
        }

        if let Some(local_address) = line.split_whitespace().nth(3) {
            if let Some(port) = parse_port_from_socket_name(local_address) {
                ports.insert(port);
            }
        }
    }

    ports.into_iter().collect()
}

#[cfg(unix)]
fn line_contains_any_pid(line: &str, pids: &[u32]) -> bool {
    pids.iter().any(|pid| line.contains(&format!("pid={pid},")))
}

#[cfg(unix)]
fn parse_port_from_socket_name(socket_name: &str) -> Option<u16> {
    let candidate = socket_name.rsplit(':').next()?.trim_end_matches(')');
    candidate.parse::<u16>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_cpu_usage_clamps_invalid_values() {
        assert_eq!(normalize_cpu_usage(-1.0), 0);
        assert_eq!(normalize_cpu_usage(f32::NAN), 0);
        assert_eq!(normalize_cpu_usage(12.345), 12345);
    }

    #[test]
    fn service_telemetry_retains_only_recent_entries() {
        let mut telemetry = ServiceTelemetry {
            snapshot_limit: 2,
            exit_history_limit: 2,
            ..ServiceTelemetry::default()
        };

        for pid in [1, 2, 3] {
            telemetry.record_snapshot(TelemetrySnapshot::from_sample(ProcessTelemetrySample {
                pid,
                cpu_millis: 100,
                memory_bytes: 200,
                uptime: Some(Duration::from_secs(1)),
                open_ports: Vec::new(),
                disk_read_bytes: None,
                disk_written_bytes: None,
                network_rx_bytes: None,
                network_tx_bytes: None,
            }));
        }

        telemetry.record_exit(ExitRecord::new(Some(1), false));
        telemetry.record_exit(ExitRecord::new(Some(2), false));
        telemetry.record_exit(ExitRecord::new(Some(0), true));

        assert_eq!(
            telemetry.latest.as_ref().map(|snapshot| snapshot.pid),
            Some(3)
        );
        assert_eq!(telemetry.recent.len(), 2);
        assert_eq!(
            telemetry.recent.front().map(|snapshot| snapshot.pid),
            Some(2)
        );
        assert_eq!(telemetry.exit_history.len(), 2);
        assert_eq!(
            telemetry
                .exit_history
                .front()
                .map(|record| (record.exit_code, record.success)),
            Some((Some(2), false))
        );
    }

    #[cfg(unix)]
    #[test]
    fn parses_process_group_pids_from_ps_output() {
        let output = r#"
            101     100
            102     101
            103     101
            104     104
        "#;

        assert_eq!(parse_ps_process_group_pids(output, 101), vec![102, 103]);
    }

    #[cfg(unix)]
    #[test]
    fn parses_lsof_listening_ports() {
        let output = r#"
p123
n*:8080
n127.0.0.1:8090
n[::1]:9000
        "#;

        assert_eq!(parse_lsof_ports(output, &[123]), vec![8080, 8090, 9000]);
    }

    #[cfg(unix)]
    #[test]
    fn ignores_lsof_ports_from_unrelated_processes() {
        let output = r#"
p123
n*:8080
p999
n*:9000
        "#;

        assert_eq!(parse_lsof_ports(output, &[123]), vec![8080]);
    }

    #[cfg(unix)]
    #[test]
    fn parses_ss_listening_ports_for_matching_pids() {
        let output = r#"
LISTEN 0 4096 127.0.0.1:8080 0.0.0.0:* users:(("api",pid=123,fd=18))
LISTEN 0 4096 [::1]:8090 [::]:* users:(("api",pid=124,fd=19))
LISTEN 0 4096 127.0.0.1:9000 0.0.0.0:* users:(("other",pid=999,fd=20))
        "#;

        assert_eq!(parse_ss_ports(output, &[123, 124]), vec![8080, 8090]);
    }
}
