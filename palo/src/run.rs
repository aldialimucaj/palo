use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

use palo_core::config::{ConfigLoadError, PaloConfig};
use palo_core::events::{CommandKind, CommandRequest, EventPayload};
use palo_core::orchestration::Orchestrator;
use tracing::{debug, info, warn};

use crate::logging::{AppLogCollector, RuntimeLogConfig};
use crate::mcp;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOptions {
    pub workspace_root: PathBuf,
    pub config_path: Option<PathBuf>,
}

#[derive(Debug)]
pub enum RunError {
    Config(ConfigLoadError),
    Runtime(color_eyre::Report),
    Shutdown(Vec<String>),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => error.fmt(f),
            Self::Runtime(error) => write!(f, "failed to start palo runtime: {error}"),
            Self::Shutdown(errors) => {
                write!(
                    f,
                    "failed to stop {} palo-managed service(s) before exit",
                    errors.len()
                )
            }
        }
    }
}

impl std::error::Error for RunError {}

impl From<ConfigLoadError> for RunError {
    fn from(value: ConfigLoadError) -> Self {
        Self::Config(value)
    }
}

impl From<color_eyre::Report> for RunError {
    fn from(value: color_eyre::Report) -> Self {
        Self::Runtime(value)
    }
}

pub async fn run_app(options: RunOptions) -> Result<(), RunError> {
    let config = load_config(&options)?;
    let logging = RuntimeLogConfig::from_config(&config, SystemTime::now());

    run_app_with_config(options, config, logging).await
}

pub async fn run_app_with_config(
    options: RunOptions,
    config: PaloConfig,
    logging: RuntimeLogConfig,
) -> Result<(), RunError> {
    let mcp_settings = config.settings.mcp.clone();
    let autostart_services = autostart_service_ids(&config);
    let orchestrator = Orchestrator::new(config.into_app_state());
    let events = orchestrator.events();
    let app_log_collector = AppLogCollector::spawn(events.clone(), &logging);
    let mcp_server = if mcp_settings.enabled {
        let server = mcp::spawn_mcp_server(mcp_settings, orchestrator.clone()).await?;
        info!(endpoint = %server.endpoint(), "palo MCP server is enabled");
        Some(server)
    } else {
        debug!("palo MCP server is disabled");
        None
    };
    let app = palo_tui::build_app(&orchestrator).await;

    info!(
        workspace_root = %options.workspace_root.display(),
        autostart_service_count = autostart_services.len(),
        run_log_directory = %logging
            .run_directory()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "disabled".to_string()),
        capture_app_logs = logging.app_logs_enabled(),
        "starting palo runtime",
    );

    for service_id in autostart_services {
        debug!(service_id = %service_id, "queueing autostart service");
        let _ = events.publish(EventPayload::CommandRequested(CommandRequest::for_service(
            service_id,
            CommandKind::Start,
        )));
    }

    let run_result = palo_tui::run_app(app).await;
    if let Some(server) = mcp_server {
        server.shutdown().await;
    }
    let shutdown_result = shutdown_runtime(&orchestrator).await;
    if let Some(collector) = app_log_collector {
        collector.shutdown().await;
    }
    run_result?;
    shutdown_result?;

    Ok(())
}

async fn shutdown_runtime(orchestrator: &Orchestrator) -> Result<(), RunError> {
    info!("stopping all palo-managed services before runtime exit");

    let results = orchestrator.stop_all().await;
    let mut stopped_count = 0usize;
    let mut errors = Vec::new();

    for result in results {
        match result {
            Ok(result) => {
                stopped_count += 1;
                debug!(
                    service_id = %result.service_id,
                    exit_code = result.exit_code,
                    success = result.success,
                    "stopped palo-managed service during runtime shutdown",
                );
            }
            Err(error) => {
                warn!(
                    error = %error,
                    "failed to stop palo-managed service during runtime shutdown",
                );
                errors.push(error.to_string());
            }
        }
    }

    if errors.is_empty() {
        info!(
            stopped_service_count = stopped_count,
            "finished stopping palo-managed services before runtime exit",
        );
        Ok(())
    } else {
        Err(RunError::Shutdown(errors))
    }
}

