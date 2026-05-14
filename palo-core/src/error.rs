use std::error::Error as StdError;
use std::fmt;
use std::path::PathBuf;

use crate::domain::ServiceId;
use crate::events::{CommandKind, OrchestrationStage};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaloError {
    Configuration(ConfigurationError),
    Discovery(DiscoveryError),
    Build(BuildError),
    Process(ProcessError),
    Watch(WatchError),
    UiCommand(UiCommandError),
}

impl PaloError {
    pub fn stage(&self) -> OrchestrationStage {
        match self {
            Self::Configuration(_) => OrchestrationStage::Validation,
            Self::Discovery(_) => OrchestrationStage::DependencyResolution,
            Self::Build(error) => error.stage.into(),
            Self::Process(error) => error.operation.into(),
            Self::Watch(_) => OrchestrationStage::Watch,
            Self::UiCommand(_) => OrchestrationStage::CommandHandling,
        }
    }

    pub fn service_id(&self) -> Option<&ServiceId> {
        match self {
            Self::Configuration(_) | Self::Discovery(_) => None,
            Self::Build(error) => Some(&error.service_id),
            Self::Process(error) => Some(&error.service_id),
            Self::Watch(error) => error.service_id.as_ref(),
            Self::UiCommand(error) => error.target_service_id.as_ref(),
        }
    }
}

impl fmt::Display for PaloError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(error) => error.fmt(f),
            Self::Discovery(error) => error.fmt(f),
            Self::Build(error) => error.fmt(f),
            Self::Process(error) => error.fmt(f),
            Self::Watch(error) => error.fmt(f),
            Self::UiCommand(error) => error.fmt(f),
        }
    }
}

impl StdError for PaloError {}

impl From<ConfigurationError> for PaloError {
    fn from(value: ConfigurationError) -> Self {
        Self::Configuration(value)
    }
}

impl From<DiscoveryError> for PaloError {
    fn from(value: DiscoveryError) -> Self {
        Self::Discovery(value)
    }
}

impl From<BuildError> for PaloError {
    fn from(value: BuildError) -> Self {
        Self::Build(value)
    }
}

impl From<ProcessError> for PaloError {
    fn from(value: ProcessError) -> Self {
        Self::Process(value)
    }
}

impl From<WatchError> for PaloError {
    fn from(value: WatchError) -> Self {
        Self::Watch(value)
    }
}

impl From<UiCommandError> for PaloError {
    fn from(value: UiCommandError) -> Self {
        Self::UiCommand(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigurationError {
    pub path: Option<String>,
    pub message: String,
}

impl ConfigurationError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            path: None,
            message: message.into(),
        }
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }
}

impl fmt::Display for ConfigurationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "configuration error{}: {}{}",
            format_path_context(self.path.as_deref()),
            self.message,
            remediation_suffix(error_remediation_message(ErrorRemediation::Configuration {
                path: self.path.as_deref(),
            }))
        )
    }
}

impl StdError for ConfigurationError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryError {
    pub project_type: Option<String>,
    pub path: Option<PathBuf>,
    pub message: String,
}

impl DiscoveryError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            project_type: None,
            path: None,
            message: message.into(),
        }
    }

    pub fn with_project_type(mut self, project_type: impl Into<String>) -> Self {
        self.project_type = Some(project_type.into());
        self
    }

    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "discovery error{}{}: {}{}",
            format_pathbuf_context(self.path.as_ref()),
            format_project_type_context(self.project_type.as_deref()),
            self.message,
            remediation_suffix(error_remediation_message(ErrorRemediation::Discovery {
                project_type: self.project_type.as_deref(),
            }))
        )
    }
}

impl StdError for DiscoveryError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildStage {
    Check,
    Build,
    Hook,
}

