use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use color_eyre::eyre::{Context, Result};
use palo_core::config::McpSettings;
use palo_core::domain::{
    AppState, CommandSpec, DependencyCondition, LifecycleState, RestartPolicy, ServiceDefinition,
    ServiceHealth, ServiceId, ServiceRuntime,
};
use palo_core::events::{EventBus, EventPayload, LogEvent, LogOrigin, LogStream};
use palo_core::orchestration::Orchestrator;
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ErrorData as McpError, Json, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub struct McpServerHandle {
    endpoint: String,
    cancellation: CancellationToken,
    task: JoinHandle<()>,
}

impl McpServerHandle {
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub async fn shutdown(self) {
        info!(endpoint = %self.endpoint, "stopping palo MCP server");
        self.cancellation.cancel();

        if let Err(error) = self.task.await {
            warn!(error = %error, "palo MCP server task did not shut down cleanly");
        }
    }
}

pub async fn spawn_mcp_server(
    settings: McpSettings,
    orchestrator: Orchestrator,
) -> Result<McpServerHandle> {
    let cancellation = CancellationToken::new();
    let logs = RecentLogStore::new(settings.log_retention);
    logs.spawn_collector(orchestrator.events());

    let mut transport_config = StreamableHttpServerConfig::default()
        .with_stateful_mode(settings.stateful)
        .with_json_response(settings.json_response)
        .with_cancellation_token(cancellation.child_token());
    transport_config.allowed_hosts = settings.allowed_hosts.clone();
    transport_config.allowed_origins = settings.allowed_origins.clone();

    let service_orchestrator = orchestrator.clone();
    let service_logs = logs.clone();
    let service: StreamableHttpService<PaloMcpServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || {
                Ok(PaloMcpServer::new(
                    service_orchestrator.clone(),
                    service_logs.clone(),
                ))
            },
            Default::default(),
            transport_config,
        );

    let path = settings.path.clone();
    let router = Router::new().nest_service(&path, service);
    let listener = tokio::net::TcpListener::bind((settings.host.as_str(), settings.port))
        .await
        .with_context(|| {
            format!(
                "failed to bind MCP server to {}:{}",
                settings.host, settings.port
            )
        })?;
    let bound_addr = listener
        .local_addr()
        .context("failed to read bound MCP server address")?;
    let endpoint = format!(
        "http://{}:{}{}",
        display_host(&settings.host),
        bound_addr.port(),
        path
    );

    info!(
        host = %settings.host,
        port = bound_addr.port(),
        path = %path,
        endpoint = %endpoint,
        stateful = settings.stateful,
        json_response = settings.json_response,
        "starting palo MCP server",
    );

    let shutdown = cancellation.clone();
    let task = tokio::spawn(async move {
        let result = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                shutdown.cancelled_owned().await;
            })
            .await;

        match result {
            Ok(()) => info!("palo MCP server stopped"),
            Err(error) => error!(error = %error, "palo MCP server failed"),
        }
    });

    Ok(McpServerHandle {
        endpoint,
        cancellation,
        task,
    })
}

#[derive(Clone)]
struct PaloMcpServer {
    orchestrator: Orchestrator,
    logs: RecentLogStore,
    tool_router: ToolRouter<Self>,
}

impl PaloMcpServer {
    fn new(orchestrator: Orchestrator, logs: RecentLogStore) -> Self {
        Self {
            orchestrator,
            logs,
            tool_router: Self::tool_router(),
        }
    }

    async fn snapshot(&self) -> AppState {
        self.orchestrator.snapshot_state().await
    }

    async fn operation_response(
        &self,
        message: impl Into<String>,
        service_id: Option<&ServiceId>,
        commands_run: Option<usize>,
    ) -> OperationResponse {
        let snapshot = self.snapshot().await;
        OperationResponse {
            message: message.into(),
            service: service_id.and_then(|service_id| service_status(&snapshot, service_id)),
            summary: runtime_summary(&snapshot),
            commands_run,
        }
    }
}

