use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use config::{Config, File, FileFormat};
use serde::{Deserialize, Deserializer};
use tracing::{debug, info, warn};

use crate::domain::{
    AppState, BuildDefinition, CommandSpec, DEFAULT_SERVICE_LOG_RETENTION, DependencyCondition,
    ExpectedStatusRange, HealthCheck, HookDefinition, HookPhase, HttpHealthProbe, ReadinessCheck,
    RestartPolicy, ServiceDefinition, ServiceDependency, ServiceId, WatchConfiguration,
};
use crate::error::ConfigurationError;

pub const DEFAULT_CONFIG_FILE_NAME: &str = "palo.yml";
const DEFAULT_WATCH_DEBOUNCE: Duration = Duration::from_millis(2500);
const DEFAULT_RESTART_BACKOFF: Duration = Duration::from_secs(1);
const DEFAULT_MAX_CRASH_RETRIES: u32 = 3;
const DEFAULT_READINESS_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_READINESS_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_READINESS_RETRIES: u32 = 30;
const DEFAULT_HTTP_HEALTH_EXPECTED_STATUS: ExpectedStatusRange = ExpectedStatusRange::new(200, 399);
const DEFAULT_HTTP_HEALTH_METHOD: &str = "GET";
const DEFAULT_DEPENDENCY_WAIT_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_MCP_HOST: &str = "127.0.0.1";
const DEFAULT_MCP_PORT: u16 = 9464;
const DEFAULT_MCP_PATH: &str = "/mcp";
const DEFAULT_MCP_LOG_RETENTION: usize = 512;
const DEFAULT_LOG_DIRECTORY: &str = ".palo/logs";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaloConfig {
    pub workspace_root: PathBuf,
    pub settings: PaloSettings,
    pub services: BTreeMap<ServiceId, ConfiguredService>,
}

impl PaloConfig {
    pub fn from_workspace(workspace_root: impl Into<PathBuf>) -> Result<Self, ConfigLoadError> {
        let workspace_root = workspace_root.into();
        let config_path = workspace_root.join(DEFAULT_CONFIG_FILE_NAME);
        Self::from_path(config_path)
    }

    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, ConfigLoadError> {
        let path = path.into();
        info!(path = %path.display(), "loading palo configuration");

        let raw = load_raw_config(&path)?;
        let config = validate_and_build(raw, &path)?;

        debug!(
            path = %path.display(),
            service_count = config.services.len(),
            "loaded palo configuration",
        );

        Ok(config)
    }

