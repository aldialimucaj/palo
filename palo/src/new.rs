use std::fmt;
use std::fs;
use std::path::PathBuf;

use palo_core::config::DEFAULT_CONFIG_FILE_NAME;
use tracing::{info, warn};

pub const TEMPLATE_CONFIG: &str = r#"# Palo workspace template.
# Keep this file at the workspace root and edit service IDs, commands, ports, and paths for your project.
palo:
  settings:
    font_size: 14 # Optional TUI font size hint for clients that support it.
    log_retention: 500 # Default number of in-memory app log lines kept by the UI.
    telemetry_refresh: 1s # How often process CPU and memory telemetry is refreshed.
    logs:
      enabled: true # Set false to disable file logging entirely.
      directory: .palo/logs # Run logs are written below this workspace-relative directory.
      palo: true # Capture Palo's own runtime logs.
      apps: true # Capture stdout and stderr from managed services.
  mcp:
    enabled: false # Set true to expose the MCP control server.
    host: 127.0.0.1 # Bind address for MCP HTTP transport.
    port: 9464 # Bind port for MCP HTTP transport.
    path: /mcp # MCP endpoint path; it must start with a slash.
    allowed_hosts: [localhost, 127.0.0.1, "::1"] # Host headers allowed to reach MCP.
    allowed_origins: [] # Browser origins allowed to reach MCP; add explicit origins when needed.
    stateful: true # Keep MCP sessions stateful across requests.
    json_response: false # Return normal streaming MCP responses unless a client requires JSON.
    log_retention: 512 # Number of MCP request log entries kept in memory.

services:
  api:
    type: rust # Supported values: rust, generic.
    autostart: true # Start this service automatically when `palo run` opens.
    target: debug # Rust target profile hint; use release for optimized binaries.
    log_retention: 500 # Override in-memory app log lines retained for this service.
    command: ["cargo", "run", "--package", "api"] # Main process command; use `executable` instead for a built binary.
    working_dir: . # Commands run relative to this directory.
    env:
      RUST_LOG: info # Environment values are applied to run, build, readiness, and hook commands.
      API_BIND: 127.0.0.1:8080
    build:
      check: ["cargo", "check", "--package", "api"] # Fast preflight command.
      cmd: ["cargo", "build", "--package", "api"] # Build command before starting the service.
    healthcheck:
      http:
        url: http://127.0.0.1:8080/health # Service is ready after this endpoint returns an expected status.
        method: GET
        expected_status: 200..399 # Accept a single code such as 200 or a range such as 200..399.
      initial_delay: 1s # Wait before the first health probe.
      interval: 1s # Delay between probes.
      timeout: 2s # Per-probe timeout.
      retries: 30 # Fail readiness after this many unsuccessful probes.
    depends_on:
      db:
        condition: ready # started, running, or ready.
        restart: true # Restart this service when the dependency is actively restarted.
        required: true # Set false for optional dependencies.
        timeout: 60s # Maximum wait for the dependency condition.
    restart:
      on: change # Supported values: manual, mcp, never, change, crash, always.
      debounce: 500ms # Only valid with restart.on = change.
    watch:
      enabled: true # On-change services default to watching their working_dir.
      paths:
        - .
      include:
        - "src/**/*.rs"
        - "Cargo.toml"
      exclude:
        - "target/**"
      ignore_paths:
        - ".palo" # Exact files or whole directories to ignore.
      ignore_regex:
        - "(^|/)generated/" # Regexes are matched against paths relative to watch.paths.
      debounce: 500ms # Watch-specific debounce override.
    hooks:
      pre_build:
        - ["cargo", "fmt", "--check"] # Runs before build.check and build.cmd.
      post_build:
        - ["cargo", "test", "--package", "api", "--no-run"] # Runs after a successful build.
      post_start:
        - ["sh", "-c", "echo api started"] # Runs after the process starts.
      pre_stop:
        - ["sh", "-c", "echo stopping api"] # Runs before Palo stops the process.

  worker:
    type: generic
    autostart: false # Keep optional background workers manual by default.
    command: ["cargo", "run", "--package", "worker"]
    working_dir: .
    readiness:
      command: ["sh", "-c", "test -f .palo/worker-ready"] # Legacy command readiness; do not combine with healthcheck.
      initial_delay: 500ms
      interval: 1s
      timeout: 2s
      retries: 10
    restart:
      on: crash # Restart when the process exits unexpectedly.
      max_crash_retries: 3 # Only valid with restart.on = crash.
      backoff: 1s # Delay before crash or always restarts.

  db:
    type: generic
    autostart: true
    executable: ["postgres", "-D", "data/postgres"] # Alternative to command for direct executable launches.
    working_dir: .
    restart:
      on: never # Palo will not restart this service automatically.

  docs:
    type: generic
    autostart: false
    command: ["npm", "run", "docs"]
    working_dir: docs
    depends_on: [api] # Short dependency form waits for the dependency to become ready.
    restart:
      on: manual # Restart only when requested from the UI or command surface.

  mcp-controlled:
    type: generic
    autostart: false
    command: ["./scripts/run-maintenance-task"]
    working_dir: .
    restart:
      on: mcp # Use MCP commands, not file watching, to restart this service.
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOptions {
    pub workspace_root: PathBuf,
    pub template: bool,
    pub overwrite: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOutcome {
    pub path: PathBuf,
    pub overwritten: bool,
    pub bytes_written: usize,
}

#[derive(Debug)]
pub enum NewError {
    TemplateRequired,
    ConfigAlreadyExists { path: PathBuf },
    Io(std::io::Error),
}

impl fmt::Display for NewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TemplateRequired => {
                write!(f, "`palo new` currently requires `--template`")
            }
            Self::ConfigAlreadyExists { path } => write!(
                f,
                "refusing to overwrite existing config `{}`; rerun with `palo new --template --overwrite`",
                path.display()
            ),
            Self::Io(error) => write!(f, "failed to write palo template: {error}"),
        }
    }
}

