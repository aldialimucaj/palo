use std::fs;
use std::path::PathBuf;

use tracing::debug;

use super::NetworkUsage;

#[derive(Debug, Default)]
pub(super) struct Collector {
    host_namespace_logged: bool,
}

impl Collector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sample_process_group(&mut self, root_pid: u32, _pids: &[u32]) -> Option<NetworkUsage> {
        if uses_host_network_namespace(root_pid)? {
            if !self.host_namespace_logged {
                self.host_namespace_logged = true;
                debug!(
                    root_pid,
                    "process-level network telemetry is unavailable for processes in the host network namespace"
                );
            }
            return None;
        }

        let path = proc_net_dev_path(root_pid);
        let contents = fs::read_to_string(&path)
            .map_err(|error| {
                debug!(
                    root_pid,
                    path = %path.display(),
                    %error,
                    "failed to read process network namespace interface counters"
                );
            })
            .ok()?;

        parse_proc_net_dev(&contents).or_else(|| {
            debug!(
                root_pid,
                path = %path.display(),
                "failed to parse process network namespace interface counters"
            );
            None
        })
    }
}

fn uses_host_network_namespace(root_pid: u32) -> Option<bool> {
    let self_path = proc_net_namespace_path("self");
    let root_path = proc_net_namespace_path(root_pid);

    let self_namespace = fs::read_link(&self_path)
        .map_err(|error| {
            debug!(
                root_pid,
                path = %self_path.display(),
                %error,
                "failed to read current process network namespace"
            );
        })
        .ok()?;

    let root_namespace = fs::read_link(&root_path)
        .map_err(|error| {
            debug!(
                root_pid,
                path = %root_path.display(),
                %error,
                "failed to read service root process network namespace"
            );
        })
        .ok()?;

    Some(self_namespace == root_namespace)
}

fn proc_net_namespace_path(pid: impl std::fmt::Display) -> PathBuf {
    PathBuf::from(format!("/proc/{pid}/ns/net"))
}

fn proc_net_dev_path(pid: u32) -> PathBuf {
    PathBuf::from(format!("/proc/{pid}/net/dev"))
}

fn parse_proc_net_dev(contents: &str) -> Option<NetworkUsage> {
    let mut usage = NetworkUsage {
        rx_bytes: 0,
        tx_bytes: 0,
    };

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let Some((interface, counters)) = line.split_once(':') else {
            continue;
        };
        if interface.trim() == "lo" {
            continue;
        }

        let mut fields = counters.split_whitespace();
        let rx_bytes = fields.next()?.parse::<u64>().ok()?;
        let tx_bytes = fields.nth(7)?.parse::<u64>().ok()?;

        usage.rx_bytes = usage.rx_bytes.saturating_add(rx_bytes);
        usage.tx_bytes = usage.tx_bytes.saturating_add(tx_bytes);
    }

    Some(usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_network_device_counters() {
        let contents = r#"
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
  lo: 1000 10 0 0 0 0 0 0 2000 20 0 0 0 0 0 0
eth0: 12345 100 0 0 0 0 0 0 67890 200 0 0 0 0 0 0
wlan0: 50 1 0 0 0 0 0 0 70 2 0 0 0 0 0 0
"#;

        assert_eq!(
            parse_proc_net_dev(contents),
            Some(NetworkUsage {
                rx_bytes: 12395,
                tx_bytes: 67960
            })
        );
    }

    #[test]
    fn returns_zero_when_only_loopback_is_present() {
        let contents = r#"
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
lo: 1000 10 0 0 0 0 0 0 2000 20 0 0 0 0 0 0
"#;

        assert_eq!(
            parse_proc_net_dev(contents),
            Some(NetworkUsage {
                rx_bytes: 0,
                tx_bytes: 0
            })
        );
    }

    #[test]
    fn rejects_malformed_network_device_counters() {
        let contents = r#"
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
eth0: not-a-number 100 0 0 0 0 0 0 67890 200 0 0 0 0 0 0
"#;

        assert_eq!(parse_proc_net_dev(contents), None);
    }
}