    pub fn into_app_state(self) -> AppState {
        let mut state = AppState::default();

        for service in self.services.into_values() {
            state.insert_service(service.definition);
        }

        state
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaloSettings {
    pub font_size: Option<u16>,
    pub log_retention: Option<usize>,
    pub telemetry_refresh: Option<Duration>,
    pub logs: LogSettings,
    pub mcp: McpSettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogSettings {
    pub enabled: bool,
    pub directory: PathBuf,
    pub palo: bool,
    pub apps: bool,
}

impl Default for LogSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: PathBuf::from(DEFAULT_LOG_DIRECTORY),
            palo: true,
            apps: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSettings {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub allowed_hosts: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub stateful: bool,
    pub json_response: bool,
    pub log_retention: usize,
}

impl Default for McpSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            host: DEFAULT_MCP_HOST.to_string(),
            port: DEFAULT_MCP_PORT,
            path: DEFAULT_MCP_PATH.to_string(),
            allowed_hosts: vec![
                "localhost".to_string(),
                "127.0.0.1".to_string(),
                "::1".to_string(),
            ],
            allowed_origins: Vec::new(),
            stateful: true,
            json_response: false,
            log_retention: DEFAULT_MCP_LOG_RETENTION,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredService {
    pub definition: ServiceDefinition,
    pub project_type: ProjectType,
    pub autostart: bool,
    pub target: TargetMode,
    pub executable: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProjectType {
    #[default]
    Generic,
    Rust,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TargetMode {
    #[default]
    Debug,
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigLoadError {
    errors: Vec<ConfigurationError>,
}

impl ConfigLoadError {
    pub fn new(errors: Vec<ConfigurationError>) -> Self {
        Self { errors }
    }

    pub fn single(error: ConfigurationError) -> Self {
        Self::new(vec![error])
    }

    pub fn errors(&self) -> &[ConfigurationError] {
        &self.errors
    }
}

impl fmt::Display for ConfigLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.errors.as_slice() {
            [] => f.write_str("configuration error: unknown configuration failure"),
            [error] => error.fmt(f),
            errors => {
                writeln!(f, "configuration errors:")?;

                for error in errors {
                    writeln!(f, "- {error}")?;
                }

                Ok(())
            }
        }
    }
}

impl std::error::Error for ConfigLoadError {}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    palo: RawPaloSection,
    #[serde(default)]
    services: BTreeMap<String, RawServiceConfig>,
}

#[derive(Debug, Deserialize, Default)]
struct RawPaloSection {
    #[serde(default)]
    settings: RawPaloSettings,
    #[serde(default)]
    mcp: RawMcpSettings,
}

#[derive(Debug, Deserialize, Default)]
struct RawPaloSettings {
    font_size: Option<u16>,
    log_retention: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    telemetry_refresh: Option<Duration>,
    #[serde(default)]
    logs: Option<RawLogSettings>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawLogSettings {
    Enabled(bool),
    Options(RawLogSettingsOptions),
}

#[derive(Debug, Deserialize, Default)]
struct RawLogSettingsOptions {
    enabled: Option<bool>,
    #[serde(alias = "dir", alias = "path")]
    directory: Option<PathBuf>,
    #[serde(alias = "capture_palo", alias = "capture_palo_logs")]
    palo: Option<bool>,
    #[serde(alias = "app", alias = "capture_apps", alias = "capture_app_logs")]
    apps: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
struct RawMcpSettings {
    enabled: Option<bool>,
    host: Option<String>,
    port: Option<u16>,
    path: Option<String>,
    allowed_hosts: Option<Vec<String>>,
    #[serde(default)]
    allowed_origins: Option<Vec<String>>,
    stateful: Option<bool>,
    json_response: Option<bool>,
    log_retention: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
struct RawServiceConfig {
    #[serde(rename = "type")]
    project_type: Option<String>,
    autostart: Option<bool>,
    target: Option<String>,
    executable: Option<RawCommandValue>,
    #[serde(default)]
    command: Option<RawCommandValue>,
    working_dir: Option<PathBuf>,
    log_retention: Option<usize>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    build: RawBuildConfig,
    #[serde(default, alias = "ready")]
    readiness: Option<RawReadinessConfig>,
    #[serde(default)]
    healthcheck: Option<RawHealthCheckConfig>,
    #[serde(default)]
    restart: Option<RawRestartConfig>,
    #[serde(default)]
    watch: RawWatchConfig,
    #[serde(default)]
    depends_on: RawDependsOn,
    #[serde(default)]
    hooks: RawHooksConfig,
}

#[derive(Debug, Deserialize, Default)]
struct RawBuildConfig {
    #[serde(default)]
    check: Option<RawCommandValue>,
    #[serde(default, alias = "cmd")]
    command: Option<RawCommandValue>,
}

#[derive(Debug, Deserialize)]
struct RawReadinessConfig {
    #[serde(default, alias = "cmd")]
    command: Option<RawCommandValue>,
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    initial_delay: Option<Duration>,
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    interval: Option<Duration>,
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    timeout: Option<Duration>,
    retries: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RawHealthCheckConfig {
    #[serde(default)]
    http: Option<RawHttpHealthProbe>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    expected_status: Option<RawExpectedStatus>,
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    initial_delay: Option<Duration>,
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    interval: Option<Duration>,
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    timeout: Option<Duration>,
    retries: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RawHttpHealthProbe {
    url: String,
    method: Option<String>,
    expected_status: Option<RawExpectedStatus>,
}

#[derive(Debug)]
enum RawExpectedStatus {
    Code(u16),
    Range(ExpectedStatusRange),
}

impl<'de> Deserialize<'de> for RawExpectedStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Value {
            Code(u16),
            Text(String),
        }

        match Value::deserialize(deserializer)? {
            Value::Code(code) => Ok(Self::Code(code)),
            Value::Text(value) => parse_expected_status_range(&value)
                .map(Self::Range)
                .map_err(serde::de::Error::custom),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawDependsOn {
    List(Vec<RawDependencyItem>),
    Map(BTreeMap<String, RawDependencyOptions>),
}

impl Default for RawDependsOn {
    fn default() -> Self {
        Self::List(Vec::new())
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawDependencyItem {
    Name(String),
    Detailed(RawDetailedDependency),
}

#[derive(Debug, Deserialize)]
struct RawDetailedDependency {
    #[serde(alias = "id", alias = "name")]
    service: String,
    condition: Option<String>,
    restart: Option<bool>,
    required: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    timeout: Option<Duration>,
}

#[derive(Debug, Deserialize, Default)]
struct RawDependencyOptions {
    condition: Option<String>,
    restart: Option<bool>,
    required: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    timeout: Option<Duration>,
}

#[derive(Debug, Deserialize, Default)]
struct RawRestartConfig {
    on: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    debounce: Option<Duration>,
    max_crash_retries: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    backoff: Option<Duration>,
}

#[derive(Debug, Deserialize, Default)]
struct RawWatchConfig {
    enabled: Option<bool>,
    #[serde(default, alias = "dir", alias = "paths")]
    paths: Vec<PathBuf>,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default, alias = "ignore", alias = "ignored_paths")]
    ignore_paths: Vec<PathBuf>,
    #[serde(default, alias = "ignore_regex", alias = "ignored_regex")]
    ignore_regex: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    debounce: Option<Duration>,
}

#[derive(Debug, Deserialize, Default)]
struct RawHooksConfig {
    #[serde(default)]
    pre_build: Option<RawHookCommands>,
    #[serde(default)]
    post_build: Option<RawHookCommands>,
    #[serde(default)]
    pre_start: Option<RawHookCommands>,
    #[serde(default)]
    post_start: Option<RawHookCommands>,
    #[serde(default)]
    pre_stop: Option<RawHookCommands>,
    #[serde(default)]
    post_stop: Option<RawHookCommands>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RawHookCommands {
    Single(RawCommandValue),
    Many(Vec<RawCommandValue>),
}

impl RawHookCommands {
    fn into_vec(self) -> Vec<RawCommandValue> {
        match self {
            Self::Single(command) => vec![command],
            Self::Many(commands) => commands,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RawCommandValue {
    String(String),
    Args(Vec<String>),
}

fn load_raw_config(path: &Path) -> Result<RawConfig, ConfigLoadError> {
    if !path.exists() {
        return Err(ConfigLoadError::single(
            ConfigurationError::new(format!("config file `{}` was not found", path.display()))
                .with_path(DEFAULT_CONFIG_FILE_NAME),
        ));
    }

    let config = Config::builder()
        .add_source(File::from(path).format(FileFormat::Yaml))
        .build()
        .map_err(|error| {
            ConfigLoadError::single(ConfigurationError::new(format!(
                "failed to read config file `{}`: {error}",
                path.display()
            )))
        })?;

    config.try_deserialize::<RawConfig>().map_err(|error| {
        ConfigLoadError::single(ConfigurationError::new(format!(
            "failed to parse config file `{}`: {error}",
            path.display()
        )))
    })
}

fn validate_and_build(raw: RawConfig, path: &Path) -> Result<PaloConfig, ConfigLoadError> {
    let workspace_root = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut errors = Vec::new();
    if raw.services.is_empty() {
        errors.push(
            ConfigurationError::new("at least one service must be defined").with_path("services"),
        );
    }

    let mut services = BTreeMap::new();
    let global_log_retention = validate_log_retention(
        raw.palo.settings.log_retention,
        "palo.settings.log_retention",
        &mut errors,
    );
    let logs = build_log_settings(raw.palo.settings.logs, &workspace_root, &mut errors);
    let mcp = build_mcp_settings(raw.palo.mcp, &mut errors);

    for (service_name, raw_service) in raw.services {
        match build_service(
            service_name,
            raw_service,
            &workspace_root,
            global_log_retention,
        ) {
            Ok(service) => {
                services.insert(service.definition.id.clone(), service);
            }
            Err(mut service_errors) => errors.append(&mut service_errors),
        }
    }

    validate_dependency_graph(&services, &mut errors);

    if errors.is_empty() {
        let config = PaloConfig {
            workspace_root,
            settings: PaloSettings {
                font_size: raw.palo.settings.font_size,
                log_retention: global_log_retention,
                telemetry_refresh: raw.palo.settings.telemetry_refresh,
                logs,
                mcp,
            },
            services,
        };
        debug!(
            path = %path.display(),
            service_count = config.services.len(),
            "validated palo configuration",
        );
        Ok(config)
    } else {
        warn!(
            path = %path.display(),
            error_count = errors.len(),
            "palo configuration validation failed",
        );
        Err(ConfigLoadError::new(errors))
    }
}

fn build_service(
    service_name: String,
    raw: RawServiceConfig,
    workspace_root: &Path,
    global_log_retention: Option<usize>,
) -> Result<ConfiguredService, Vec<ConfigurationError>> {
    let mut errors = Vec::new();
    let service_path = format!("services.{service_name}");

    let target = match parse_target_mode(raw.target.as_deref(), &service_path) {
        Ok(target) => target,
        Err(error) => {
            errors.push(error);
            TargetMode::Debug
        }
    };

    let project_type = match parse_project_type(raw.project_type.as_deref(), &service_path) {
        Ok(project_type) => project_type,
        Err(error) => {
            errors.push(error);
            ProjectType::Generic
        }
    };

    let working_dir = raw
        .working_dir
        .as_deref()
        .map(|path| normalize_path(workspace_root, path))
        .unwrap_or_else(|| workspace_root.to_path_buf());

    let autostart = raw.autostart.unwrap_or(true);
    let log_retention = validate_log_retention(
        raw.log_retention,
        &format!("{service_path}.log_retention"),
        &mut errors,
    )
    .or(global_log_retention)
    .unwrap_or(DEFAULT_SERVICE_LOG_RETENTION);

    let executable_was_configured = raw.executable.is_some();
    let command = match build_run_command(
        &service_name,
        raw.command,
        raw.executable,
        &raw.env,
        &working_dir,
    ) {
        Ok(command) => Some(command),
        Err(error) => {
            errors.push(error);
            None
        }
    };

    let build = build_build_definition(
        &service_name,
        &raw.build,
        &raw.env,
        &working_dir,
        &mut errors,
    );
    let readiness = build_readiness_check(
        &service_name,
        raw.readiness,
        &raw.env,
        &working_dir,
        &mut errors,
    );
    let healthcheck = build_health_check(&service_name, raw.healthcheck, &mut errors);
    if readiness.is_some() && healthcheck.is_some() {
        errors.push(
            ConfigurationError::new(
                "configure either legacy `readiness.command` or `healthcheck`, not both",
            )
            .with_path(format!("services.{service_name}.healthcheck")),
        );
    }
    let hooks = build_hooks(
        &service_name,
        &raw.hooks,
        &raw.env,
        &working_dir,
        &mut errors,
    );
    let restart = build_restart_policy(&service_name, raw.restart.as_ref(), &mut errors);
    let watch = build_watch_configuration(
        &service_name,
        &raw.watch,
        restart.as_ref().ok(),
        &working_dir,
        &mut errors,
    );
    let dependencies = build_dependencies(&service_name, raw.depends_on, &mut errors);

    if !errors.is_empty() {
        warn!(
            service = service_name,
            error_count = errors.len(),
            "service configuration failed validation",
        );
        return Err(errors);
    }

    let command = command.expect("command validated before success return");
    let executable = executable_was_configured.then(|| command_to_vec(&command));
    let restart = restart.expect("restart validated before success return");

    if !raw.env.is_empty() {
        debug!(
            service = service_name,
            env_var_count = raw.env.len(),
            "configured service environment variables",
        );
    }
    debug!(
        service = service_name,
        log_retention, "configured service in-memory log retention",
    );

    Ok(ConfiguredService {
        definition: ServiceDefinition {
            id: ServiceId::new(service_name.clone()),
            name: service_name.clone(),
            command,
            build,
            readiness,
            healthcheck,
            restart,
            watch,
            depends_on: dependencies
                .iter()
                .map(|dependency| dependency.service_id.clone())
                .collect(),
            dependencies,
            hooks,
            log_retention,
        },
        project_type,
        autostart,
        target,
        executable,
    })
}

fn validate_log_retention(
    value: Option<usize>,
    path: &str,
    errors: &mut Vec<ConfigurationError>,
) -> Option<usize> {
    match value {
        Some(0) => {
            errors.push(
                ConfigurationError::new("log_retention must be greater than zero").with_path(path),
            );
            None
        }
        Some(value) => Some(value),
        None => None,
    }
}

fn build_log_settings(
    raw: Option<RawLogSettings>,
    workspace_root: &Path,
    errors: &mut Vec<ConfigurationError>,
) -> LogSettings {
    let mut settings = LogSettings::default();

    match raw {
        None => {}
        Some(RawLogSettings::Enabled(enabled)) => settings.enabled = enabled,
        Some(RawLogSettings::Options(raw)) => {
            if let Some(enabled) = raw.enabled {
                settings.enabled = enabled;
            }
            if let Some(directory) = raw.directory {
                settings.directory = directory;
            }
            if let Some(palo) = raw.palo {
                settings.palo = palo;
            }
            if let Some(apps) = raw.apps {
                settings.apps = apps;
            }
        }
    }

    if settings.directory.as_os_str().is_empty() {
        errors.push(
            ConfigurationError::new("logs.directory must not be empty")
                .with_path("palo.settings.logs.directory"),
        );
        settings.directory = PathBuf::from(DEFAULT_LOG_DIRECTORY);
    }

    settings.directory = normalize_path(workspace_root, &settings.directory);

    debug!(
        enabled = settings.enabled,
        directory = %settings.directory.display(),
        capture_palo = settings.palo,
        capture_apps = settings.apps,
        "configured runtime file logging",
    );

    settings
}

fn build_mcp_settings(raw: RawMcpSettings, errors: &mut Vec<ConfigurationError>) -> McpSettings {
    let defaults = McpSettings::default();
    let host = raw.host.unwrap_or(defaults.host);
    let path = raw.path.unwrap_or(defaults.path);
    let log_retention = raw.log_retention.unwrap_or(defaults.log_retention);

    if host.trim().is_empty() {
        errors
            .push(ConfigurationError::new("MCP host must not be empty").with_path("palo.mcp.host"));
    }

    if path.trim().is_empty() || !path.starts_with('/') {
        errors.push(
            ConfigurationError::new("MCP path must start with `/`").with_path("palo.mcp.path"),
        );
    }

    if log_retention == 0 {
        errors.push(
            ConfigurationError::new("MCP log_retention must be greater than zero")
                .with_path("palo.mcp.log_retention"),
        );
    }

    McpSettings {
        enabled: raw.enabled.unwrap_or(defaults.enabled),
        host,
        port: raw.port.unwrap_or(defaults.port),
        path,
        allowed_hosts: raw.allowed_hosts.unwrap_or(defaults.allowed_hosts),
        allowed_origins: raw.allowed_origins.unwrap_or(defaults.allowed_origins),
        stateful: raw.stateful.unwrap_or(defaults.stateful),
        json_response: raw.json_response.unwrap_or(defaults.json_response),
        log_retention,
    }
}

fn validate_dependency_graph(
    services: &BTreeMap<ServiceId, ConfiguredService>,
    errors: &mut Vec<ConfigurationError>,
) {
    for (service_id, service) in services {
        for dependency in service.definition.dependency_contracts() {
            if dependency.service_id == *service_id {
                errors.push(
                    ConfigurationError::new("service cannot depend on itself")
                        .with_path(format!("services.{service_id}.depends_on")),
                );
                continue;
            }

            if dependency.required && !services.contains_key(&dependency.service_id) {
                errors.push(
                    ConfigurationError::new(format!(
                        "service `{service_id}` depends on unknown service `{}`",
                        dependency.service_id
                    ))
                    .with_path(format!("services.{service_id}.depends_on")),
                );
            }
        }
    }

    let mut permanent = BTreeSet::new();
    let mut temporary = BTreeSet::new();
    for service_id in services.keys() {
        validate_dependency_cycles(services, service_id, &mut permanent, &mut temporary, errors);
    }
}

fn validate_dependency_cycles(
    services: &BTreeMap<ServiceId, ConfiguredService>,
    service_id: &ServiceId,
    permanent: &mut BTreeSet<ServiceId>,
    temporary: &mut BTreeSet<ServiceId>,
    errors: &mut Vec<ConfigurationError>,
) {
    if permanent.contains(service_id) {
        return;
    }

    if !temporary.insert(service_id.clone()) {
        errors.push(
            ConfigurationError::new(format!(
                "dependency cycle detected at service `{service_id}`"
            ))
            .with_path(format!("services.{service_id}.depends_on")),
        );
        return;
    }

    let Some(service) = services.get(service_id) else {
        temporary.remove(service_id);
        return;
    };

    for dependency in service.definition.dependency_contracts() {
        if services.contains_key(&dependency.service_id) {
            validate_dependency_cycles(
                services,
                &dependency.service_id,
                permanent,
                temporary,
                errors,
            );
        }
    }

    temporary.remove(service_id);
    permanent.insert(service_id.clone());
}

fn build_run_command(
    service_name: &str,
    command: Option<RawCommandValue>,
    executable: Option<RawCommandValue>,
    env: &BTreeMap<String, String>,
    working_dir: &Path,
) -> Result<CommandSpec, ConfigurationError> {
    match (command, executable) {
        (Some(_), Some(_)) => Err(ConfigurationError::new(
            "use either `command` or `executable`, not both",
        )
        .with_path(format!("services.{service_name}"))),
        (Some(command), None) => parse_command(
            command,
            &format!("services.{service_name}.command"),
            env,
            working_dir,
        ),
        (None, Some(executable)) => parse_command(
            executable,
            &format!("services.{service_name}.executable"),
            env,
            working_dir,
        ),
        (None, None) => Err(ConfigurationError::new(
            "a service must define either `command` or `executable`",
        )
        .with_path(format!("services.{service_name}.command"))),
    }
}

fn command_to_vec(command: &CommandSpec) -> Vec<String> {
    std::iter::once(command.program.clone())
        .chain(command.args.iter().cloned())
        .collect()
}

fn build_build_definition(
    service_name: &str,
    raw: &RawBuildConfig,
    env: &BTreeMap<String, String>,
    working_dir: &Path,
    errors: &mut Vec<ConfigurationError>,
) -> BuildDefinition {
    let check = raw
        .check
        .clone()
        .map(|command| {
            parse_command(
                command,
                &format!("services.{service_name}.build.check"),
                env,
                working_dir,
            )
        })
        .transpose()
        .unwrap_or_else(|error| {
            errors.push(error);
            None
        });

    let build = raw
        .command
        .clone()
        .map(|command| {
            parse_command(
                command,
                &format!("services.{service_name}.build.cmd"),
                env,
                working_dir,
            )
        })
        .transpose()
        .unwrap_or_else(|error| {
            errors.push(error);
            None
        });

    BuildDefinition {
        check,
        build,
        hooks: Vec::new(),
    }
}

fn build_readiness_check(
    service_name: &str,
    raw: Option<RawReadinessConfig>,
    env: &BTreeMap<String, String>,
    working_dir: &Path,
    errors: &mut Vec<ConfigurationError>,
) -> Option<ReadinessCheck> {
    let Some(raw) = raw else {
        return None;
    };

    let path = format!("services.{service_name}.readiness");
    let command = match raw.command {
        Some(command) => parse_command(command, &format!("{path}.command"), env, working_dir),
        None => Err(
            ConfigurationError::new("readiness requires a `command` value")
                .with_path(format!("{path}.command")),
        ),
    }
    .unwrap_or_else(|error| {
        errors.push(error);
        CommandSpec::new("")
    });

    let retries = raw.retries.unwrap_or(DEFAULT_READINESS_RETRIES);
    if retries == 0 {
        errors.push(
            ConfigurationError::new("readiness.retries must be greater than zero")
                .with_path(format!("{path}.retries")),
        );
    }

    let interval = raw.interval.unwrap_or(DEFAULT_READINESS_INTERVAL);
    let timeout = raw.timeout.unwrap_or(DEFAULT_READINESS_TIMEOUT);

    if interval.is_zero() {
        errors.push(
            ConfigurationError::new("readiness.interval must be greater than zero")
                .with_path(format!("{path}.interval")),
        );
    }

    if timeout.is_zero() {
        errors.push(
            ConfigurationError::new("readiness.timeout must be greater than zero")
                .with_path(format!("{path}.timeout")),
        );
    }

    if command.program.is_empty() {
        return None;
    }

    Some(ReadinessCheck {
        command,
        initial_delay: raw.initial_delay.unwrap_or_default(),
        interval,
        timeout,
        retries,
    })
}

fn build_health_check(
    service_name: &str,
    raw: Option<RawHealthCheckConfig>,
    errors: &mut Vec<ConfigurationError>,
) -> Option<HealthCheck> {
    let Some(raw) = raw else {
        return None;
    };

    let root_path = format!("services.{service_name}.healthcheck");
    let (http, http_path) = match raw.http {
        Some(http) => (http, format!("{root_path}.http")),
        None => {
            let Some(url) = raw.url else {
                errors.push(
                    ConfigurationError::new("healthcheck requires `http.url` or `url`")
                        .with_path(format!("{root_path}.http.url")),
                );
                return None;
            };
            (
                RawHttpHealthProbe {
                    url,
                    method: raw.method,
                    expected_status: raw.expected_status,
                },
                root_path.clone(),
            )
        }
    };

    if reqwest::Url::parse(&http.url)
        .map(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
        .unwrap_or(false)
        == false
    {
        warn!(
            service = service_name,
            url = %http.url,
            "healthcheck URL failed validation",
        );
        errors.push(
            ConfigurationError::new("healthcheck URL must be an absolute HTTP or HTTPS URL")
                .with_path(format!("{http_path}.url")),
        );
    }

    let method = http
        .method
        .unwrap_or_else(|| DEFAULT_HTTP_HEALTH_METHOD.to_string())
        .to_ascii_uppercase();
    if reqwest::Method::from_bytes(method.as_bytes()).is_err() {
        warn!(
            service = service_name,
            method = %method,
            "healthcheck method failed validation",
        );
        errors.push(
            ConfigurationError::new("healthcheck method must be a valid HTTP method")
                .with_path(format!("{http_path}.method")),
        );
    }

    let expected_status = match http.expected_status {
        Some(RawExpectedStatus::Code(code)) => ExpectedStatusRange::new(code, code),
        Some(RawExpectedStatus::Range(range)) => range,
        None => DEFAULT_HTTP_HEALTH_EXPECTED_STATUS,
    };
    validate_status_range(
        expected_status,
        &format!("{http_path}.expected_status"),
        errors,
    );

    let retries = raw.retries.unwrap_or(DEFAULT_READINESS_RETRIES);
    if retries == 0 {
        errors.push(
            ConfigurationError::new("healthcheck.retries must be greater than zero")
                .with_path(format!("{root_path}.retries")),
        );
    }

    let interval = raw.interval.unwrap_or(DEFAULT_READINESS_INTERVAL);
    if interval.is_zero() {
        errors.push(
            ConfigurationError::new("healthcheck.interval must be greater than zero")
                .with_path(format!("{root_path}.interval")),
        );
    }

    let timeout = raw.timeout.unwrap_or(DEFAULT_READINESS_TIMEOUT);
    if timeout.is_zero() {
        errors.push(
            ConfigurationError::new("healthcheck.timeout must be greater than zero")
                .with_path(format!("{root_path}.timeout")),
        );
    }

    let initial_delay = raw.initial_delay.unwrap_or_default();
    if raw.initial_delay.is_some() && initial_delay.is_zero() {
        errors.push(
            ConfigurationError::new("healthcheck.initial_delay must be greater than zero when set")
                .with_path(format!("{root_path}.initial_delay")),
        );
    }

    Some(HealthCheck {
        http: HttpHealthProbe {
            url: http.url,
            method,
            expected_status,
        },
        initial_delay,
        interval,
        timeout,
        retries,
    })
}

fn validate_status_range(
    range: ExpectedStatusRange,
    path: &str,
    errors: &mut Vec<ConfigurationError>,
) {
    if range.start > range.end || range.start < 100 || range.end > 599 {
        errors.push(
            ConfigurationError::new(
                "healthcheck expected_status must be an HTTP status code or range within 100..599",
            )
            .with_path(path),
        );
    }
}

fn parse_expected_status_range(value: &str) -> Result<ExpectedStatusRange, String> {
    let trimmed = value.trim();
    let Some((start, end)) = trimmed
        .split_once("..=")
        .or_else(|| trimmed.split_once(".."))
    else {
        let code = trimmed
            .parse::<u16>()
            .map_err(|_| "expected status must be a code like 200 or range like 200..399")?;
        return Ok(ExpectedStatusRange::new(code, code));
    };

    let start = start
        .trim()
        .parse::<u16>()
        .map_err(|_| "expected status range start must be numeric")?;
    let end = end
        .trim()
        .parse::<u16>()
        .map_err(|_| "expected status range end must be numeric")?;
    Ok(ExpectedStatusRange::new(start, end))
}

fn build_dependencies(
    service_name: &str,
    raw: RawDependsOn,
    errors: &mut Vec<ConfigurationError>,
) -> Vec<ServiceDependency> {
    let mut dependencies = Vec::new();
    match raw {
        RawDependsOn::List(items) => {
            for (index, item) in items.into_iter().enumerate() {
                match item {
                    RawDependencyItem::Name(service) => dependencies.push(build_dependency(
                        service_name,
                        service,
                        None,
                        None,
                        None,
                        None,
                        &format!("services.{service_name}.depends_on[{index}]"),
                        errors,
                    )),
                    RawDependencyItem::Detailed(raw) => dependencies.push(build_dependency(
                        service_name,
                        raw.service,
                        raw.condition,
                        raw.restart,
                        raw.required,
                        raw.timeout,
                        &format!("services.{service_name}.depends_on[{index}]"),
                        errors,
                    )),
                }
            }
        }
        RawDependsOn::Map(entries) => {
            for (service, raw) in entries {
                let path = format!("services.{service_name}.depends_on.{service}");
                dependencies.push(build_dependency(
                    service_name,
                    service,
                    raw.condition,
                    raw.restart,
                    raw.required,
                    raw.timeout,
                    &path,
                    errors,
                ));
            }
        }
    }

    let mut seen = BTreeMap::<ServiceId, usize>::new();
    for (index, dependency) in dependencies.iter().enumerate() {
        if dependency.service_id.as_str().trim().is_empty() {
            errors.push(
                ConfigurationError::new("dependency service id must not be empty")
                    .with_path(format!("services.{service_name}.depends_on[{index}]")),
            );
        }

        if let Some(previous) = seen.insert(dependency.service_id.clone(), index) {
            errors.push(
                ConfigurationError::new(format!(
                    "dependency `{}` is declared more than once",
                    dependency.service_id
                ))
                .with_path(format!("services.{service_name}.depends_on[{previous}]")),
            );
        }
    }

    dependencies
}

fn build_dependency(
    service_name: &str,
    service: String,
    condition: Option<String>,
    restart: Option<bool>,
    required: Option<bool>,
    timeout: Option<Duration>,
    path: &str,
    errors: &mut Vec<ConfigurationError>,
) -> ServiceDependency {
    let condition = match parse_dependency_condition(condition.as_deref()) {
        Ok(condition) => condition,
        Err(message) => {
            errors.push(ConfigurationError::new(message).with_path(format!("{path}.condition")));
            DependencyCondition::Ready
        }
    };

    let wait_timeout = timeout.unwrap_or(DEFAULT_DEPENDENCY_WAIT_TIMEOUT);
    if wait_timeout.is_zero() {
        errors.push(
            ConfigurationError::new("dependency timeout must be greater than zero")
                .with_path(format!("{path}.timeout")),
        );
    }

    debug!(
        service = service_name,
        dependency = %service,
        condition = ?condition,
        restart = restart.unwrap_or(true),
        required = required.unwrap_or(true),
        wait_timeout_ms = wait_timeout.as_millis(),
        "configured service dependency",
    );

    ServiceDependency {
        service_id: ServiceId::new(service),
        condition,
        restart: restart.unwrap_or(true),
        required: required.unwrap_or(true),
        wait_timeout,
    }
}

fn parse_dependency_condition(value: Option<&str>) -> Result<DependencyCondition, String> {
    match value.unwrap_or("ready") {
        "started" | "service_started" => Ok(DependencyCondition::Started),
        "running" | "booted" | "service_running" => Ok(DependencyCondition::Running),
        "ready" | "healthy" | "service_ready" | "service_healthy" => Ok(DependencyCondition::Ready),
        other => Err(format!(
            "unsupported dependency condition `{other}`; expected one of started, running, ready"
        )),
    }
}

fn build_hooks(
    service_name: &str,
    raw: &RawHooksConfig,
    env: &BTreeMap<String, String>,
    working_dir: &Path,
    errors: &mut Vec<ConfigurationError>,
) -> Vec<HookDefinition> {
    let mut hooks = Vec::new();

    hooks.extend(parse_hook_phase(
        service_name,
        "pre_build",
        HookPhase::PreBuild,
        raw.pre_build.clone(),
        env,
        working_dir,
        errors,
    ));
    hooks.extend(parse_hook_phase(
        service_name,
        "post_build",
        HookPhase::PostBuild,
        raw.post_build.clone(),
        env,
        working_dir,
        errors,
    ));
    hooks.extend(parse_hook_phase(
        service_name,
        "pre_start",
        HookPhase::PreStart,
        raw.pre_start.clone(),
        env,
        working_dir,
        errors,
    ));
    hooks.extend(parse_hook_phase(
        service_name,
        "post_start",
        HookPhase::PostStart,
        raw.post_start.clone(),
        env,
        working_dir,
        errors,
    ));
    hooks.extend(parse_hook_phase(
        service_name,
        "pre_stop",
        HookPhase::PreStop,
        raw.pre_stop.clone(),
        env,
        working_dir,
        errors,
    ));
    hooks.extend(parse_hook_phase(
        service_name,
        "post_stop",
        HookPhase::PostStop,
        raw.post_stop.clone(),
        env,
        working_dir,
        errors,
    ));

    hooks
}

fn parse_hook_phase(
    service_name: &str,
    phase_name: &str,
    phase: HookPhase,
    commands: Option<RawHookCommands>,
    env: &BTreeMap<String, String>,
    working_dir: &Path,
    errors: &mut Vec<ConfigurationError>,
) -> Vec<HookDefinition> {
    let Some(commands) = commands else {
        return Vec::new();
    };

    let mut hooks = Vec::new();
    for (index, command) in commands.into_vec().into_iter().enumerate() {
        match parse_command(
            command,
            &format!("services.{service_name}.hooks.{phase_name}[{index}]"),
            env,
            working_dir,
        ) {
            Ok(command) => hooks.push(HookDefinition {
                name: format!("{phase_name}-{index}"),
                phase,
                command,
                required: true,
            }),
            Err(error) => errors.push(error),
        }
    }

    hooks
}

fn build_restart_policy(
    service_name: &str,
    raw: Option<&RawRestartConfig>,
    errors: &mut Vec<ConfigurationError>,
) -> Result<RestartPolicy, ()> {
    let Some(raw) = raw else {
        return Ok(RestartPolicy::Manual);
    };

    let path = format!("services.{service_name}.restart");
    let Some(mode) = raw.on.as_deref() else {
        errors.push(
            ConfigurationError::new("restart policy requires an `on` value")
                .with_path(format!("{path}.on")),
        );
        return Err(());
    };

    match mode {
        "manual" => {
            validate_absent(
                raw.max_crash_retries,
                &format!("{path}.max_crash_retries"),
                "restart.on = manual",
                errors,
            );
            validate_absent(
                raw.backoff,
                &format!("{path}.backoff"),
                "restart.on = manual",
                errors,
            );
            validate_absent(
                raw.debounce,
                &format!("{path}.debounce"),
                "restart.on = manual",
                errors,
            );
            Ok(RestartPolicy::Manual)
        }
        "mcp" => {
            validate_absent(
                raw.max_crash_retries,
                &format!("{path}.max_crash_retries"),
                "restart.on = mcp",
                errors,
            );
            validate_absent(
                raw.backoff,
                &format!("{path}.backoff"),
                "restart.on = mcp",
                errors,
            );
            validate_absent(
                raw.debounce,
                &format!("{path}.debounce"),
                "restart.on = mcp",
                errors,
            );
            Ok(RestartPolicy::Mcp)
        }
        "never" => {
            validate_absent(
                raw.max_crash_retries,
                &format!("{path}.max_crash_retries"),
                "restart.on = never",
                errors,
            );
            validate_absent(
                raw.backoff,
                &format!("{path}.backoff"),
                "restart.on = never",
                errors,
            );
            validate_absent(
                raw.debounce,
                &format!("{path}.debounce"),
                "restart.on = never",
                errors,
            );
            Ok(RestartPolicy::Never)
        }
        "change" | "on-change" => {
            validate_absent(
                raw.max_crash_retries,
                &format!("{path}.max_crash_retries"),
                "restart.on = change",
                errors,
            );
            validate_absent(
                raw.backoff,
                &format!("{path}.backoff"),
                "restart.on = change",
                errors,
            );
            Ok(RestartPolicy::OnChange)
        }
        "crash" | "on-crash" => {
            validate_absent(
                raw.debounce,
                &format!("{path}.debounce"),
                "restart.on = crash",
                errors,
            );
            Ok(RestartPolicy::OnCrash {
                max_retries: Some(raw.max_crash_retries.unwrap_or(DEFAULT_MAX_CRASH_RETRIES)),
                backoff: raw.backoff.unwrap_or(DEFAULT_RESTART_BACKOFF),
            })
        }
        "always" => {
            validate_absent(
                raw.max_crash_retries,
                &format!("{path}.max_crash_retries"),
                "restart.on = always",
                errors,
            );
            validate_absent(
                raw.debounce,
                &format!("{path}.debounce"),
                "restart.on = always",
                errors,
            );
            Ok(RestartPolicy::Always {
                backoff: raw.backoff.unwrap_or(DEFAULT_RESTART_BACKOFF),
            })
        }
        other => {
            errors.push(
                ConfigurationError::new(format!(
                    "unsupported restart policy `{other}`; expected one of manual, mcp, never, change, crash, always"
                ))
                .with_path(format!("{path}.on")),
            );
            Err(())
        }
    }
}

fn build_watch_configuration(
    service_name: &str,
    raw: &RawWatchConfig,
    restart: Option<&RestartPolicy>,
    working_dir: &Path,
    errors: &mut Vec<ConfigurationError>,
) -> WatchConfiguration {
    let enabled = raw
        .enabled
        .unwrap_or(matches!(restart, Some(RestartPolicy::OnChange)));
    let paths = if raw.paths.is_empty() && enabled {
        vec![working_dir.to_path_buf()]
    } else {
        raw.paths
            .iter()
            .map(|path| normalize_path(working_dir, path))
            .collect()
    };

    validate_watch_rule_scope(
        service_name,
        "include",
        raw.include.is_empty(),
        &paths,
        errors,
    );
    validate_watch_rule_scope(
        service_name,
        "exclude",
        raw.exclude.is_empty(),
        &paths,
        errors,
    );
    validate_watch_rule_scope(
        service_name,
        "ignore_paths",
        raw.ignore_paths.is_empty(),
        &paths,
        errors,
    );
    validate_watch_rule_scope(
        service_name,
        "ignore_regex",
        raw.ignore_regex.is_empty(),
        &paths,
        errors,
    );

    for (index, pattern) in raw.ignore_regex.iter().enumerate() {
        if let Err(error) = regex::Regex::new(pattern) {
            errors.push(
                ConfigurationError::new(format!("invalid watch ignore regex `{pattern}`: {error}"))
                    .with_path(format!(
                        "services.{service_name}.watch.ignore_regex[{index}]"
                    )),
            );
        }
    }

    let debounce = raw.debounce.unwrap_or_else(|| match restart {
        Some(RestartPolicy::OnChange) => DEFAULT_WATCH_DEBOUNCE,
        _ => DEFAULT_WATCH_DEBOUNCE,
    });

    WatchConfiguration {
        enabled,
        paths,
        include: raw.include.clone(),
        exclude: raw.exclude.clone(),
        ignore_paths: raw
            .ignore_paths
            .iter()
            .map(|path| normalize_path(working_dir, path))
            .collect(),
        ignore_regex: raw.ignore_regex.clone(),
        debounce,
    }
}

fn validate_watch_rule_scope(
    service_name: &str,
    field: &str,
    is_empty: bool,
    paths: &[PathBuf],
    errors: &mut Vec<ConfigurationError>,
) {
    if !is_empty && paths.is_empty() {
        errors.push(
            ConfigurationError::new(format!("watch.{field} requires at least one watch path"))
                .with_path(format!("services.{service_name}.watch.{field}")),
        );
    }
}

fn parse_command(
    raw: RawCommandValue,
    path: &str,
    env: &BTreeMap<String, String>,
    working_dir: &Path,
) -> Result<CommandSpec, ConfigurationError> {
    let parts = match raw {
        RawCommandValue::String(value) => shlex::split(&value).ok_or_else(|| {
            ConfigurationError::new("command contains invalid shell quoting").with_path(path)
        })?,
        RawCommandValue::Args(value) => value,
    };

    let Some(program) = parts.first() else {
        return Err(ConfigurationError::new("command must not be empty").with_path(path));
    };

    if program.trim().is_empty() {
        return Err(ConfigurationError::new("command program must not be empty").with_path(path));
    }

    Ok(CommandSpec {
        program: program.clone(),
        args: parts.iter().skip(1).cloned().collect(),
        env: env.clone(),
        working_dir: Some(working_dir.to_path_buf()),
    })
}

fn parse_project_type(
    value: Option<&str>,
    service_path: &str,
) -> Result<ProjectType, ConfigurationError> {
    match value.unwrap_or("generic") {
        "generic" => Ok(ProjectType::Generic),
        "rust" => Ok(ProjectType::Rust),
        other => Err(ConfigurationError::new(format!(
            "unsupported service type `{other}`; expected `generic` or `rust`"
        ))
        .with_path(format!("{service_path}.type"))),
    }
}

fn parse_target_mode(
    value: Option<&str>,
    service_path: &str,
) -> Result<TargetMode, ConfigurationError> {
    match value.unwrap_or("debug") {
        "debug" => Ok(TargetMode::Debug),
        "release" => Ok(TargetMode::Release),
        other => Err(ConfigurationError::new(format!(
            "unsupported target `{other}`; expected `debug` or `release`"
        ))
        .with_path(format!("{service_path}.target"))),
    }
}

fn validate_absent<T: Copy>(
    value: Option<T>,
    path: &str,
    context: &str,
    errors: &mut Vec<ConfigurationError>,
) {
    if value.is_some() {
        errors.push(
            ConfigurationError::new(format!("field is incompatible with {context}"))
                .with_path(path),
        );
    }
}

fn normalize_path(workspace_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace_root.join(path)
    }
}

fn deserialize_optional_duration<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum DurationValue {
        String(String),
        Integer(u64),
    }

    let value = Option::<DurationValue>::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(DurationValue::Integer(value)) => Ok(Some(Duration::from_millis(value))),
        Some(DurationValue::String(value)) => humantime::parse_duration(&value)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_string_command_into_program_and_args() {
        let command = parse_command(
            RawCommandValue::String("cargo run -p api".to_string()),
            "services.api.command",
            &BTreeMap::new(),
            Path::new("/workspace"),
        )
        .expect("command should parse");

        assert_eq!(command.program, "cargo");
        assert_eq!(command.args, vec!["run", "-p", "api"]);
        assert_eq!(command.working_dir, Some(PathBuf::from("/workspace")));
    }

    #[test]
    fn defaults_manual_restart_when_omitted() {
        let mut errors = Vec::new();
        let restart =
            build_restart_policy("api", None, &mut errors).expect("restart should default");

        assert_eq!(restart, RestartPolicy::Manual);
        assert!(errors.is_empty());
    }

    #[test]
    fn invalid_restart_combination_surfaces_path() {
        let mut errors = Vec::new();
        let raw = RawRestartConfig {
            on: Some("manual".to_string()),
            debounce: Some(Duration::from_secs(1)),
            max_crash_retries: None,
            backoff: None,
        };

        let _ = build_restart_policy("api", Some(&raw), &mut errors);

        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].path.as_deref(),
            Some("services.api.restart.debounce")
        );
    }

    #[test]
    fn parses_mcp_restart_policy() {
        let mut errors = Vec::new();
        let raw = RawRestartConfig {
            on: Some("mcp".to_string()),
            debounce: None,
            max_crash_retries: None,
            backoff: None,
        };

        let restart =
            build_restart_policy("api", Some(&raw), &mut errors).expect("policy should parse");

        assert_eq!(restart, RestartPolicy::Mcp);
        assert!(errors.is_empty());
    }

    #[test]
    fn mcp_restart_policy_does_not_enable_watch_by_default() {
        let workspace = tempfile::tempdir().expect("tempdir should exist");
        let service_dir = workspace.path().join("services").join("api");
        fs::create_dir_all(&service_dir).expect("service directory should exist");
        let config_path = workspace.path().join(DEFAULT_CONFIG_FILE_NAME);
        fs::write(
            &config_path,
            r#"
services:
  api:
    command: cargo run -p api
    working_dir: services/api
    restart:
      on: mcp
"#,
        )
        .expect("fixture should be written");

        let config = PaloConfig::from_path(config_path).expect("config should load");
        let api = config
            .services
            .get(&ServiceId::new("api"))
            .expect("api service should exist");

        assert_eq!(api.definition.restart, RestartPolicy::Mcp);
        assert!(!api.definition.watch.enabled);
        assert!(api.definition.watch.paths.is_empty());
    }

    #[test]
    fn config_load_error_formats_multiple_entries() {
        let error = ConfigLoadError::new(vec![
            ConfigurationError::new("missing required field").with_path("services.api.command"),
            ConfigurationError::new("invalid restart policy").with_path("services.api.restart.on"),
        ]);

        let rendered = error.to_string();
        assert!(rendered.contains("services.api.command"));
        assert!(rendered.contains("services.api.restart.on"));
    }

    #[test]
    fn missing_config_file_reports_default_name() {
        let error = PaloConfig::from_path(PathBuf::from("/definitely/missing/palo.yml"))
            .expect_err("missing file should error");

        assert_eq!(error.errors()[0].path.as_deref(), Some("palo.yml"));
    }

    #[test]
    fn into_app_state_keeps_service_runtime_slots() {
        let workspace = tempfile::tempdir().expect("tempdir should exist");
        let config_path = workspace.path().join(DEFAULT_CONFIG_FILE_NAME);
        fs::write(
            &config_path,
            r#"
services:
  api:
    command: cargo run -p api
"#,
        )
        .expect("fixture should be written");

        let config = PaloConfig::from_path(config_path).expect("config should load");
        let state = config.into_app_state();

        assert!(state.services.contains_key(&ServiceId::new("api")));
        assert!(state.runtime.contains_key(&ServiceId::new("api")));
    }

    #[test]
    fn on_change_services_default_watch_scope_to_service_working_dir() {
        let workspace = tempfile::tempdir().expect("tempdir should exist");
        let service_dir = workspace.path().join("services").join("api");
        fs::create_dir_all(&service_dir).expect("service directory should exist");
        let config_path = workspace.path().join(DEFAULT_CONFIG_FILE_NAME);
        fs::write(
            &config_path,
            r#"
services:
  api:
    command: cargo run -p api
    working_dir: services/api
    restart:
      on: change
"#,
        )
        .expect("fixture should be written");

        let config = PaloConfig::from_path(config_path).expect("config should load");
        let api = config
            .services
            .get(&ServiceId::new("api"))
            .expect("api service should exist");

        assert_eq!(api.definition.watch.paths, vec![service_dir]);
        assert!(api.definition.watch.enabled);
    }

    #[test]
    fn on_change_watch_rules_can_use_default_watch_scope() {
        let workspace = tempfile::tempdir().expect("tempdir should exist");
        let service_dir = workspace.path().join("services").join("api");
        fs::create_dir_all(&service_dir).expect("service directory should exist");
        let config_path = workspace.path().join(DEFAULT_CONFIG_FILE_NAME);
        fs::write(
            &config_path,
            r#"
services:
  api:
    command: cargo run -p api
    working_dir: services/api
    restart:
      on: change
    watch:
      include:
        - "src/**/*.rs"
      ignore_paths:
        - target
      ignore_regex:
        - "(^|/)generated/"
"#,
        )
        .expect("fixture should be written");

        let config = PaloConfig::from_path(config_path).expect("config should load");
        let api = config
            .services
            .get(&ServiceId::new("api"))
            .expect("api service should exist");

        assert_eq!(api.definition.watch.paths, vec![service_dir.clone()]);
        assert_eq!(
            api.definition.watch.ignore_paths,
            vec![service_dir.join("target")]
        );
        assert_eq!(
            api.definition.watch.ignore_regex,
            vec!["(^|/)generated/".to_string()]
        );
    }

    #[test]
    fn invalid_watch_ignore_regex_surfaces_path() {
        let workspace = tempfile::tempdir().expect("tempdir should exist");
        let config_path = workspace.path().join(DEFAULT_CONFIG_FILE_NAME);
        fs::write(
            &config_path,
            r#"
services:
  api:
    command: cargo run -p api
    restart:
      on: change
    watch:
      ignore_regex:
        - "["
"#,
        )
        .expect("fixture should be written");

        let error =
            PaloConfig::from_path(config_path).expect_err("invalid regex should fail validation");

        assert_eq!(
            error.errors()[0].path.as_deref(),
            Some("services.api.watch.ignore_regex[0]")
        );
    }
}