impl From<BuildStage> for OrchestrationStage {
    fn from(value: BuildStage) -> Self {
        match value {
            BuildStage::Check => OrchestrationStage::Check,
            BuildStage::Build | BuildStage::Hook => OrchestrationStage::Build,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildError {
    pub service_id: ServiceId,
    pub stage: BuildStage,
    pub hook_name: Option<String>,
    pub exit_code: Option<i32>,
    pub message: String,
}

impl BuildError {
    pub fn new(
        service_id: impl Into<ServiceId>,
        stage: BuildStage,
        message: impl Into<String>,
    ) -> Self {
        Self {
            service_id: service_id.into(),
            stage,
            hook_name: None,
            exit_code: None,
            message: message.into(),
        }
    }

    pub fn with_hook_name(mut self, hook_name: impl Into<String>) -> Self {
        self.hook_name = Some(hook_name.into());
        self
    }

    pub fn with_exit_code(mut self, exit_code: i32) -> Self {
        self.exit_code = Some(exit_code);
        self
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "build error for service `{}` during {}{}: {}{}",
            self.service_id,
            build_stage_name(self.stage),
            format_hook_context(self.hook_name.as_deref()),
            self.message,
            remediation_suffix(error_remediation_message(ErrorRemediation::Build {
                stage: self.stage,
            }))
        )
    }
}

impl StdError for BuildError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessOperation {
    Spawn,
    Observe,
    Stop,
    Wait,
    Readiness,
}

impl From<ProcessOperation> for OrchestrationStage {
    fn from(value: ProcessOperation) -> Self {
        match value {
            ProcessOperation::Spawn | ProcessOperation::Readiness => OrchestrationStage::Start,
            ProcessOperation::Observe | ProcessOperation::Wait => OrchestrationStage::Runtime,
            ProcessOperation::Stop => OrchestrationStage::Stop,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessError {
    pub service_id: ServiceId,
    pub operation: ProcessOperation,
    pub message: String,
}

impl ProcessError {
    pub fn new(
        service_id: impl Into<ServiceId>,
        operation: ProcessOperation,
        message: impl Into<String>,
    ) -> Self {
        Self {
            service_id: service_id.into(),
            operation,
            message: message.into(),
        }
    }
}

impl fmt::Display for ProcessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "process error for service `{}` during {}: {}{}",
            self.service_id,
            process_operation_name(self.operation),
            self.message,
            remediation_suffix(error_remediation_message(ErrorRemediation::Process {
                operation: self.operation,
            }))
        )
    }
}

impl StdError for ProcessError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchError {
    pub service_id: Option<ServiceId>,
    pub path: Option<PathBuf>,
    pub message: String,
}

impl WatchError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            service_id: None,
            path: None,
            message: message.into(),
        }
    }

    pub fn for_service(mut self, service_id: impl Into<ServiceId>) -> Self {
        self.service_id = Some(service_id.into());
        self
    }

    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }
}

impl fmt::Display for WatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "watch error{}{}: {}{}",
            format_service_context(self.service_id.as_ref()),
            format_pathbuf_context(self.path.as_ref()),
            self.message,
            remediation_suffix(error_remediation_message(ErrorRemediation::Watch))
        )
    }
}

impl StdError for WatchError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiCommandError {
    pub target_service_id: Option<ServiceId>,
    pub command: CommandKind,
    pub message: String,
}

impl UiCommandError {
    pub fn new(command: CommandKind, message: impl Into<String>) -> Self {
        Self {
            target_service_id: None,
            command,
            message: message.into(),
        }
    }

    pub fn for_service(mut self, service_id: impl Into<ServiceId>) -> Self {
        self.target_service_id = Some(service_id.into());
        self
    }
}

impl fmt::Display for UiCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ui command error for {}{}: {}",
            format_command_target(self.target_service_id.as_ref()),
            format_command_context(self.command),
            self.message
        )
    }
}

impl StdError for UiCommandError {}