pub fn load_config(options: &RunOptions) -> Result<PaloConfig, ConfigLoadError> {
    match &options.config_path {
        Some(config_path) => {
            info!(path = %config_path.display(), "loading palo runtime from explicit config path");
            PaloConfig::from_path(config_path.clone())
        }
        None => {
            info!(
                workspace_root = %options.workspace_root.display(),
                "loading palo runtime from workspace root",
            );
            PaloConfig::from_workspace(options.workspace_root.clone())
        }
    }
}

fn autostart_service_ids(config: &PaloConfig) -> Vec<palo_core::domain::ServiceId> {
    config
        .services
        .iter()
        .filter_map(|(service_id, service)| service.autostart.then_some(service_id.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{RunOptions, autostart_service_ids, load_config, shutdown_runtime};
    use palo_core::config::PaloConfig;
    use palo_core::domain::{
        AppState, BuildDefinition, CommandSpec, DEFAULT_SERVICE_LOG_RETENTION, LifecycleState,
        RestartPolicy, ServiceDefinition, ServiceId, WatchConfiguration,
    };
    use palo_core::orchestration::Orchestrator;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn autostart_service_ids_only_returns_enabled_services() {
        let tempdir = tempdir().expect("tempdir should be created");
        fs::write(
            tempdir.path().join("palo.yml"),
            concat!(
                "services:\n",
                "  api:\n",
                "    command: [\"echo\", \"api\"]\n",
                "    autostart: true\n",
                "  worker:\n",
                "    command: [\"echo\", \"worker\"]\n",
                "    autostart: false\n",
            ),
        )
        .expect("config should be written");
        let config =
            PaloConfig::from_workspace(tempdir.path()).expect("fixture config should load");

        let autostart = autostart_service_ids(&config);

        assert_eq!(autostart.len(), 1);
        assert_eq!(autostart[0].as_str(), "api");
    }

    #[test]
    fn load_config_uses_workspace_root_by_default() {
        let tempdir = tempdir().expect("tempdir should be created");
        fs::write(
            tempdir.path().join("palo.yml"),
            "services:\n  app:\n    command: [\"echo\", \"hello\"]\n    autostart: false\n",
        )
        .expect("config should be written");

        let config = load_config(&RunOptions {
            workspace_root: tempdir.path().to_path_buf(),
            config_path: None,
        })
        .expect("config should load");

        assert!(config.services.contains_key(&"app".into()));
    }

    #[tokio::test]
    async fn shutdown_runtime_stops_running_services() {
        let service = ServiceDefinition {
            id: ServiceId::new("api"),
            name: "api".to_string(),
            command: CommandSpec::new("sh")
                .with_args(["-c", r#"trap 'exit 0' TERM; while :; do sleep 1; done"#]),
            build: BuildDefinition {
                check: None,
                build: None,
                hooks: Vec::new(),
            },
            readiness: None,
            healthcheck: None,
            restart: RestartPolicy::Manual,
            watch: WatchConfiguration::disabled(),
            dependencies: Vec::new(),
            depends_on: Vec::new(),
            hooks: Vec::new(),
            log_retention: DEFAULT_SERVICE_LOG_RETENTION,
        };
        let mut state = AppState::default();
        state.insert_service(service);
        let orchestrator = Orchestrator::new(state);

        orchestrator
            .start_service(&ServiceId::new("api"))
            .await
            .expect("service should start");

        shutdown_runtime(&orchestrator)
            .await
            .expect("shutdown should stop services");

        let state = orchestrator.snapshot_state().await;
        let runtime = state
            .runtime
            .get(&ServiceId::new("api"))
            .expect("runtime should exist");

        assert_eq!(runtime.lifecycle, LifecycleState::Stopped);
        assert_eq!(runtime.pid, None);
    }
}
