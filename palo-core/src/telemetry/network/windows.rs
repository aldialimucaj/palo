use std::collections::BTreeSet;

use super::NetworkUsage;
use tracing::debug;

#[derive(Debug, Default)]
pub(super) struct Collector {
    unavailable_logged: bool,
}

impl Collector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sample_process_group(&mut self, root_pid: u32, pids: &[u32]) -> Option<NetworkUsage> {
        let scope = ProcessScope::from_root_and_children(root_pid, pids);

        let usage = sample_attributable_process_network(&scope);
        if usage.is_none() {
            self.log_unavailable_once(&scope);
        }

        usage
    }

    fn log_unavailable_once(&mut self, scope: &ProcessScope) {
        if self.unavailable_logged {
            return;
        }

        self.unavailable_logged = true;
        debug!(
            root_pid = scope.root_pid,
            pids = ?scope.pids,
            "windows process-level network telemetry requires attributable per-process counters; host interface totals are intentionally ignored"
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessScope {
    root_pid: u32,
    pids: Vec<u32>,
}

impl ProcessScope {
    fn from_root_and_children(root_pid: u32, pids: &[u32]) -> Self {
        let normalized = pids
            .iter()
            .copied()
            .chain(std::iter::once(root_pid))
            .filter(|pid| *pid != 0)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();

        Self {
            root_pid,
            pids: normalized,
        }
    }
}

fn sample_attributable_process_network(_scope: &ProcessScope) -> Option<NetworkUsage> {
    // A correct Windows implementation needs an attributable per-process source
    // such as ETW TCP/IP events correlated to this service's PID set. Palo must
    // not derive service telemetry from interface-wide counters because those
    // include unrelated host traffic.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_scope_includes_root_pid() {
        let scope = ProcessScope::from_root_and_children(42, &[100, 101]);

        assert_eq!(scope.root_pid, 42);
        assert_eq!(scope.pids, vec![42, 100, 101]);
    }

    #[test]
    fn process_scope_deduplicates_and_sorts_pids() {
        let scope = ProcessScope::from_root_and_children(42, &[101, 42, 100, 101]);

        assert_eq!(scope.pids, vec![42, 100, 101]);
    }

    #[test]
    fn process_scope_filters_zero_child_pids() {
        let scope = ProcessScope::from_root_and_children(42, &[0, 100]);

        assert_eq!(scope.pids, vec![42, 100]);
    }

    #[test]
    fn process_scope_filters_zero_root_pid() {
        let scope = ProcessScope::from_root_and_children(0, &[0, 100]);

        assert_eq!(scope.root_pid, 0);
        assert_eq!(scope.pids, vec![100]);
    }
}