enum ErrorRemediation<'a> {
    Configuration { path: Option<&'a str> },
    Discovery { project_type: Option<&'a str> },
    Build { stage: BuildStage },
    Process { operation: ProcessOperation },
    Watch,
}

fn error_remediation_message(remediation: ErrorRemediation<'_>) -> &'static str {
    match remediation {
        ErrorRemediation::Configuration { path: Some(path) } if path == "palo.yml" => {
            "check that `palo.yml` exists in the workspace root and contains valid YAML"
        }
        ErrorRemediation::Configuration { .. } => {
            "correct the referenced `palo.yml` field and try again"
        }
        ErrorRemediation::Discovery {
            project_type: Some("rust"),
        } => "verify the Cargo workspace metadata and rerun `palo init --type rust`",
        ErrorRemediation::Discovery { .. } => {
            "verify the project metadata or create `palo.yml` manually"
        }
        ErrorRemediation::Build {
            stage: BuildStage::Check,
        } => {
            "inspect the preceding check output and rerun the command manually in the service working directory"
        }
        ErrorRemediation::Build {
            stage: BuildStage::Build,
        } => {
            "inspect the preceding build output and rerun the command manually in the service working directory"
        }
        ErrorRemediation::Build {
            stage: BuildStage::Hook,
        } => "inspect the hook logs and verify the referenced command succeeds outside Palo",
        ErrorRemediation::Process {
            operation: ProcessOperation::Spawn,
        } => "verify the executable path, working directory, and required environment variables",
        ErrorRemediation::Process {
            operation: ProcessOperation::Readiness,
        } => "inspect the readiness command and confirm the service can become ready outside Palo",
        ErrorRemediation::Process { .. } => {
            "inspect the service logs and confirm the process can shut down cleanly outside Palo"
        }
        ErrorRemediation::Watch => {
            "verify the watched paths exist and that the include or exclude globs are valid"
        }
    }
}

fn remediation_suffix(remediation: &'static str) -> String {
    format!("; remediation: {remediation}")
}

fn format_path_context(path: Option<&str>) -> String {
    path.map(|value| format!(" at `{value}`"))
        .unwrap_or_default()
}

fn format_pathbuf_context(path: Option<&PathBuf>) -> String {
    path.map(|value| format!(" at `{}`", value.display()))
        .unwrap_or_default()
}

fn format_project_type_context(project_type: Option<&str>) -> String {
    project_type
        .map(|value| format!(" for `{value}`"))
        .unwrap_or_default()
}

fn format_hook_context(hook_name: Option<&str>) -> String {
    hook_name
        .map(|value| format!(" hook `{value}`"))
        .unwrap_or_default()
}

fn format_service_context(service_id: Option<&ServiceId>) -> String {
    service_id
        .map(|value| format!(" for service `{}`", value.as_str()))
        .unwrap_or_default()
}

fn format_command_target(service_id: Option<&ServiceId>) -> String {
    service_id
        .map(|value| format!("service `{}`", value.as_str()))
        .unwrap_or_else(|| "all services".to_string())
}

fn format_command_context(command: CommandKind) -> String {
    format!(" command `{}`", command_name(command))
}

fn build_stage_name(stage: BuildStage) -> &'static str {
    match stage {
        BuildStage::Check => "check",
        BuildStage::Build => "build",
        BuildStage::Hook => "hook",
    }
}

fn process_operation_name(operation: ProcessOperation) -> &'static str {
    match operation {
        ProcessOperation::Spawn => "spawn",
        ProcessOperation::Observe => "observe",
        ProcessOperation::Stop => "stop",
        ProcessOperation::Wait => "wait",
        ProcessOperation::Readiness => "readiness",
    }
}

