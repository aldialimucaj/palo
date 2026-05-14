use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::telemetry::ServiceTelemetry;

pub const DEFAULT_SERVICE_LOG_RETENTION: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServiceId(String);

impl ServiceId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ServiceId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ServiceId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for ServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub working_dir: Option<PathBuf>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            working_dir: None,
        }
    }

    pub fn with_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_working_dir(mut self, working_dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(working_dir.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookPhase {
    PreBuild,
    PostBuild,
    PreStart,
    PostStart,
    PreStop,
    PostStop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDefinition {
    pub name: String,
    pub phase: HookPhase,
    pub command: CommandSpec,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildDefinition {
    pub check: Option<CommandSpec>,
    pub build: Option<CommandSpec>,
    pub hooks: Vec<HookDefinition>,
}

impl BuildDefinition {
    pub fn is_empty(&self) -> bool {
        self.check.is_none() && self.build.is_none() && self.hooks.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartPolicy {
    Manual,
    Mcp,
    Never,
    OnChange,
    OnCrash {
        max_retries: Option<u32>,
        backoff: Duration,
    },
    Always {
        backoff: Duration,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyCondition {
    Started,
    Running,
    Ready,
}

impl DependencyCondition {
    pub fn is_satisfied_by(self, lifecycle: LifecycleState, health: ServiceHealth) -> bool {
        match self {
            Self::Started => matches!(
                lifecycle,
                LifecycleState::Starting | LifecycleState::Running
            ),
            Self::Running => lifecycle == LifecycleState::Running,
            Self::Ready => lifecycle == LifecycleState::Running && health == ServiceHealth::Healthy,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceDependency {
    pub service_id: ServiceId,
    pub condition: DependencyCondition,
    pub restart: bool,
    pub required: bool,
    pub wait_timeout: Duration,
}

impl ServiceDependency {
    pub fn required(service_id: impl Into<ServiceId>) -> Self {
        Self {
            service_id: service_id.into(),
            condition: DependencyCondition::Ready,
            restart: true,
            required: true,
            wait_timeout: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadinessCheck {
    pub command: CommandSpec,
    pub initial_delay: Duration,
    pub interval: Duration,
    pub timeout: Duration,
    pub retries: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectedStatusRange {
    pub start: u16,
    pub end: u16,
}

impl ExpectedStatusRange {
    pub const fn new(start: u16, end: u16) -> Self {
        Self { start, end }
    }

    pub fn contains(self, status: u16) -> bool {
        self.start <= status && status <= self.end
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpHealthProbe {
    pub url: String,
    pub method: String,
    pub expected_status: ExpectedStatusRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthCheck {
    pub http: HttpHealthProbe,
    pub initial_delay: Duration,
    pub interval: Duration,
    pub timeout: Duration,
    pub retries: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchConfiguration {
    pub enabled: bool,
    pub paths: Vec<PathBuf>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub ignore_paths: Vec<PathBuf>,
    pub ignore_regex: Vec<String>,
    pub debounce: Duration,
}

impl WatchConfiguration {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            paths: Vec::new(),
            include: Vec::new(),
            exclude: Vec::new(),
            ignore_paths: Vec::new(),
            ignore_regex: Vec::new(),
            debounce: Duration::from_millis(2500),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceDefinition {
    pub id: ServiceId,
    pub name: String,
    pub command: CommandSpec,
    pub build: BuildDefinition,
    pub readiness: Option<ReadinessCheck>,
    pub healthcheck: Option<HealthCheck>,
    pub restart: RestartPolicy,
    pub watch: WatchConfiguration,
    pub dependencies: Vec<ServiceDependency>,
    pub depends_on: Vec<ServiceId>,
    pub hooks: Vec<HookDefinition>,
    pub log_retention: usize,
}

impl ServiceDefinition {
    pub fn dependency_contracts(&self) -> Vec<ServiceDependency> {
        if self.dependencies.is_empty() {
            return self
                .depends_on
                .iter()
                .cloned()
                .map(ServiceDependency::required)
                .collect();
        }

        self.dependencies.clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceHealth {
    Unknown,
    Healthy,
    Degraded,
    Unhealthy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    Discovered,
    Validated,
    Checked,
    Built,
    Starting,
    Running,
    Stopped,
    Failed,
    Restarting,
}

impl LifecycleState {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Restarting)
    }

    pub fn can_transition_to(&self, next: Self) -> bool {
        use LifecycleState::*;

        matches!(
            (*self, next),
            (Discovered, Validated | Failed | Stopped)
                | (Validated, Checked | Built | Starting | Failed | Stopped)
                | (Checked, Built | Starting | Failed | Stopped)
                | (Built, Starting | Failed | Stopped)
                | (Starting, Running | Failed | Restarting | Stopped)
                | (Running, Restarting | Failed | Stopped)
                | (Stopped, Starting | Restarting | Failed)
                | (Failed, Restarting | Stopped)
                | (Restarting, Starting | Failed | Stopped)
        )
    }

    pub fn transition_to(&mut self, next: Self) -> bool {
        if self.can_transition_to(next) {
            *self = next;
            return true;
        }

        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRuntime {
    pub lifecycle: LifecycleState,
    pub health: ServiceHealth,
    pub pid: Option<u32>,
    pub started_at: Option<SystemTime>,
    pub restart_count: u64,
    pub last_exit_code: Option<i32>,
    pub last_error: Option<String>,
    pub telemetry: ServiceTelemetry,
}

impl Default for ServiceRuntime {
    fn default() -> Self {
        Self {
            lifecycle: LifecycleState::Discovered,
            health: ServiceHealth::Unknown,
            pid: None,
            started_at: None,
            restart_count: 0,
            last_exit_code: None,
            last_error: None,
            telemetry: ServiceTelemetry::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AppState {
    pub services: BTreeMap<ServiceId, ServiceDefinition>,
    pub runtime: BTreeMap<ServiceId, ServiceRuntime>,
}

impl AppState {
    pub fn insert_service(&mut self, service: ServiceDefinition) -> Option<ServiceDefinition> {
        let service_id = service.id.clone();
        self.runtime.entry(service_id.clone()).or_default();
        self.services.insert(service_id, service)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_service(service_id: &str) -> ServiceDefinition {
        ServiceDefinition {
            id: ServiceId::new(service_id),
            name: format!("{service_id} service"),
            command: CommandSpec::new("cargo")
                .with_args(["run", "-p", service_id])
                .with_working_dir(format!("/workspace/{service_id}")),
            build: BuildDefinition {
                check: Some(CommandSpec::new("cargo").with_args(["check", "-p", service_id])),
                build: Some(CommandSpec::new("cargo").with_args(["build", "-p", service_id])),
                hooks: vec![HookDefinition {
                    name: "post-build".to_string(),
                    phase: HookPhase::PostBuild,
                    command: CommandSpec::new("cargo").with_args(["fmt", "--check"]),
                    required: true,
                }],
            },
            readiness: None,
            healthcheck: None,
            restart: RestartPolicy::OnChange,
            watch: WatchConfiguration::disabled(),
            dependencies: vec![ServiceDependency::required("db")],
            depends_on: vec![ServiceId::new("db")],
            hooks: vec![HookDefinition {
                name: "post-start".to_string(),
                phase: HookPhase::PostStart,
                command: CommandSpec::new("echo").with_args(["ready"]),
                required: false,
            }],
            log_retention: DEFAULT_SERVICE_LOG_RETENTION,
        }
    }

    #[test]
    fn service_id_newtype_exposes_string_identity() {
        let service_id = ServiceId::new("api");

        assert_eq!(service_id.as_str(), "api");
        assert_eq!(service_id.to_string(), "api");
        assert_eq!(ServiceId::from("api"), service_id);
        assert_eq!(ServiceId::from(String::from("api")), service_id);
    }

    #[test]
    fn command_spec_builder_populates_args_and_working_directory() {
        let command = CommandSpec::new("cargo")
            .with_args(["run", "-p", "api"])
            .with_working_dir("services/api");

        assert_eq!(command.program, "cargo");
        assert_eq!(command.args, vec!["run", "-p", "api"]);
        assert!(command.env.is_empty());
        assert_eq!(command.working_dir, Some(PathBuf::from("services/api")));
    }

    #[test]
    fn build_definition_reports_when_empty() {
        let empty = BuildDefinition {
            check: None,
            build: None,
            hooks: Vec::new(),
        };
        let populated = BuildDefinition {
            check: Some(CommandSpec::new("cargo").with_args(["check"])),
            build: None,
            hooks: Vec::new(),
        };

        assert!(empty.is_empty());
        assert!(!populated.is_empty());
    }

    #[test]
    fn disabled_watch_configuration_uses_safe_defaults() {
        let watch = WatchConfiguration::disabled();

        assert!(!watch.enabled);
        assert!(watch.paths.is_empty());
        assert!(watch.include.is_empty());
        assert!(watch.exclude.is_empty());
        assert!(watch.ignore_paths.is_empty());
        assert!(watch.ignore_regex.is_empty());
        assert_eq!(watch.debounce, Duration::from_millis(2500));
    }

    #[test]
    fn service_runtime_defaults_to_discovered_unknown_state() {
        let runtime = ServiceRuntime::default();

        assert_eq!(runtime.lifecycle, LifecycleState::Discovered);
        assert_eq!(runtime.health, ServiceHealth::Unknown);
        assert_eq!(runtime.pid, None);
        assert_eq!(runtime.started_at, None);
        assert_eq!(runtime.restart_count, 0);
        assert_eq!(runtime.last_exit_code, None);
        assert_eq!(runtime.last_error, None);
        assert_eq!(runtime.telemetry, ServiceTelemetry::default());
    }

    #[test]
    fn app_state_initializes_runtime_for_inserted_services() {
        let mut app_state = AppState::default();
        let service = sample_service("api");

        let previous = app_state.insert_service(service.clone());

        assert!(previous.is_none());
        assert_eq!(app_state.services.get(&service.id), Some(&service));
        assert_eq!(
            app_state.runtime.get(&service.id),
            Some(&ServiceRuntime::default())
        );
    }

    #[test]
    fn app_state_preserves_runtime_when_replacing_service_definition() {
        let mut app_state = AppState::default();
        let initial_service = sample_service("api");
        let replacement_service = ServiceDefinition {
            name: "renamed api".to_string(),
            ..sample_service("api")
        };

        app_state.insert_service(initial_service.clone());
        let runtime = app_state.runtime.get_mut(&initial_service.id).unwrap();
        runtime.lifecycle = LifecycleState::Running;
        runtime.pid = Some(4242);

        let previous = app_state.insert_service(replacement_service.clone());

        assert_eq!(previous, Some(initial_service));
        assert_eq!(
            app_state.services.get(&replacement_service.id),
            Some(&replacement_service)
        );
        assert_eq!(
            app_state.runtime.get(&replacement_service.id),
            Some(&ServiceRuntime {
                lifecycle: LifecycleState::Running,
                health: ServiceHealth::Unknown,
                pid: Some(4242),
                started_at: None,
                restart_count: 0,
                last_exit_code: None,
                last_error: None,
                telemetry: ServiceTelemetry::default(),
            })
        );
    }

    #[test]
    fn lifecycle_state_active_detection_matches_transitional_and_running_states() {
        assert!(LifecycleState::Starting.is_active());
        assert!(LifecycleState::Running.is_active());
        assert!(LifecycleState::Restarting.is_active());
        assert!(!LifecycleState::Discovered.is_active());
        assert!(!LifecycleState::Stopped.is_active());
        assert!(!LifecycleState::Failed.is_active());
    }

    #[test]
    fn lifecycle_state_transition_rules_allow_expected_paths() {
        let allowed_transitions = [
            (LifecycleState::Discovered, LifecycleState::Validated),
            (LifecycleState::Discovered, LifecycleState::Stopped),
            (LifecycleState::Validated, LifecycleState::Checked),
            (LifecycleState::Validated, LifecycleState::Built),
            (LifecycleState::Validated, LifecycleState::Starting),
            (LifecycleState::Checked, LifecycleState::Built),
            (LifecycleState::Checked, LifecycleState::Starting),
            (LifecycleState::Built, LifecycleState::Starting),
            (LifecycleState::Starting, LifecycleState::Running),
            (LifecycleState::Starting, LifecycleState::Restarting),
            (LifecycleState::Running, LifecycleState::Restarting),
            (LifecycleState::Running, LifecycleState::Stopped),
            (LifecycleState::Stopped, LifecycleState::Starting),
            (LifecycleState::Stopped, LifecycleState::Failed),
            (LifecycleState::Failed, LifecycleState::Restarting),
            (LifecycleState::Restarting, LifecycleState::Starting),
        ];

        for (current, next) in allowed_transitions {
            assert!(
                current.can_transition_to(next),
                "expected {current:?} -> {next:?} to be allowed"
            );
        }
    }

    #[test]
    fn lifecycle_state_transition_rules_reject_invalid_paths() {
        let invalid_transitions = [
            (LifecycleState::Discovered, LifecycleState::Running),
            (LifecycleState::Validated, LifecycleState::Restarting),
            (LifecycleState::Checked, LifecycleState::Validated),
            (LifecycleState::Built, LifecycleState::Checked),
            (LifecycleState::Starting, LifecycleState::Built),
            (LifecycleState::Running, LifecycleState::Validated),
            (LifecycleState::Stopped, LifecycleState::Running),
            (LifecycleState::Failed, LifecycleState::Running),
            (LifecycleState::Restarting, LifecycleState::Running),
        ];

        for (current, next) in invalid_transitions {
            assert!(
                !current.can_transition_to(next),
                "expected {current:?} -> {next:?} to be rejected"
            );
        }
    }

    #[test]
    fn transition_to_updates_state_only_for_valid_transitions() {
        let mut lifecycle = LifecycleState::Validated;

        assert!(lifecycle.transition_to(LifecycleState::Built));
        assert_eq!(lifecycle, LifecycleState::Built);

        assert!(!lifecycle.transition_to(LifecycleState::Checked));
        assert_eq!(lifecycle, LifecycleState::Built);
    }
}
