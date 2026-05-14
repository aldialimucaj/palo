use super::NetworkUsage;

#[derive(Debug, Default)]
pub(super) struct Collector;

impl Collector {
    pub fn new() -> Self {
        Self
    }

    pub fn sample_process_group(&mut self, _root_pid: u32, _pids: &[u32]) -> Option<NetworkUsage> {
        None
    }
}