impl std::error::Error for NewError {}

impl From<std::io::Error> for NewError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub fn run_new(options: NewOptions) -> Result<NewOutcome, NewError> {
    if !options.template {
        warn!(
            workspace_root = %options.workspace_root.display(),
            "refusing to create palo config without a selected new command mode",
        );
        return Err(NewError::TemplateRequired);
    }

    let config_path = options.workspace_root.join(DEFAULT_CONFIG_FILE_NAME);
    if config_path.exists() && !options.overwrite {
        warn!(path = %config_path.display(), "refusing to overwrite existing palo template");
        return Err(NewError::ConfigAlreadyExists { path: config_path });
    }

    info!(
        workspace_root = %options.workspace_root.display(),
        path = %config_path.display(),
        overwrite = options.overwrite,
        "creating palo commented template",
    );

    fs::write(&config_path, TEMPLATE_CONFIG)?;

    info!(
        path = %config_path.display(),
        bytes_written = TEMPLATE_CONFIG.len(),
        overwritten = options.overwrite,
        "wrote palo commented template",
    );

    Ok(NewOutcome {
        path: config_path,
        overwritten: options.overwrite,
        bytes_written: TEMPLATE_CONFIG.len(),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use palo_core::config::PaloConfig;
    use palo_core::domain::{RestartPolicy, ServiceId};

    use super::{NewError, NewOptions, TEMPLATE_CONFIG, run_new};

    #[test]
    fn template_config_loads_through_core_schema() {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        fs::write(tempdir.path().join("palo.yml"), TEMPLATE_CONFIG).expect("template should write");

        let config = PaloConfig::from_workspace(tempdir.path()).expect("template should load");

        assert_eq!(config.services.len(), 5);
        assert_eq!(config.settings.log_retention, Some(500));
        assert!(config.settings.logs.enabled);
        assert!(!config.settings.mcp.enabled);
        assert_eq!(config.settings.mcp.log_retention, 512);

        let api = &config.services[&ServiceId::new("api")];
        assert_eq!(api.definition.log_retention, 500);
        assert_eq!(api.definition.restart, RestartPolicy::OnChange);
        assert!(api.definition.healthcheck.is_some());
        assert_eq!(api.definition.depends_on, vec![ServiceId::new("db")]);
        assert_eq!(api.definition.hooks.len(), 4);

        let worker = &config.services[&ServiceId::new("worker")];
        assert!(worker.definition.readiness.is_some());
        assert!(matches!(
            worker.definition.restart,
            RestartPolicy::OnCrash {
                max_retries: Some(3),
                ..
            }
        ));
    }

    #[test]
    fn new_template_writes_palo_yml() {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");

        let outcome = run_new(NewOptions {
            workspace_root: tempdir.path().to_path_buf(),
            template: true,
            overwrite: false,
        })
        .expect("new template should succeed");

        assert_eq!(outcome.path, tempdir.path().join("palo.yml"));
        assert!(!outcome.overwritten);
        assert_eq!(outcome.bytes_written, TEMPLATE_CONFIG.len());

        let rendered =
            fs::read_to_string(tempdir.path().join("palo.yml")).expect("template should read");
        assert!(
            rendered.contains("# Supported values: manual, mcp, never, change, crash, always.")
        );
        PaloConfig::from_workspace(tempdir.path()).expect("written template should load");
    }

    #[test]
    fn new_template_refuses_to_overwrite_without_flag() {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let config_path = tempdir.path().join("palo.yml");
        fs::write(&config_path, "services: {}\n").expect("seed config should write");

        let error = run_new(NewOptions {
            workspace_root: tempdir.path().to_path_buf(),
            template: true,
            overwrite: false,
        })
        .expect_err("existing config should be preserved");

        match error {
            NewError::ConfigAlreadyExists { path } => assert_eq!(path, config_path),
            other => panic!("expected overwrite error, got {other:?}"),
        }
    }

    #[test]
    fn overwrite_flag_replaces_existing_config_with_template() {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let config_path = tempdir.path().join("palo.yml");
        fs::write(&config_path, "services: {}\n").expect("seed config should write");

        let outcome = run_new(NewOptions {
            workspace_root: tempdir.path().to_path_buf(),
            template: true,
            overwrite: true,
        })
        .expect("overwrite should succeed");

        assert!(outcome.overwritten);
        let rendered = fs::read_to_string(config_path).expect("config should be readable");
        assert!(rendered.contains("Palo workspace template"));
        assert!(!rendered.contains("services: {}"));
    }
}