#[tool_router(router = tool_router)]
impl PaloMcpServer {
    #[tool(
        name = "runtime_status",
        description = "Show aggregate Palo runtime status and resource usage."
    )]
    async fn runtime_status(&self) -> Json<RuntimeStatusResponse> {
        debug!("MCP requested runtime status");
        let snapshot = self.snapshot().await;
        Json(RuntimeStatusResponse {
            summary: runtime_summary(&snapshot),
        })
    }

    #[tool(
        name = "list_services",
        description = "List Palo services with lifecycle, process, restart, watch, and telemetry status."
    )]
    async fn list_services(&self) -> Json<ListServicesResponse> {
        debug!("MCP requested service list");
        let snapshot = self.snapshot().await;
        let services = snapshot
            .services
            .iter()
            .filter_map(|(service_id, _)| service_status(&snapshot, service_id))
            .collect();

        Json(ListServicesResponse {
            summary: runtime_summary(&snapshot),
            services,
        })
    }

    #[tool(
        name = "get_service",
        description = "Show the full Palo runtime status for a single service."
    )]
    async fn get_service(
        &self,
        Parameters(request): Parameters<ServiceRequest>,
    ) -> Result<Json<ServiceStatusResponse>, McpError> {
        let service_id = parse_service_id(request.service_id)?;
        debug!(service_id = %service_id, "MCP requested service status");
        let snapshot = self.snapshot().await;
        let service = service_status(&snapshot, &service_id).ok_or_else(|| {
            McpError::invalid_params(format!("service `{service_id}` is not defined"), None)
        })?;

        Ok(Json(ServiceStatusResponse { service }))
    }

    #[tool(
        name = "list_processes",
        description = "List currently running Palo-managed service processes."
    )]
    async fn list_processes(&self) -> Json<ListProcessesResponse> {
        debug!("MCP requested process list");
        let snapshot = self.snapshot().await;
        let processes = snapshot
            .services
            .keys()
            .filter_map(|service_id| process_status(&snapshot, service_id))
            .collect();

        Json(ListProcessesResponse { processes })
    }

    #[tool(
        name = "start_service",
        description = "Start a Palo service and its dependencies."
    )]
    async fn start_service(
        &self,
        Parameters(request): Parameters<ServiceRequest>,
    ) -> Result<Json<OperationResponse>, McpError> {
        let service_id = parse_service_id(request.service_id)?;
        info!(service_id = %service_id, "MCP requested service start");
        self.orchestrator
            .start_service(&service_id)
            .await
            .map_err(map_palo_error)?;

        Ok(Json(
            self.operation_response(
                format!("started service `{service_id}`"),
                Some(&service_id),
                None,
            )
            .await,
        ))
    }

    #[tool(
        name = "stop_service",
        description = "Stop a Palo service and its dependents."
    )]
    async fn stop_service(
        &self,
        Parameters(request): Parameters<ServiceRequest>,
    ) -> Result<Json<OperationResponse>, McpError> {
        let service_id = parse_service_id(request.service_id)?;
        info!(service_id = %service_id, "MCP requested service stop");
        self.orchestrator
            .stop_service(&service_id)
            .await
            .map_err(map_palo_error)?;

        Ok(Json(
            self.operation_response(
                format!("stopped service `{service_id}`"),
                Some(&service_id),
                None,
            )
            .await,
        ))
    }

    #[tool(name = "restart_service", description = "Restart a Palo service.")]
    async fn restart_service(
        &self,
        Parameters(request): Parameters<ServiceRequest>,
    ) -> Result<Json<OperationResponse>, McpError> {
        let service_id = parse_service_id(request.service_id)?;
        info!(service_id = %service_id, "MCP requested service restart");
        self.orchestrator
            .restart_service(&service_id)
            .await
            .map_err(map_palo_error)?;

        Ok(Json(
            self.operation_response(
                format!("restarted service `{service_id}`"),
                Some(&service_id),
                None,
            )
            .await,
        ))
    }

    #[tool(
        name = "check_service",
        description = "Run the configured check command for a service."
    )]
    async fn check_service(
        &self,
        Parameters(request): Parameters<ServiceRequest>,
    ) -> Result<Json<OperationResponse>, McpError> {
        let service_id = parse_service_id(request.service_id)?;
        info!(service_id = %service_id, "MCP requested service check");
        let commands_run = self
            .orchestrator
            .check_service(&service_id)
            .await
            .map_err(map_palo_error)?;

        Ok(Json(
            self.operation_response(
                format!("checked service `{service_id}`"),
                Some(&service_id),
                Some(commands_run),
            )
            .await,
        ))
    }

    #[tool(
        name = "build_service",
        description = "Run the configured build pipeline for a service."
    )]
    async fn build_service(
        &self,
        Parameters(request): Parameters<ServiceRequest>,
    ) -> Result<Json<OperationResponse>, McpError> {
        let service_id = parse_service_id(request.service_id)?;
        info!(service_id = %service_id, "MCP requested service build");
        let commands_run = self
            .orchestrator
            .build_service(&service_id)
            .await
            .map_err(map_palo_error)?;

        Ok(Json(
            self.operation_response(
                format!("built service `{service_id}`"),
                Some(&service_id),
                Some(commands_run),
            )
            .await,
        ))
    }

    #[tool(
        name = "start_all_services",
        description = "Start all Palo services in dependency order."
    )]
    async fn start_all_services(&self) -> Result<Json<OperationResponse>, McpError> {
        info!("MCP requested all services start");
        self.orchestrator
            .start_all()
            .await
            .map_err(map_palo_error)?;
        Ok(Json(
            self.operation_response("started all services", None, None)
                .await,
        ))
    }

    #[tool(
        name = "stop_all_services",
        description = "Stop all Palo services in dependent-first order."
    )]
    async fn stop_all_services(&self) -> Result<Json<OperationResponse>, McpError> {
        info!("MCP requested all services stop");
        for result in self.orchestrator.stop_all().await {
            result.map_err(map_palo_error)?;
        }

        Ok(Json(
            self.operation_response("stopped all services", None, None)
                .await,
        ))
    }

    #[tool(
        name = "restart_all_services",
        description = "Restart all Palo services."
    )]
    async fn restart_all_services(&self) -> Result<Json<OperationResponse>, McpError> {
        info!("MCP requested all services restart");
        for result in self.orchestrator.stop_all().await {
            result.map_err(map_palo_error)?;
        }
        self.orchestrator
            .start_all()
            .await
            .map_err(map_palo_error)?;

        Ok(Json(
            self.operation_response("restarted all services", None, None)
                .await,
        ))
    }

    #[tool(
        name = "check_all_services",
        description = "Run configured check commands for all services."
    )]
    async fn check_all_services(&self) -> Result<Json<OperationResponse>, McpError> {
        info!("MCP requested all services check");
        let commands_run = self
            .orchestrator
            .check_all()
            .await
            .map_err(map_palo_error)?;

        Ok(Json(
            self.operation_response("checked all services", None, Some(commands_run))
                .await,
        ))
    }

    #[tool(
        name = "build_all_services",
        description = "Run configured build pipelines for all services."
    )]
    async fn build_all_services(&self) -> Result<Json<OperationResponse>, McpError> {
        info!("MCP requested all services build");
        let commands_run = self
            .orchestrator
            .build_all()
            .await
            .map_err(map_palo_error)?;

        Ok(Json(
            self.operation_response("built all services", None, Some(commands_run))
                .await,
        ))
    }

    #[tool(
        name = "show_logs",
        description = "Show recent Palo-captured stdout or stderr logs, optionally filtered by service."
    )]
    async fn show_logs(
        &self,
        Parameters(request): Parameters<ShowLogsRequest>,
    ) -> Result<Json<ShowLogsResponse>, McpError> {
        if let Some(service_id) = &request.service_id {
            let service_id = ServiceId::new(service_id.clone());
            let snapshot = self.snapshot().await;
            if !snapshot.services.contains_key(&service_id) {
                return Err(McpError::invalid_params(
                    format!("service `{service_id}` is not defined"),
                    None,
                ));
            }
        }

        let stream = request
            .stream
            .as_deref()
            .map(parse_log_stream)
            .transpose()?;
        let requested_limit = request.limit.unwrap_or(100);
        let limit = requested_limit.min(self.logs.retention());

        debug!(
            service_id = ?request.service_id,
            stream = ?stream,
            limit,
            "MCP requested service logs",
        );

        let logs = self
            .logs
            .query(request.service_id.as_deref(), stream, limit)
            .await;

        Ok(Json(ShowLogsResponse {
            returned: logs.len(),
            retained: self.logs.len().await,
            logs,
        }))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PaloMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Control the local Palo runtime: inspect services/processes/logs and run start, stop, restart, check, and build operations.",
        )
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ServiceRequest {
    #[schemars(description = "The Palo service id from palo.yml.")]
    service_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ShowLogsRequest {
    #[schemars(description = "Optional Palo service id to filter logs.")]
    service_id: Option<String>,
    #[schemars(description = "Optional stream filter: stdout or stderr.")]
    stream: Option<String>,
    #[schemars(description = "Maximum number of recent log lines to return.")]
    limit: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RuntimeStatusResponse {
    summary: RuntimeSummary,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ListServicesResponse {
    summary: RuntimeSummary,
    services: Vec<ServiceStatus>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ServiceStatusResponse {
    service: ServiceStatus,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ListProcessesResponse {
    processes: Vec<ProcessStatus>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct OperationResponse {
    message: String,
    service: Option<ServiceStatus>,
    summary: RuntimeSummary,
    commands_run: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ShowLogsResponse {
    returned: usize,
    retained: usize,
    logs: Vec<LogLine>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RuntimeSummary {
    total_services: usize,
    running_services: usize,
    failed_services: usize,
    aggregate_cpu_millis: u64,
    aggregate_memory_bytes: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ServiceStatus {
    id: String,
    name: String,
    lifecycle: String,
    health: String,
    pid: Option<u32>,
    started_at_unix_ms: Option<u64>,
    last_exit_code: Option<i32>,
    last_error: Option<String>,
    command: Vec<String>,
    working_dir: Option<String>,
    restart_policy: String,
    watch_enabled: bool,
    watch_paths: Vec<String>,
    watch_include: Vec<String>,
    watch_exclude: Vec<String>,
    watch_ignore_paths: Vec<String>,
    watch_ignore_regex: Vec<String>,
    depends_on: Vec<String>,
    dependencies: Vec<DependencyStatus>,
    readiness: Option<ReadinessStatus>,
    healthcheck: Option<HealthCheckStatus>,
    telemetry: Option<TelemetryStatus>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct DependencyStatus {
    service_id: String,
    condition: String,
    restart: bool,
    required: bool,
    wait_timeout_ms: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ReadinessStatus {
    command: Vec<String>,
    initial_delay_ms: u64,
    interval_ms: u64,
    timeout_ms: u64,
    retries: u32,
}

#[derive(Debug, Serialize, JsonSchema)]
struct HealthCheckStatus {
    url: String,
    method: String,
    expected_status: String,
    initial_delay_ms: u64,
    interval_ms: u64,
    timeout_ms: u64,
    retries: u32,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ProcessStatus {
    service_id: String,
    pid: u32,
    lifecycle: String,
    health: String,
    cpu_millis: u64,
    memory_bytes: u64,
    uptime_ms: Option<u64>,
    open_ports: Vec<u16>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct TelemetryStatus {
    collected_at_unix_ms: u64,
    cpu_millis: u64,
    memory_bytes: u64,
    uptime_ms: Option<u64>,
    open_ports: Vec<u16>,
    disk_read_bytes: Option<u64>,
    disk_written_bytes: Option<u64>,
    network_rx_bytes: Option<u64>,
    network_tx_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct LogLine {
    emitted_at_unix_ms: u64,
    service_id: String,
    origin: String,
    stream: String,
    message: String,
}

#[derive(Clone)]
struct RecentLogStore {
    inner: Arc<Mutex<VecDeque<LogLine>>>,
    retention: usize,
}

impl RecentLogStore {
    fn new(retention: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(retention.min(4096)))),
            retention,
        }
    }

    fn retention(&self) -> usize {
        self.retention
    }

    fn spawn_collector(&self, events: EventBus) {
        let store = self.clone();
        tokio::spawn(async move {
            let mut receiver = events.subscribe();
            info!(
                retention = store.retention,
                "starting MCP log event collector"
            );

            loop {
                match receiver.recv().await {
                    Ok(event) => {
                        if let EventPayload::LogEmitted(log) = event.payload {
                            store.push(event.emitted_at, log).await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(skipped, "MCP log collector skipped lagged events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        info!("MCP log event collector stopped");
                        break;
                    }
                }
            }
        });
    }

    async fn push(&self, emitted_at: SystemTime, log: LogEvent) {
        let mut logs = self.inner.lock().await;
        logs.push_back(LogLine {
            emitted_at_unix_ms: system_time_millis(emitted_at),
            service_id: log.service_id.to_string(),
            origin: log_origin_name(log.origin).to_string(),
            stream: log_stream_name(log.stream).to_string(),
            message: log.message,
        });

        while logs.len() > self.retention {
            logs.pop_front();
        }
    }

    async fn query(
        &self,
        service_id: Option<&str>,
        stream: Option<LogStream>,
        limit: usize,
    ) -> Vec<LogLine> {
        let logs = self.inner.lock().await;
        let mut filtered = logs
            .iter()
            .rev()
            .filter(|line| service_id.map(|id| line.service_id == id).unwrap_or(true))
            .filter(|line| {
                stream
                    .map(|stream| line.stream == log_stream_name(stream))
                    .unwrap_or(true)
            })
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        filtered.reverse();
        filtered
    }

    async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }
}

fn service_status(snapshot: &AppState, service_id: &ServiceId) -> Option<ServiceStatus> {
    let definition = snapshot.services.get(service_id)?;
    let runtime = snapshot.runtime.get(service_id);
    Some(service_status_from_parts(definition, runtime))
}

fn service_status_from_parts(
    definition: &ServiceDefinition,
    runtime: Option<&ServiceRuntime>,
) -> ServiceStatus {
    ServiceStatus {
        id: definition.id.to_string(),
        name: definition.name.clone(),
        lifecycle: runtime
            .map(|runtime| lifecycle_name(runtime.lifecycle))
            .unwrap_or("discovered")
            .to_string(),
        health: runtime
            .map(|runtime| health_name(runtime.health))
            .unwrap_or("unknown")
            .to_string(),
        pid: runtime.and_then(|runtime| runtime.pid),
        started_at_unix_ms: runtime
            .and_then(|runtime| runtime.started_at)
            .map(system_time_millis),
        last_exit_code: runtime.and_then(|runtime| runtime.last_exit_code),
        last_error: runtime.and_then(|runtime| runtime.last_error.clone()),
        command: command_parts(&definition.command),
        working_dir: definition
            .command
            .working_dir
            .as_ref()
            .map(|path| path.display().to_string()),
        restart_policy: restart_policy_name(&definition.restart).to_string(),
        watch_enabled: definition.watch.enabled,
        watch_paths: definition
            .watch
            .paths
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        watch_include: definition.watch.include.clone(),
        watch_exclude: definition.watch.exclude.clone(),
        watch_ignore_paths: definition
            .watch
            .ignore_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        watch_ignore_regex: definition.watch.ignore_regex.clone(),
        depends_on: definition
            .depends_on
            .iter()
            .map(ToString::to_string)
            .collect(),
        dependencies: definition
            .dependency_contracts()
            .into_iter()
            .map(|dependency| DependencyStatus {
                service_id: dependency.service_id.to_string(),
                condition: dependency_condition_name(dependency.condition).to_string(),
                restart: dependency.restart,
                required: dependency.required,
                wait_timeout_ms: duration_millis(dependency.wait_timeout),
            })
            .collect(),
        readiness: definition
            .readiness
            .as_ref()
            .map(|readiness| ReadinessStatus {
                command: command_parts(&readiness.command),
                initial_delay_ms: duration_millis(readiness.initial_delay),
                interval_ms: duration_millis(readiness.interval),
                timeout_ms: duration_millis(readiness.timeout),
                retries: readiness.retries,
            }),
        healthcheck: definition
            .healthcheck
            .as_ref()
            .map(|healthcheck| HealthCheckStatus {
                url: healthcheck.http.url.clone(),
                method: healthcheck.http.method.clone(),
                expected_status: format!(
                    "{}..{}",
                    healthcheck.http.expected_status.start, healthcheck.http.expected_status.end
                ),
                initial_delay_ms: duration_millis(healthcheck.initial_delay),
                interval_ms: duration_millis(healthcheck.interval),
                timeout_ms: duration_millis(healthcheck.timeout),
                retries: healthcheck.retries,
            }),
        telemetry: runtime
            .and_then(|runtime| runtime.telemetry.latest.as_ref())
            .map(|snapshot| TelemetryStatus {
                collected_at_unix_ms: system_time_millis(snapshot.collected_at),
                cpu_millis: snapshot.cpu_millis,
                memory_bytes: snapshot.memory_bytes,
                uptime_ms: snapshot.uptime.map(duration_millis),
                open_ports: snapshot.open_ports.clone(),
                disk_read_bytes: snapshot.disk_read_bytes,
                disk_written_bytes: snapshot.disk_written_bytes,
                network_rx_bytes: snapshot.network_rx_bytes,
                network_tx_bytes: snapshot.network_tx_bytes,
            }),
    }
}

fn process_status(snapshot: &AppState, service_id: &ServiceId) -> Option<ProcessStatus> {
    let runtime = snapshot.runtime.get(service_id)?;
    let pid = runtime.pid?;
    let latest = runtime.telemetry.latest.as_ref();

    Some(ProcessStatus {
        service_id: service_id.to_string(),
        pid,
        lifecycle: lifecycle_name(runtime.lifecycle).to_string(),
        health: health_name(runtime.health).to_string(),
        cpu_millis: latest
            .map(|snapshot| snapshot.cpu_millis)
            .unwrap_or_default(),
        memory_bytes: latest
            .map(|snapshot| snapshot.memory_bytes)
            .unwrap_or_default(),
        uptime_ms: latest.and_then(|snapshot| snapshot.uptime.map(duration_millis)),
        open_ports: latest
            .map(|snapshot| snapshot.open_ports.clone())
            .unwrap_or_default(),
    })
}

fn runtime_summary(snapshot: &AppState) -> RuntimeSummary {
    let mut summary = RuntimeSummary {
        total_services: snapshot.services.len(),
        running_services: 0,
        failed_services: 0,
        aggregate_cpu_millis: 0,
        aggregate_memory_bytes: 0,
    };

    for runtime in snapshot.runtime.values() {
        if runtime.lifecycle == LifecycleState::Running {
            summary.running_services += 1;
        }
        if runtime.lifecycle == LifecycleState::Failed {
            summary.failed_services += 1;
        }
        if let Some(snapshot) = &runtime.telemetry.latest {
            summary.aggregate_cpu_millis += snapshot.cpu_millis;
            summary.aggregate_memory_bytes += snapshot.memory_bytes;
        }
    }

    summary
}

fn command_parts(command: &CommandSpec) -> Vec<String> {
    std::iter::once(command.program.clone())
        .chain(command.args.iter().cloned())
        .collect()
}

fn parse_service_id(value: String) -> Result<ServiceId, McpError> {
    if value.trim().is_empty() {
        return Err(McpError::invalid_params(
            "service_id must not be empty",
            None,
        ));
    }

    Ok(ServiceId::new(value))
}

fn parse_log_stream(value: &str) -> Result<LogStream, McpError> {
    match value {
        "stdout" => Ok(LogStream::Stdout),
        "stderr" => Ok(LogStream::Stderr),
        other => Err(McpError::invalid_params(
            format!("unsupported log stream `{other}`; expected stdout or stderr"),
            None,
        )),
    }
}

fn map_palo_error(error: palo_core::error::PaloError) -> McpError {
    warn!(error = %error, "MCP operation failed");
    McpError::internal_error(error.to_string(), None)
}

fn lifecycle_name(value: LifecycleState) -> &'static str {
    match value {
        LifecycleState::Discovered => "discovered",
        LifecycleState::Validated => "validated",
        LifecycleState::Checked => "checked",
        LifecycleState::Built => "built",
        LifecycleState::Starting => "starting",
        LifecycleState::Running => "running",
        LifecycleState::Stopped => "stopped",
        LifecycleState::Failed => "failed",
        LifecycleState::Restarting => "restarting",
    }
}

fn health_name(value: ServiceHealth) -> &'static str {
    match value {
        ServiceHealth::Unknown => "unknown",
        ServiceHealth::Healthy => "healthy",
        ServiceHealth::Degraded => "degraded",
        ServiceHealth::Unhealthy => "unhealthy",
    }
}

fn restart_policy_name(value: &RestartPolicy) -> &'static str {
    match value {
        RestartPolicy::Manual => "manual",
        RestartPolicy::Mcp => "mcp",
        RestartPolicy::Never => "never",
        RestartPolicy::OnChange => "on_change",
        RestartPolicy::OnCrash { .. } => "on_crash",
        RestartPolicy::Always { .. } => "always",
    }
}

fn dependency_condition_name(value: DependencyCondition) -> &'static str {
    match value {
        DependencyCondition::Started => "started",
        DependencyCondition::Running => "running",
        DependencyCondition::Ready => "ready",
    }
}

fn log_origin_name(value: LogOrigin) -> &'static str {
    match value {
        LogOrigin::App => "app",
        LogOrigin::PaloInternal => "palo_internal",
    }
}

fn log_stream_name(value: LogStream) -> &'static str {
    match value {
        LogStream::Stdout => "stdout",
        LogStream::Stderr => "stderr",
    }
}

fn system_time_millis(value: SystemTime) -> u64 {
    value
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn duration_millis(value: Duration) -> u64 {
    value.as_millis().try_into().unwrap_or(u64::MAX)
}

fn display_host(host: &str) -> String {
    match host {
        "0.0.0.0" => "127.0.0.1".to_string(),
        "::" => "[::1]".to_string(),
        value if value.contains(':') && !value.starts_with('[') => format!("[{value}]"),
        value => value.to_string(),
    }
}