fn command_name(command: CommandKind) -> &'static str {
    match command {
        CommandKind::Start => "start",
        CommandKind::Stop => "stop",
        CommandKind::Restart => "restart",
        CommandKind::Validate => "validate",
        CommandKind::Check => "check",
        CommandKind::Build => "build",
        CommandKind::Quit => "quit",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palo_error_reports_stage_for_each_error_category() {
        let config = PaloError::Configuration(
            ConfigurationError::new("missing services").with_path("services"),
        );
        let discovery = PaloError::Discovery(
            DiscoveryError::new("no supported project detected").with_project_type("rust"),
        );
        let build = PaloError::Build(BuildError::new(
            "api",
            BuildStage::Check,
            "cargo check failed",
        ));
        let process = PaloError::Process(ProcessError::new(
            "api",
            ProcessOperation::Spawn,
            "failed to spawn child process",
        ));
        let watch = PaloError::Watch(WatchError::new("watch backend disconnected"));
        let command = PaloError::UiCommand(UiCommandError::new(
            CommandKind::Restart,
            "command rejected while service is building",
        ));

        assert_eq!(config.stage(), OrchestrationStage::Validation);
        assert_eq!(discovery.stage(), OrchestrationStage::DependencyResolution);
        assert_eq!(build.stage(), OrchestrationStage::Check);
        assert_eq!(process.stage(), OrchestrationStage::Start);
        assert_eq!(watch.stage(), OrchestrationStage::Watch);
        assert_eq!(command.stage(), OrchestrationStage::CommandHandling);
    }

    #[test]
    fn palo_error_exposes_service_binding_when_available() {
        let build = PaloError::Build(BuildError::new("api", BuildStage::Build, "build failed"));
        let process = PaloError::Process(ProcessError::new(
            "worker",
            ProcessOperation::Observe,
            "stdout reader terminated",
        ));
        let watch = PaloError::Watch(WatchError::new("debounce overflow").for_service("api"));
        let command = PaloError::UiCommand(
            UiCommandError::new(CommandKind::Stop, "service is already stopped").for_service("api"),
        );

        assert_eq!(build.service_id(), Some(&ServiceId::new("api")));
        assert_eq!(process.service_id(), Some(&ServiceId::new("worker")));
        assert_eq!(watch.service_id(), Some(&ServiceId::new("api")));
        assert_eq!(command.service_id(), Some(&ServiceId::new("api")));
    }

    #[test]
    fn category_specific_errors_capture_context() {
        let config =
            ConfigurationError::new("missing required field").with_path("services.api.command");
        let discovery = DiscoveryError::new("binary target is ambiguous")
            .with_project_type("rust")
            .with_path("Cargo.toml");
        let build = BuildError::new("api", BuildStage::Hook, "hook command failed")
            .with_hook_name("post-build")
            .with_exit_code(101);
        let watch = WatchError::new("path is outside workspace")
            .for_service("api")
            .with_path("src/generated");
        let command = UiCommandError::new(CommandKind::Start, "service is already running")
            .for_service("api");

        assert_eq!(config.path.as_deref(), Some("services.api.command"));
        assert_eq!(discovery.project_type.as_deref(), Some("rust"));
        assert_eq!(discovery.path, Some(PathBuf::from("Cargo.toml")));
        assert_eq!(build.hook_name.as_deref(), Some("post-build"));
        assert_eq!(build.exit_code, Some(101));
        assert_eq!(watch.service_id, Some(ServiceId::new("api")));
        assert_eq!(watch.path, Some(PathBuf::from("src/generated")));
        assert_eq!(command.target_service_id, Some(ServiceId::new("api")));
    }

    #[test]
    fn error_display_messages_stay_user_readable() {
        let error = PaloError::UiCommand(
            UiCommandError::new(CommandKind::Restart, "service has not been discovered yet")
                .for_service("api"),
        );

        assert_eq!(
            error.to_string(),
            "ui command error for service `api` command `restart`: service has not been discovered yet"
        );
    }

    #[test]
    fn config_and_process_errors_include_remediation_guidance() {
        let config =
            ConfigurationError::new("missing required field").with_path("services.api.command");
        let process = ProcessError::new("api", ProcessOperation::Spawn, "failed to spawn `cargo`");

        assert!(
            config
                .to_string()
                .contains("remediation: correct the referenced `palo.yml` field and try again")
        );
        assert!(
            process
                .to_string()
                .contains("remediation: verify the executable path, working directory, and required environment variables")
        );
    }
}
