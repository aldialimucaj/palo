use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

use crate::domain::{
    AppState, HealthCheck, LifecycleState, ReadinessCheck, RestartPolicy, ServiceDefinition,
    ServiceDependency, ServiceHealth, ServiceId,
};
use crate::error::{
    BuildError, BuildStage, DiscoveryError, PaloError, ProcessError, ProcessOperation,
    UiCommandError,
};
use crate::events::{
    CommandKind, CommandOutcome, CommandRequest, CommandStatusEvent, CommandTarget, EventBus,
    EventPayload, OrchestrationErrorEvent, ServiceStateChanged, StateChangeReason, TelemetryUpdate,
};
use crate::execution::{CommandPipeline, PipelineStage, ProcessManager, ProcessResult};
use crate::telemetry::{ExitRecord, TelemetrySampler, TelemetrySnapshot};
use crate::watch::WatchRegistry;

const DEFAULT_EVENT_BUS_CAPACITY: usize = 512;
const DEFAULT_TELEMETRY_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct Orchestrator {
    events: EventBus,
    process_manager: ProcessManager,
    state: Arc<Mutex<AppState>>,
    supervisors: Arc<Mutex<BTreeMap<ServiceId, SupervisorState>>>,
    service_operations: Arc<Mutex<BTreeMap<ServiceId, Arc<Mutex<()>>>>>,
    watchers: WatchRegistry,
    telemetry_interval: Duration,
}

#[derive(Debug, Clone)]
struct SupervisorState {
    desired_running: bool,
    generation: u64,
    restart_attempts: u32,
    last_watch_restart_at: Option<Instant>,
}

impl Default for SupervisorState {
    fn default() -> Self {
        Self {
            desired_running: false,
            generation: 0,
            restart_attempts: 0,
            last_watch_restart_at: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DependencyWaitState {
    Missing,
    Observed {
        lifecycle: LifecycleState,
        health: ServiceHealth,
    },
    Ready,
    Failed,
}

impl Orchestrator {
    pub fn new(state: AppState) -> Self {
        Self::with_event_bus_and_telemetry_interval(
            state,
            EventBus::new(DEFAULT_EVENT_BUS_CAPACITY),
            DEFAULT_TELEMETRY_INTERVAL,
        )
    }

    pub fn with_event_bus(state: AppState, events: EventBus) -> Self {
        Self::with_event_bus_and_telemetry_interval(state, events, DEFAULT_TELEMETRY_INTERVAL)
    }

    pub fn with_event_bus_and_telemetry_interval(
        state: AppState,
        events: EventBus,
        telemetry_interval: Duration,
    ) -> Self {
        Self {
            process_manager: ProcessManager::new(events.clone()),
            events,
            state: Arc::new(Mutex::new(state)),
            supervisors: Arc::new(Mutex::new(BTreeMap::new())),
            service_operations: Arc::new(Mutex::new(BTreeMap::new())),
            watchers: WatchRegistry::default(),
            telemetry_interval,
        }
    }

    pub fn events(&self) -> EventBus {
        self.events.clone()
    }

    pub fn spawn_command_handler(&self) {
        let mut receiver = self.events.subscribe();
        let orchestrator = self.clone();
        tokio::spawn(async move {
            info!("starting command handler");
            loop {
                let Ok(event) = receiver.recv().await else {
                    break;
                };

                if let EventPayload::CommandRequested(request) = event.payload {
                    let orchestrator = orchestrator.clone();
                    tokio::spawn(async move {
                        orchestrator.handle_command_request(request).await;
                    });
                }
            }
            info!("command handler stopped");
        });
    }

    pub async fn snapshot_state(&self) -> AppState {
        self.state.lock().await.clone()
    }

    pub async fn start_all(&self) -> Result<(), PaloError> {
        let order = {
            let state = self.state.lock().await;
            topological_start_order(&state, state.services.keys().cloned().collect())?
        };

        info!(service_count = order.len(), "starting all services");
        for service_id in order {
            self.start_single_service(&service_id, None).await?;
        }

        Ok(())
    }

    pub async fn start_service(&self, service_id: &ServiceId) -> Result<(), PaloError> {
        let order = {
            let state = self.state.lock().await;
            topological_start_order(&state, vec![service_id.clone()])?
        };

        info!(service_id = %service_id, ordered_count = order.len(), "starting service with dependencies");
        for ordered_service_id in order {
            self.start_single_service(&ordered_service_id, None).await?;
        }

        Ok(())
    }

    pub async fn stop_all(&self) -> Vec<Result<ProcessResult, PaloError>> {
        let order = {
            let state = self.state.lock().await;
            topological_shutdown_order(&state, state.services.keys().cloned().collect())
                .unwrap_or_default()
        };

        info!(service_count = order.len(), "stopping all services");
        let mut results = Vec::with_capacity(order.len());
        for service_id in order {
            match self.stop_single_service(&service_id, None).await {
                Ok(Some(result)) => results.push(Ok(result)),
                Ok(None) => {}
                Err(error) => results.push(Err(error)),
            }
        }

        results
    }

    pub async fn stop_service(
        &self,
        service_id: &ServiceId,
    ) -> Result<Option<ProcessResult>, PaloError> {
        let order = {
            let state = self.state.lock().await;
            topological_shutdown_order(&state, vec![service_id.clone()])?
        };

        info!(service_id = %service_id, ordered_count = order.len(), "stopping service with dependents");
        let mut last = None;
        for ordered_service_id in order {
            last = self.stop_single_service(&ordered_service_id, None).await?;
        }

        Ok(last)
    }

    pub async fn restart_service(&self, service_id: &ServiceId) -> Result<(), PaloError> {
        info!(service_id = %service_id, "manually restarting service");
        self.restart_dependency_graph(
            service_id,
            StateChangeReason::Command(CommandKind::Restart),
            Duration::ZERO,
        )
        .await
    }

    async fn restart_dependency_graph(
        &self,
        service_id: &ServiceId,
        reason: StateChangeReason,
        backoff: Duration,
    ) -> Result<(), PaloError> {
        let restart_scope = {
            let state = self.state.lock().await;
            restart_propagation_scope(&state, service_id)?
        };
        let active_services = {
            let state = self.state.lock().await;
            state
                .runtime
                .iter()
                .filter_map(|(candidate, runtime)| {
                    runtime.lifecycle.is_active().then_some(candidate.clone())
                })
                .collect::<BTreeSet<_>>()
        };
        let desired_services = {
            let supervisors = self.supervisors.lock().await;
            supervisors
                .iter()
                .filter_map(|(candidate, supervisor)| {
                    supervisor.desired_running.then_some(candidate.clone())
                })
                .collect::<BTreeSet<_>>()
        };
        let restart_targets = restart_scope
            .into_iter()
            .filter(|candidate| {
                candidate == service_id
                    || active_services.contains(candidate)
                    || desired_services.contains(candidate)
            })
            .collect::<BTreeSet<_>>();
        let stop_order = {
            let state = self.state.lock().await;
            restricted_shutdown_order(&state, vec![service_id.clone()], &restart_targets)?
        };
        let active_processes = self
            .process_manager
            .active_services()
            .await
            .into_iter()
            .collect::<BTreeSet<_>>();
        let stop_order = stop_order
            .into_iter()
            .filter(|ordered_service_id| {
                ordered_service_id != service_id || active_processes.contains(ordered_service_id)
            })
            .collect::<Vec<_>>();

        info!(
            service_id = %service_id,
            restart_target_count = restart_targets.len(),
            backoff_ms = backoff.as_millis(),
            reason = ?reason,
            "restarting service dependency graph",
        );

        for ordered_service_id in stop_order {
            self.stop_single_service(&ordered_service_id, Some(reason.clone()))
                .await?;
        }

        if !backoff.is_zero() {
            sleep(backoff).await;
        }

        let start_order = {
            let state = self.state.lock().await;
            topological_start_order(&state, restart_targets.iter().cloned().collect())?
        };

        for ordered_service_id in start_order {
            let restart_reason = restart_targets
                .contains(&ordered_service_id)
                .then(|| reason.clone());
            self.start_single_service(&ordered_service_id, restart_reason)
                .await?;
        }

        Ok(())
    }

    pub async fn check_all(&self) -> Result<usize, PaloError> {
        let order = {
            let state = self.state.lock().await;
            topological_start_order(&state, state.services.keys().cloned().collect())?
        };

        info!(service_count = order.len(), "checking all services");
        let mut commands_run = 0;
        for service_id in order {
            commands_run += self.check_service(&service_id).await?;
        }

        Ok(commands_run)
    }

    pub async fn check_service(&self, service_id: &ServiceId) -> Result<usize, PaloError> {
        info!(service_id = %service_id, "checking service");
        let lock = self.service_operation_lock(service_id).await;
        let _guard = lock.lock().await;

        let service = self.service(service_id).await?;
        self.mark_validated(service_id).await;
        self.run_check_pipeline(&service).await
    }

    pub async fn build_all(&self) -> Result<usize, PaloError> {
        let order = {
            let state = self.state.lock().await;
            topological_start_order(&state, state.services.keys().cloned().collect())?
        };

        info!(service_count = order.len(), "building all services");
        let mut commands_run = 0;
        for service_id in order {
            commands_run += self.build_service(&service_id).await?;
        }

        Ok(commands_run)
    }

    pub async fn build_service(&self, service_id: &ServiceId) -> Result<usize, PaloError> {
        info!(service_id = %service_id, "building service");
        let lock = self.service_operation_lock(service_id).await;
        let _guard = lock.lock().await;

        let service = self.service(service_id).await?;
        self.mark_validated(service_id).await;
        self.run_build_pipeline(&service).await
    }

    pub async fn trigger_watch_restart(
        &self,
        service_id: &ServiceId,
        path: Option<String>,
    ) -> Result<bool, PaloError> {
        let service = self.service(service_id).await?;
        if !matches!(service.restart, RestartPolicy::OnChange) {
            return Ok(false);
        }

        let debounce = service.watch.debounce;
        {
            let mut supervisors = self.supervisors.lock().await;
            let supervisor = supervisors.entry(service_id.clone()).or_default();
            if let Some(last) = supervisor.last_watch_restart_at {
                if last.elapsed() < debounce {
                    debug!(service_id = %service_id, "dropping watch-triggered restart inside debounce window");
                    return Ok(false);
                }
            }
            supervisor.last_watch_restart_at = Some(Instant::now());
        }

        info!(service_id = %service_id, ?path, "triggering watch restart");
        self.restart_dependency_graph(
            service_id,
            StateChangeReason::WatchTriggered { path },
            Duration::ZERO,
        )
        .await?;

        Ok(true)
    }

    async fn start_single_service(
        &self,
        service_id: &ServiceId,
        restart_reason: Option<StateChangeReason>,
    ) -> Result<(), PaloError> {
        let lock = self.service_operation_lock(service_id).await;
        let _guard = lock.lock().await;

        self.start_single_service_unlocked(service_id, restart_reason)
            .await
    }

    async fn start_single_service_unlocked(
        &self,
        service_id: &ServiceId,
        restart_reason: Option<StateChangeReason>,
    ) -> Result<(), PaloError> {
        let service = self.service(service_id).await?;
        info!(
            service_id = %service_id,
            restart_reason = ?restart_reason,
            "starting service supervision cycle",
        );
        self.ensure_dependencies_running(&service).await?;

        {
            let mut supervisors = self.supervisors.lock().await;
            let supervisor = supervisors.entry(service_id.clone()).or_default();
            if supervisor.desired_running && restart_reason.is_none() {
                debug!(service_id = %service_id, "service already marked for running; skipping start");
                return Ok(());
            }

            supervisor.desired_running = true;
            supervisor.generation += 1;
        }

        let generation = {
            let supervisors = self.supervisors.lock().await;
            supervisors
                .get(service_id)
                .map(|value| value.generation)
                .unwrap_or_default()
        };

        if restart_reason.is_some() {
            self.transition_state(
                service_id,
                LifecycleState::Restarting,
                restart_reason.clone(),
            )
            .await;
        }

        self.mark_validated(service_id).await;
        let run_pid = match self
            .run_startup_pipeline(&service, restart_reason.clone())
            .await
        {
            Ok(run_pid) => run_pid,
            Err(error) => {
                let mut supervisors = self.supervisors.lock().await;
                if let Some(supervisor) = supervisors.get_mut(service_id) {
                    supervisor.desired_running = false;
                }
                return Err(error);
            }
        };

        if let Err(error) = self.watchers.register(&service, self.clone()).await {
            self.fail_service(
                service_id,
                format!("failed to register file watcher: {error}"),
            )
            .await;
            let _ = self
                .stop_single_service(service_id, restart_reason.clone())
                .await;
            return Err(error);
        }

        self.spawn_observer(service_id.clone(), generation);
        if let Some(pid) = run_pid {
            self.spawn_telemetry_sampler(service_id.clone(), generation, pid);
        }
        if let Some(healthcheck) = service.healthcheck.clone() {
            self.spawn_health_monitor(service_id.clone(), generation, healthcheck);
        }

        Ok(())
    }

    async fn stop_single_service(
        &self,
        service_id: &ServiceId,
        reason: Option<StateChangeReason>,
    ) -> Result<Option<ProcessResult>, PaloError> {
        let lock = self.service_operation_lock(service_id).await;
        let _guard = lock.lock().await;

        self.stop_single_service_unlocked(service_id, reason).await
    }

    async fn stop_single_service_unlocked(
        &self,
        service_id: &ServiceId,
        reason: Option<StateChangeReason>,
    ) -> Result<Option<ProcessResult>, PaloError> {
        let service = self.service(service_id).await?;
        info!(
            service_id = %service_id,
            reason = ?reason,
            "stopping service supervision cycle",
        );
        let is_active = self
            .process_manager
            .active_services()
            .await
            .into_iter()
            .any(|active_id| active_id == *service_id);

        self.watchers.unregister(service_id).await;

        {
            let mut supervisors = self.supervisors.lock().await;
            let supervisor = supervisors.entry(service_id.clone()).or_default();
            supervisor.desired_running = false;
            supervisor.restart_attempts = 0;
        }

        if is_active {
            for command in CommandPipeline::shutdown(&service).commands {
                let result = self
                    .process_manager
                    .run_pipeline_command(service_id, &command)
                    .await?;
                if !result.success && command.required {
                    return Err(command_failure_error(
                        service_id,
                        &command.stage,
                        result.exit_code,
                    ));
                }
            }
        }

        let stopped = self.process_manager.stop_service(service_id).await?;
        if stopped.is_some() {
            if let Some(result) = &stopped {
                self.record_process_result(service_id, result).await;
            }
            self.transition_state(service_id, LifecycleState::Stopped, reason)
                .await;
            self.run_post_stop_hooks(&service).await?;
        }

        Ok(stopped)
    }

    async fn run_startup_pipeline(
        &self,
        service: &ServiceDefinition,
        restart_reason: Option<StateChangeReason>,
    ) -> Result<Option<u32>, PaloError> {
        let pipeline = CommandPipeline::startup(service);
        debug!(
            service_id = %service.id,
            command_count = pipeline.commands.len(),
            restart_reason = ?restart_reason,
            "prepared startup pipeline",
        );
        let has_build_step = pipeline.commands.iter().any(|command| {
            matches!(
                command.stage,
                PipelineStage::Build | PipelineStage::Hook { .. }
            )
        });

        let mut run_pid = None;
        for command in pipeline.commands {
            match command.stage {
                PipelineStage::Check => {
                    let result = self
                        .process_manager
                        .run_pipeline_command(&service.id, &command)
                        .await?;
                    if !result.success && command.required {
                        self.fail_service(
                            &service.id,
                            format!("check command exited with {:?}", result.exit_code),
                        )
                        .await;
                        return Err(command_failure_error(
                            &service.id,
                            &command.stage,
                            result.exit_code,
                        ));
                    }
                    self.transition_state(&service.id, LifecycleState::Checked, None)
                        .await;
                }
                PipelineStage::Build => {
                    let result = self
                        .process_manager
                        .run_pipeline_command(&service.id, &command)
                        .await?;
                    if !result.success && command.required {
                        self.fail_service(
                            &service.id,
                            format!("build command exited with {:?}", result.exit_code),
                        )
                        .await;
                        return Err(command_failure_error(
                            &service.id,
                            &command.stage,
                            result.exit_code,
                        ));
                    }
                    self.transition_state(
                        &service.id,
                        LifecycleState::Built,
                        Some(StateChangeReason::BuildCompleted),
                    )
                    .await;
                }
                PipelineStage::Run => {
                    self.transition_state(
                        &service.id,
                        LifecycleState::Starting,
                        restart_reason.clone(),
                    )
                    .await;

                    let result = self.process_manager.spawn_service(service).await?;
                    run_pid = result.pid;
                }
                PipelineStage::Hook { .. } => {
                    let result = self
                        .process_manager
                        .run_pipeline_command(&service.id, &command)
                        .await?;
                    if !result.success && command.required {
                        self.fail_service(
                            &service.id,
                            format!("hook command exited with {:?}", result.exit_code),
                        )
                        .await;
                        return Err(command_failure_error(
                            &service.id,
                            &command.stage,
                            result.exit_code,
                        ));
                    }
                }
                PipelineStage::Readiness => {}
            }
        }

        if has_build_step && service.build.build.is_none() {
            self.transition_state(
                &service.id,
                LifecycleState::Built,
                Some(StateChangeReason::BuildCompleted),
            )
            .await;
        }

        let readiness_result = if let Some(healthcheck) = &service.healthcheck {
            self.wait_for_service_healthcheck(service, healthcheck)
                .await
        } else if let Some(readiness) = &service.readiness {
            self.wait_for_service_readiness(service, readiness).await
        } else {
            Ok(())
        };

        if let Err(error) = readiness_result {
            self.fail_service(&service.id, error.to_string()).await;
            if run_pid.is_some() {
                match self.process_manager.stop_service(&service.id).await {
                    Ok(Some(result)) => self.record_process_result(&service.id, &result).await,
                    Ok(None) => {}
                    Err(stop_error) => self.publish_error(&stop_error),
                }
            }
            return Err(error);
        }

        self.mark_running(&service.id, run_pid).await;
        Ok(run_pid)
    }

    async fn wait_for_service_readiness(
        &self,
        service: &ServiceDefinition,
        readiness: &ReadinessCheck,
    ) -> Result<(), PaloError> {
        if !readiness.initial_delay.is_zero() {
            debug!(
                service_id = %service.id,
                initial_delay_ms = readiness.initial_delay.as_millis(),
                "waiting before first readiness check",
            );
            sleep(readiness.initial_delay).await;
        }

        for attempt in 1..=readiness.retries {
            info!(
                service_id = %service.id,
                attempt,
                max_attempts = readiness.retries,
                timeout_ms = readiness.timeout.as_millis(),
                "running service readiness check",
            );

            let result = self
                .process_manager
                .run_readiness_command(&service.id, &readiness.command, readiness.timeout)
                .await?;

            if result.success {
                info!(
                    service_id = %service.id,
                    attempt,
                    "service readiness check succeeded",
                );
                return Ok(());
            }

            warn!(
                service_id = %service.id,
                attempt,
                max_attempts = readiness.retries,
                exit_code = result.exit_code,
                "service readiness check failed",
            );

            if attempt < readiness.retries {
                sleep(readiness.interval).await;
            }
        }

        Err(PaloError::Process(ProcessError::new(
            service.id.clone(),
            ProcessOperation::Readiness,
            format!(
                "readiness check did not succeed after {} attempt(s)",
                readiness.retries
            ),
        )))
    }

    async fn wait_for_service_healthcheck(
        &self,
        service: &ServiceDefinition,
        healthcheck: &HealthCheck,
    ) -> Result<(), PaloError> {
        info!(
            service_id = %service.id,
            url = %healthcheck.http.url,
            method = %healthcheck.http.method,
            "starting HTTP health check",
        );

        if !healthcheck.initial_delay.is_zero() {
            debug!(
                service_id = %service.id,
                initial_delay_ms = healthcheck.initial_delay.as_millis(),
                "waiting before first HTTP health check",
            );
            sleep(healthcheck.initial_delay).await;
        }

        let client = reqwest::Client::new();
        for attempt in 1..=healthcheck.retries {
            debug!(
                service_id = %service.id,
                attempt,
                max_attempts = healthcheck.retries,
                timeout_ms = healthcheck.timeout.as_millis(),
                "running HTTP health check",
            );

            match run_http_health_probe(&client, healthcheck).await {
                Ok(status) => {
                    info!(
                        service_id = %service.id,
                        attempt,
                        status,
                        "HTTP health check succeeded",
                    );
                    self.update_service_health(&service.id, ServiceHealth::Healthy, None)
                        .await;
                    return Ok(());
                }
                Err(message) => {
                    let health = if attempt >= healthcheck.retries {
                        ServiceHealth::Unhealthy
                    } else {
                        ServiceHealth::Degraded
                    };
                    warn!(
                        service_id = %service.id,
                        attempt,
                        max_attempts = healthcheck.retries,
                        error = %message,
                        health = ?health,
                        "HTTP health check failed",
                    );
                    self.update_service_health(&service.id, health, None).await;
                    if health == ServiceHealth::Unhealthy {
                        warn!(
                            service_id = %service.id,
                            attempts = healthcheck.retries,
                            "HTTP health check reached unhealthy threshold",
                        );
                    }
                }
            }

            if attempt < healthcheck.retries {
                sleep(healthcheck.interval).await;
            }
        }

        Err(PaloError::Process(ProcessError::new(
            service.id.clone(),
            ProcessOperation::Readiness,
            format!(
                "HTTP health check did not succeed after {} attempt(s)",
                healthcheck.retries
            ),
        )))
    }

    fn spawn_health_monitor(
        &self,
        service_id: ServiceId,
        generation: u64,
        healthcheck: HealthCheck,
    ) {
        let orchestrator = self.clone();
        tokio::spawn(async move {
            orchestrator
                .monitor_service_health(service_id, generation, healthcheck)
                .await;
        });
    }

    async fn monitor_service_health(
        &self,
        service_id: ServiceId,
        generation: u64,
        healthcheck: HealthCheck,
    ) {
        info!(
            service_id = %service_id,
            generation,
            url = %healthcheck.http.url,
            method = %healthcheck.http.method,
            "starting HTTP health monitor",
        );

        if !healthcheck.initial_delay.is_zero() {
            sleep(healthcheck.initial_delay).await;
        }

        let client = reqwest::Client::new();
        let mut consecutive_failures = 0;

        loop {
            if !self
                .should_continue_health_monitor(&service_id, generation)
                .await
            {
                info!(
                    service_id = %service_id,
                    generation,
                    "HTTP health monitor stopped",
                );
                return;
            }

            match run_http_health_probe(&client, &healthcheck).await {
                Ok(status) => {
                    consecutive_failures = 0;
                    info!(
                        service_id = %service_id,
                        generation,
                        status,
                        "HTTP health check succeeded",
                    );
                    self.update_service_health(&service_id, ServiceHealth::Healthy, None)
                        .await;
                }
                Err(message) => {
                    consecutive_failures += 1;
                    let health = if consecutive_failures >= healthcheck.retries {
                        ServiceHealth::Unhealthy
                    } else {
                        ServiceHealth::Degraded
                    };
                    warn!(
                        service_id = %service_id,
                        generation,
                        attempt = consecutive_failures,
                        max_attempts = healthcheck.retries,
                        error = %message,
                        health = ?health,
                        "HTTP health check failed",
                    );
                    self.update_service_health(&service_id, health, None).await;
                    if health == ServiceHealth::Unhealthy {
                        warn!(
                            service_id = %service_id,
                            generation,
                            failures = consecutive_failures,
                            "HTTP health check reached unhealthy threshold",
                        );
                    }
                }
            }

            sleep(healthcheck.interval).await;
        }
    }

    async fn should_continue_health_monitor(
        &self,
        service_id: &ServiceId,
        generation: u64,
    ) -> bool {
        let supervisor_running = {
            let supervisors = self.supervisors.lock().await;
            let supervisor = supervisors.get(service_id).cloned().unwrap_or_default();
            supervisor.desired_running && supervisor.generation == generation
        };

        if !supervisor_running {
            return false;
        }

        let state = self.state.lock().await;
        state
            .runtime
            .get(service_id)
            .map(|runtime| runtime.lifecycle.is_active())
            .unwrap_or(false)
    }

    async fn run_check_pipeline(&self, service: &ServiceDefinition) -> Result<usize, PaloError> {
        let pipeline = CommandPipeline::check(service);
        debug!(
            service_id = %service.id,
            command_count = pipeline.commands.len(),
            "prepared check pipeline",
        );

        let mut commands_run = 0;
        for command in pipeline.commands {
            let result = self
                .process_manager
                .run_pipeline_command(&service.id, &command)
                .await?;
            commands_run += 1;

            if !result.success && command.required {
                return Err(command_failure_error(
                    &service.id,
                    &command.stage,
                    result.exit_code,
                ));
            }

            self.transition_state(&service.id, LifecycleState::Checked, None)
                .await;
        }

        Ok(commands_run)
    }

    async fn run_build_pipeline(&self, service: &ServiceDefinition) -> Result<usize, PaloError> {
        let pipeline = CommandPipeline::build(service);
        debug!(
            service_id = %service.id,
            command_count = pipeline.commands.len(),
            "prepared build pipeline",
        );

        let has_commands = !pipeline.commands.is_empty();
        let mut commands_run = 0;
        for command in pipeline.commands {
            let result = self
                .process_manager
                .run_pipeline_command(&service.id, &command)
                .await?;
            commands_run += 1;

            if !result.success && command.required {
                return Err(command_failure_error(
                    &service.id,
                    &command.stage,
                    result.exit_code,
                ));
            }

            match command.stage {
                PipelineStage::Check => {
                    self.transition_state(&service.id, LifecycleState::Checked, None)
                        .await;
                }
                PipelineStage::Build => {
                    self.transition_state(
                        &service.id,
                        LifecycleState::Built,
                        Some(StateChangeReason::BuildCompleted),
                    )
                    .await;
                }
                PipelineStage::Hook { .. } | PipelineStage::Run | PipelineStage::Readiness => {}
            }
        }

        if has_commands {
            self.transition_state(
                &service.id,
                LifecycleState::Built,
                Some(StateChangeReason::BuildCompleted),
            )
            .await;
        }

        Ok(commands_run)
    }

    async fn handle_command_request(&self, request: CommandRequest) {
        info!(target = %describe_command_target(&request.target), command = %command_name(request.command), "received command request");
        self.publish_command_status(
            request.clone(),
            CommandOutcome::Accepted,
            format!(
                "accepted {} for {}",
                command_name(request.command),
                describe_command_target(&request.target)
            ),
        );

        let result = match &request.target {
            CommandTarget::Service(service_id) => {
                self.handle_service_command(service_id, request.command)
                    .await
            }
            CommandTarget::AllServices => self.handle_global_command(request.command).await,
        };

        match result {
            Ok(message) => {
                info!(target = %describe_command_target(&request.target), command = %command_name(request.command), "command completed");
                self.publish_command_status(request, CommandOutcome::Completed, message);
            }
            Err(error) => {
                let outcome = if matches!(error, PaloError::UiCommand(_)) {
                    CommandOutcome::Rejected
                } else {
                    CommandOutcome::Failed
                };
                warn!(target = %describe_command_target(&request.target), command = %command_name(request.command), error = %error, "command did not complete successfully");
                self.publish_command_status(request.clone(), outcome, error.to_string());
                self.publish_error(&error);
            }
        }
    }

    async fn handle_service_command(
        &self,
        service_id: &ServiceId,
        command: CommandKind,
    ) -> Result<String, PaloError> {
        self.validate_service_command(service_id, command).await?;

        match command {
            CommandKind::Start => {
                self.start_service(service_id).await?;
                Ok(format!("started service `{service_id}`"))
            }
            CommandKind::Stop => {
                self.stop_service(service_id).await?;
                Ok(format!("stopped service `{service_id}`"))
            }
            CommandKind::Restart => {
                self.restart_service(service_id).await?;
                Ok(format!("restarted service `{service_id}`"))
            }
            CommandKind::Check => {
                let commands_run = self.check_service(service_id).await?;
                Ok(format!(
                    "checked service `{service_id}` with {commands_run} command(s)"
                ))
            }
            CommandKind::Build => {
                let commands_run = self.build_service(service_id).await?;
                Ok(format!(
                    "built service `{service_id}` with {commands_run} command(s)"
                ))
            }
            CommandKind::Quit => Err(PaloError::UiCommand(UiCommandError::new(
                command,
                "quit is only supported as a global command",
            ))),
            unsupported => Err(PaloError::UiCommand(
                UiCommandError::new(
                    unsupported,
                    "command is not supported by the runtime orchestrator",
                )
                .for_service(service_id.clone()),
            )),
        }
    }

    async fn handle_global_command(&self, command: CommandKind) -> Result<String, PaloError> {
        match command {
            CommandKind::Start => {
                self.start_all().await?;
                Ok("started all services".to_string())
            }
            CommandKind::Stop => {
                let results = self.stop_all().await;
                propagate_stop_errors(command, results)?;
                Ok("stopped all services".to_string())
            }
            CommandKind::Restart => {
                let stop_results = self.stop_all().await;
                propagate_stop_errors(command, stop_results)?;
                self.start_all().await?;
                Ok("restarted all services".to_string())
            }
            CommandKind::Check => {
                let commands_run = self.check_all().await?;
                Ok(format!(
                    "checked all services with {commands_run} command(s)"
                ))
            }
            CommandKind::Build => {
                let commands_run = self.build_all().await?;
                Ok(format!("built all services with {commands_run} command(s)"))
            }
            CommandKind::Quit => {
                let results = self.stop_all().await;
                propagate_stop_errors(command, results)?;
                Ok("stopped all services and exited".to_string())
            }
            unsupported => Err(PaloError::UiCommand(UiCommandError::new(
                unsupported,
                "command is not supported by the runtime orchestrator",
            ))),
        }
    }

    async fn validate_service_command(
        &self,
        service_id: &ServiceId,
        command: CommandKind,
    ) -> Result<(), PaloError> {
        let lifecycle = {
            let state = self.state.lock().await;
            if !state.services.contains_key(service_id) {
                return Err(PaloError::UiCommand(
                    UiCommandError::new(command, "service is not defined")
                        .for_service(service_id.clone()),
                ));
            }

            state
                .runtime
                .get(service_id)
                .map(|runtime| runtime.lifecycle)
        }
        .unwrap_or(LifecycleState::Discovered);

        match command {
            CommandKind::Start => {
                if matches!(
                    lifecycle,
                    LifecycleState::Validated
                        | LifecycleState::Checked
                        | LifecycleState::Built
                        | LifecycleState::Starting
                        | LifecycleState::Restarting
                ) {
                    return Err(PaloError::UiCommand(
                        UiCommandError::new(
                            command,
                            "service is transitioning and cannot be started",
                        )
                        .for_service(service_id.clone()),
                    ));
                }
            }
            CommandKind::Stop => {
                if matches!(
                    lifecycle,
                    LifecycleState::Validated
                        | LifecycleState::Checked
                        | LifecycleState::Built
                        | LifecycleState::Starting
                        | LifecycleState::Restarting
                ) {
                    return Err(PaloError::UiCommand(
                        UiCommandError::new(
                            command,
                            "service is transitioning and cannot be stopped",
                        )
                        .for_service(service_id.clone()),
                    ));
                }
            }
            CommandKind::Restart => {
                if matches!(
                    lifecycle,
                    LifecycleState::Validated
                        | LifecycleState::Checked
                        | LifecycleState::Built
                        | LifecycleState::Starting
                        | LifecycleState::Restarting
                ) {
                    return Err(PaloError::UiCommand(
                        UiCommandError::new(
                            command,
                            "service is transitioning and cannot be restarted",
                        )
                        .for_service(service_id.clone()),
                    ));
                }
            }
            _ => {}
        }

        Ok(())
    }

    fn spawn_observer(&self, service_id: ServiceId, generation: u64) {
        let orchestrator = self.clone();
        tokio::spawn(async move {
            if let Err(error) = orchestrator.observe_service(service_id, generation).await {
                orchestrator.publish_error(&error);
            }
        });
    }

    fn spawn_telemetry_sampler(&self, service_id: ServiceId, generation: u64, pid: u32) {
        let orchestrator = self.clone();
        tokio::spawn(async move {
            orchestrator
                .collect_service_telemetry(service_id, generation, pid)
                .await;
        });
    }

    async fn observe_service(
        &self,
        service_id: ServiceId,
        generation: u64,
    ) -> Result<(), PaloError> {
        let Some(result) = self.process_manager.wait_for_service(&service_id).await? else {
            return Ok(());
        };

        let (desired_running, current_generation, restart_attempts) = {
            let supervisors = self.supervisors.lock().await;
            let supervisor = supervisors.get(&service_id).cloned().unwrap_or_default();
            (
                supervisor.desired_running,
                supervisor.generation,
                supervisor.restart_attempts,
            )
        };
        let policy = self.service(&service_id).await?.restart;

        if current_generation != generation {
            debug!(service_id = %service_id, generation, current_generation, "ignoring stale process observation");
            return Ok(());
        }

        if !desired_running {
            debug!(service_id = %service_id, "service was intentionally stopped");
            return Ok(());
        }

        let restart_decision = RestartDecision::from_result(&policy, &result, restart_attempts);
        info!(
            service_id = %service_id,
            exit_code = result.exit_code,
            success = result.success,
            restart_attempts,
            restart_policy = ?policy,
            restart_decision = ?restart_decision,
            "observed service process exit",
        );
        match restart_decision {
            RestartDecision::NoRestart => {
                self.record_process_result(&service_id, &result).await;
                if result.success {
                    self.transition_state(
                        &service_id,
                        LifecycleState::Stopped,
                        Some(StateChangeReason::ProcessExited {
                            exit_code: result.exit_code,
                        }),
                    )
                    .await;
                } else {
                    self.fail_service(
                        &service_id,
                        format!("service process exited with {:?}", result.exit_code),
                    )
                    .await;
                }

                let service = self.service(&service_id).await?;
                let post_stop_result = self.run_post_stop_hooks(&service).await;

                let mut supervisors = self.supervisors.lock().await;
                if let Some(supervisor) = supervisors.get_mut(&service_id) {
                    supervisor.desired_running = false;
                    supervisor.restart_attempts = 0;
                }
                post_stop_result?;
            }
            RestartDecision::Restart { backoff } => {
                let reason = StateChangeReason::ProcessExited {
                    exit_code: result.exit_code,
                };
                let service = self.service(&service_id).await?;
                if let Err(error) = self.run_post_stop_hooks(&service).await {
                    self.fail_service(&service_id, error.to_string()).await;
                    let mut supervisors = self.supervisors.lock().await;
                    if let Some(supervisor) = supervisors.get_mut(&service_id) {
                        supervisor.desired_running = false;
                        supervisor.restart_attempts = 0;
                    }
                    return Err(error);
                }

                self.transition_state(
                    &service_id,
                    LifecycleState::Restarting,
                    Some(reason.clone()),
                )
                .await;

                {
                    let mut supervisors = self.supervisors.lock().await;
                    if let Some(supervisor) = supervisors.get_mut(&service_id) {
                        supervisor.restart_attempts += 1;
                    }
                }

                if !self
                    .should_restart_after_backoff(&service_id, generation)
                    .await
                {
                    debug!(service_id = %service_id, generation, "skipping process-exit restart because service is no longer desired");
                    return Ok(());
                }

                info!(service_id = %service_id, ?backoff, "restarting service after process exit");
                self.restart_dependency_graph(&service_id, reason, backoff)
                    .await?;
            }
        }

        Ok(())
    }

    async fn run_post_stop_hooks(&self, service: &ServiceDefinition) -> Result<(), PaloError> {
        let pipeline = CommandPipeline::post_shutdown(service);
        if pipeline.commands.is_empty() {
            return Ok(());
        }

        info!(
            service_id = %service.id,
            command_count = pipeline.commands.len(),
            "running service post-stop hooks",
        );

        for command in pipeline.commands {
            let result = self
                .process_manager
                .run_pipeline_command(&service.id, &command)
                .await?;

            if !result.success && command.required {
                return Err(command_failure_error(
                    &service.id,
                    &command.stage,
                    result.exit_code,
                ));
            }
        }

        Ok(())
    }

    async fn service_operation_lock(&self, service_id: &ServiceId) -> Arc<Mutex<()>> {
        let mut service_operations = self.service_operations.lock().await;
        service_operations
            .entry(service_id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn should_restart_after_backoff(&self, service_id: &ServiceId, generation: u64) -> bool {
        let supervisors = self.supervisors.lock().await;
        let supervisor = supervisors.get(service_id).cloned().unwrap_or_default();
        supervisor.desired_running && supervisor.generation == generation
    }

    async fn ensure_dependencies_running(
        &self,
        service: &ServiceDefinition,
    ) -> Result<(), PaloError> {
        for dependency in service.dependency_contracts() {
            self.wait_for_dependency_condition(service, &dependency)
                .await?;
        }

        Ok(())
    }

    async fn wait_for_dependency_condition(
        &self,
        service: &ServiceDefinition,
        dependency: &ServiceDependency,
    ) -> Result<(), PaloError> {
        info!(
            service_id = %service.id,
            dependency_id = %dependency.service_id,
            condition = ?dependency.condition,
            required = dependency.required,
            wait_timeout_ms = dependency.wait_timeout.as_millis(),
            "waiting for service dependency",
        );

        let wait_result = timeout(dependency.wait_timeout, async {
            loop {
                let dependency_state = {
                    let state = self.state.lock().await;
                    let Some(runtime) = state.runtime.get(&dependency.service_id) else {
                        return DependencyWaitState::Missing;
                    };
                    DependencyWaitState::Observed {
                        lifecycle: runtime.lifecycle,
                        health: runtime.health,
                    }
                };

                match dependency_state {
                    DependencyWaitState::Missing => return DependencyWaitState::Missing,
                    DependencyWaitState::Observed { lifecycle, health }
                        if dependency.condition.is_satisfied_by(lifecycle, health) =>
                    {
                        return DependencyWaitState::Ready;
                    }
                    DependencyWaitState::Observed { lifecycle, .. }
                        if lifecycle == LifecycleState::Failed =>
                    {
                        return DependencyWaitState::Failed;
                    }
                    DependencyWaitState::Observed { .. } => {
                        sleep(Duration::from_millis(25)).await;
                    }
                    DependencyWaitState::Ready | DependencyWaitState::Failed => unreachable!(),
                }
            }
        })
        .await;

        match wait_result {
            Ok(DependencyWaitState::Ready) => {
                debug!(
                    service_id = %service.id,
                    dependency_id = %dependency.service_id,
                    condition = ?dependency.condition,
                    "service dependency condition satisfied",
                );
                Ok(())
            }
            Ok(DependencyWaitState::Missing) if !dependency.required => {
                warn!(
                    service_id = %service.id,
                    dependency_id = %dependency.service_id,
                    "optional service dependency is not defined",
                );
                Ok(())
            }
            Ok(DependencyWaitState::Missing) => {
                let error = PaloError::Discovery(DiscoveryError::new(format!(
                    "service `{}` depends on unknown service `{}`",
                    service.id, dependency.service_id
                )));
                self.publish_error(&error);
                Err(error)
            }
            Ok(DependencyWaitState::Failed) => {
                let error = PaloError::Discovery(DiscoveryError::new(format!(
                    "service `{}` is blocked because dependency `{}` failed",
                    service.id, dependency.service_id
                )));
                self.publish_error(&error);
                self.transition_state(
                    &service.id,
                    LifecycleState::Failed,
                    Some(StateChangeReason::DependencyFailed),
                )
                .await;
                Err(error)
            }
            Ok(DependencyWaitState::Observed { .. }) => unreachable!(),
            Err(_) => {
                let error = PaloError::Discovery(DiscoveryError::new(format!(
                    "service `{}` timed out waiting for dependency `{}` to satisfy {:?}",
                    service.id, dependency.service_id, dependency.condition
                )));
                self.publish_error(&error);
                self.transition_state(
                    &service.id,
                    LifecycleState::Failed,
                    Some(StateChangeReason::DependencyFailed),
                )
                .await;
                Err(error)
            }
        }
    }

    async fn service(&self, service_id: &ServiceId) -> Result<ServiceDefinition, PaloError> {
        let state = self.state.lock().await;
        state.services.get(service_id).cloned().ok_or_else(|| {
            PaloError::UiCommand(
                UiCommandError::new(CommandKind::Start, "service is not defined")
                    .for_service(service_id.clone()),
            )
        })
    }

    async fn mark_validated(&self, service_id: &ServiceId) {
        let current = {
            let state = self.state.lock().await;
            state
                .runtime
                .get(service_id)
                .map(|runtime| runtime.lifecycle)
        };

        if matches!(current, Some(LifecycleState::Discovered)) {
            debug!(service_id = %service_id, "marking service as validated");
            self.transition_state(service_id, LifecycleState::Validated, None)
                .await;
        }
    }

    async fn mark_running(&self, service_id: &ServiceId, pid: Option<u32>) {
        let (previous, restart_count) = {
            let mut state = self.state.lock().await;
            let runtime = state.runtime.entry(service_id.clone()).or_default();
            let previous = runtime.lifecycle;
            runtime.lifecycle = LifecycleState::Running;
            runtime.health = ServiceHealth::Healthy;
            runtime.pid = pid;
            runtime.started_at = Some(SystemTime::now());
            runtime.last_error = None;
            runtime.last_exit_code = None;
            (previous, runtime.restart_count)
        };

        info!(service_id = %service_id, pid, "service is now running");
        self.publish_state_change(
            service_id.clone(),
            previous,
            LifecycleState::Running,
            ServiceHealth::Healthy,
            Some(StateChangeReason::DependencyReady),
            restart_count,
        );
    }

    async fn fail_service(&self, service_id: &ServiceId, message: String) {
        let (previous, restart_count) = {
            let mut state = self.state.lock().await;
            let runtime = state.runtime.entry(service_id.clone()).or_default();
            let previous = runtime.lifecycle;
            runtime.lifecycle = LifecycleState::Failed;
            runtime.health = ServiceHealth::Unhealthy;
            runtime.pid = None;
            runtime.last_error = Some(message.clone());
            (previous, runtime.restart_count)
        };

        warn!(service_id = %service_id, error = %message, "service entered failed state");
        self.publish_state_change(
            service_id.clone(),
            previous,
            LifecycleState::Failed,
            ServiceHealth::Unhealthy,
            Some(StateChangeReason::Supervisor),
            restart_count,
        );
    }

    async fn transition_state(
        &self,
        service_id: &ServiceId,
        next: LifecycleState,
        reason: Option<StateChangeReason>,
    ) {
        let maybe_transition = {
            let mut state = self.state.lock().await;
            let runtime = state.runtime.entry(service_id.clone()).or_default();
            let previous = runtime.lifecycle;

            if previous == next || !previous.can_transition_to(next) {
                None
            } else {
                runtime.lifecycle = next;
                if matches!(next, LifecycleState::Restarting) {
                    runtime.restart_count += 1;
                }
                if matches!(next, LifecycleState::Stopped) {
                    runtime.health = ServiceHealth::Unknown;
                    runtime.pid = None;
                }
                Some((previous, runtime.health, runtime.restart_count))
            }
        };

        if let Some((previous, health, restart_count)) = maybe_transition {
            info!(
                service_id = %service_id,
                previous = ?previous,
                next = ?next,
                reason = ?reason,
                restart_count,
                "service lifecycle transition",
            );
            self.publish_state_change(
                service_id.clone(),
                previous,
                next,
                health,
                reason,
                restart_count,
            );
        }
    }

    async fn update_service_health(
        &self,
        service_id: &ServiceId,
        health: ServiceHealth,
        reason: Option<StateChangeReason>,
    ) {
        let maybe_change = {
            let mut state = self.state.lock().await;
            let runtime = state.runtime.entry(service_id.clone()).or_default();
            if runtime.health == health {
                None
            } else {
                runtime.health = health;
                Some((runtime.lifecycle, runtime.restart_count))
            }
        };

        if let Some((lifecycle, restart_count)) = maybe_change {
            debug!(
                service_id = %service_id,
                lifecycle = ?lifecycle,
                health = ?health,
                "service health changed",
            );
            self.publish_state_change(
                service_id.clone(),
                lifecycle,
                lifecycle,
                health,
                reason.or(Some(StateChangeReason::Supervisor)),
                restart_count,
            );
        }
    }

    fn publish_state_change(
        &self,
        service_id: ServiceId,
        previous: LifecycleState,
        current: LifecycleState,
        health: ServiceHealth,
        reason: Option<StateChangeReason>,
        restart_count: u64,
    ) {
        let mut event = ServiceStateChanged::new(service_id, previous, current)
            .with_health(health)
            .with_restart_count(restart_count);
        if let Some(reason) = reason {
            event = event.with_reason(reason);
        }

        let _ = self
            .events
            .publish(EventPayload::ServiceStateChanged(event));
    }

    pub(crate) fn publish_runtime_error(&self, error: &PaloError) {
        warn!(error = %error, "publishing orchestration error");
        let mut event = OrchestrationErrorEvent::new(error.stage(), error.to_string());
        if let Some(service_id) = error.service_id() {
            event = event.for_service(service_id.clone());
        }

        let _ = self.events.publish(EventPayload::OrchestrationError(event));
    }

    fn publish_error(&self, error: &PaloError) {
        self.publish_runtime_error(error);
    }

    fn publish_command_status(
        &self,
        request: CommandRequest,
        outcome: CommandOutcome,
        message: impl Into<String>,
    ) {
        let _ = self
            .events
            .publish(EventPayload::CommandStatusUpdated(CommandStatusEvent::new(
                request, outcome, message,
            )));
    }

    async fn collect_service_telemetry(&self, service_id: ServiceId, generation: u64, pid: u32) {
        let mut sampler = TelemetrySampler::new();

        loop {
            if !self
                .should_collect_telemetry(&service_id, generation, pid)
                .await
            {
                break;
            }

            let Some(snapshot) = sampler.sample_process(pid) else {
                debug!(service_id = %service_id, pid, "stopping telemetry collection because the process no longer exists");
                break;
            };

            self.record_telemetry_snapshot(&service_id, snapshot).await;
            sleep(self.telemetry_interval).await;
        }
    }

    async fn should_collect_telemetry(
        &self,
        service_id: &ServiceId,
        generation: u64,
        pid: u32,
    ) -> bool {
        let (desired_running, current_generation) = {
            let supervisors = self.supervisors.lock().await;
            let supervisor = supervisors.get(service_id).cloned().unwrap_or_default();
            (supervisor.desired_running, supervisor.generation)
        };
        let current_pid = {
            let state = self.state.lock().await;
            state
                .runtime
                .get(service_id)
                .and_then(|runtime| runtime.pid)
        };

        desired_running && current_generation == generation && current_pid == Some(pid)
    }

    async fn record_telemetry_snapshot(&self, service_id: &ServiceId, snapshot: TelemetrySnapshot) {
        {
            let mut state = self.state.lock().await;
            let runtime = state.runtime.entry(service_id.clone()).or_default();
            runtime.telemetry.record_snapshot(snapshot.clone());
        }

        let _ = self
            .events
            .publish(EventPayload::TelemetryUpdated(TelemetryUpdate::new(
                service_id.clone(),
                snapshot,
            )));
    }

    async fn record_process_result(&self, service_id: &ServiceId, result: &ProcessResult) {
        let exit_record = ExitRecord::new(result.exit_code, result.success);
        let mut state = self.state.lock().await;
        let runtime = state.runtime.entry(service_id.clone()).or_default();
        runtime.last_exit_code = result.exit_code;
        runtime.telemetry.record_exit(exit_record);
        debug!(
            service_id = %service_id,
            exit_code = result.exit_code,
            success = result.success,
            "recorded process result",
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartDecision {
    NoRestart,
    Restart { backoff: Duration },
}

impl RestartDecision {
    fn from_result(policy: &RestartPolicy, result: &ProcessResult, attempts: u32) -> Self {
        match policy {
            RestartPolicy::Manual
            | RestartPolicy::Mcp
            | RestartPolicy::Never
            | RestartPolicy::OnChange => Self::NoRestart,
            RestartPolicy::Always { backoff } => Self::Restart { backoff: *backoff },
            RestartPolicy::OnCrash {
                max_retries,
                backoff,
            } if !result.success && max_retries.map(|limit| attempts < limit).unwrap_or(true) => {
                Self::Restart { backoff: *backoff }
            }
            RestartPolicy::OnCrash { .. } => Self::NoRestart,
        }
    }
}

fn command_failure_error(
    service_id: &ServiceId,
    stage: &PipelineStage,
    exit_code: Option<i32>,
) -> PaloError {
    match stage {
        PipelineStage::Check => PaloError::Build(BuildError::new(
            service_id.clone(),
            BuildStage::Check,
            format!("check command exited with {:?}", exit_code),
        )),
        PipelineStage::Build => PaloError::Build(BuildError::new(
            service_id.clone(),
            BuildStage::Build,
            format!("build command exited with {:?}", exit_code),
        )),
        PipelineStage::Hook { name, .. } => PaloError::Build(
            BuildError::new(
                service_id.clone(),
                BuildStage::Hook,
                format!("hook command exited with {:?}", exit_code),
            )
            .with_hook_name(name.clone()),
        ),
        PipelineStage::Readiness => PaloError::Process(ProcessError::new(
            service_id.clone(),
            ProcessOperation::Readiness,
            format!("readiness command exited with {:?}", exit_code),
        )),
        PipelineStage::Run => PaloError::UiCommand(
            UiCommandError::new(
                CommandKind::Start,
                format!("run command exited with {:?}", exit_code),
            )
            .for_service(service_id.clone()),
        ),
    }
}

fn propagate_stop_errors(
    command: CommandKind,
    results: Vec<Result<ProcessResult, PaloError>>,
) -> Result<(), PaloError> {
    for result in results {
        match result {
            Ok(_) => {}
            Err(error) => return Err(error),
        }
    }

    if matches!(
        command,
        CommandKind::Stop | CommandKind::Quit | CommandKind::Restart
    ) {
        return Ok(());
    }

    Ok(())
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

fn describe_command_target(target: &CommandTarget) -> String {
    match target {
        CommandTarget::Service(service_id) => format!("service `{service_id}`"),
        CommandTarget::AllServices => "all services".to_string(),
    }
}

fn topological_start_order(
    state: &AppState,
    roots: Vec<ServiceId>,
) -> Result<Vec<ServiceId>, PaloError> {
    let mut ordered = Vec::new();
    let mut permanent = BTreeSet::new();
    let mut temporary = BTreeSet::new();

    for root in roots {
        visit_dependencies(state, &root, &mut permanent, &mut temporary, &mut ordered)?;
    }

    Ok(ordered)
}

fn visit_dependencies(
    state: &AppState,
    service_id: &ServiceId,
    permanent: &mut BTreeSet<ServiceId>,
    temporary: &mut BTreeSet<ServiceId>,
    ordered: &mut Vec<ServiceId>,
) -> Result<(), PaloError> {
    if permanent.contains(service_id) {
        return Ok(());
    }

    if !temporary.insert(service_id.clone()) {
        return Err(PaloError::Discovery(DiscoveryError::new(format!(
            "dependency cycle detected at service `{service_id}`"
        ))));
    }

    let service = state.services.get(service_id).ok_or_else(|| {
        PaloError::Discovery(DiscoveryError::new(format!(
            "service `{service_id}` is not defined"
        )))
    })?;

    for dependency in service.dependency_contracts() {
        if !state.services.contains_key(&dependency.service_id) {
            if !dependency.required {
                debug!(
                    service_id = %service_id,
                    dependency_id = %dependency.service_id,
                    "skipping optional dependency during startup ordering",
                );
                continue;
            }

            return Err(PaloError::Discovery(DiscoveryError::new(format!(
                "service `{service_id}` depends on unknown service `{}`",
                dependency.service_id
            ))));
        }

        visit_dependencies(state, &dependency.service_id, permanent, temporary, ordered)?;
    }

    temporary.remove(service_id);
    permanent.insert(service_id.clone());
    ordered.push(service_id.clone());
    Ok(())
}

fn topological_shutdown_order(
    state: &AppState,
    roots: Vec<ServiceId>,
) -> Result<Vec<ServiceId>, PaloError> {
    let mut dependents = BTreeMap::<ServiceId, Vec<ServiceId>>::new();
    for (service_id, service) in &state.services {
        for dependency in service.dependency_contracts() {
            dependents
                .entry(dependency.service_id)
                .or_default()
                .push(service_id.clone());
        }
    }

    let mut ordered = Vec::new();
    let mut seen = BTreeSet::new();
    for root in roots {
        visit_dependents(&root, &dependents, &mut seen, &mut ordered);
    }

    Ok(ordered)
}

fn restricted_shutdown_order(
    state: &AppState,
    roots: Vec<ServiceId>,
    allowed: &BTreeSet<ServiceId>,
) -> Result<Vec<ServiceId>, PaloError> {
    let order = topological_shutdown_order(state, roots)?;
    Ok(order
        .into_iter()
        .filter(|service_id| allowed.contains(service_id))
        .collect())
}

fn restart_propagation_scope(
    state: &AppState,
    root: &ServiceId,
) -> Result<BTreeSet<ServiceId>, PaloError> {
    if !state.services.contains_key(root) {
        return Err(PaloError::Discovery(DiscoveryError::new(format!(
            "service `{root}` is not defined"
        ))));
    }

    let mut dependents = BTreeMap::<ServiceId, Vec<ServiceId>>::new();
    for (service_id, service) in &state.services {
        for dependency in service.dependency_contracts() {
            if dependency.restart {
                dependents
                    .entry(dependency.service_id)
                    .or_default()
                    .push(service_id.clone());
            }
        }
    }

    let mut scope = BTreeSet::new();
    visit_restart_dependents(root, &dependents, &mut scope);
    Ok(scope)
}

async fn run_http_health_probe(
    client: &reqwest::Client,
    healthcheck: &HealthCheck,
) -> Result<u16, String> {
    let method = reqwest::Method::from_bytes(healthcheck.http.method.as_bytes())
        .map_err(|error| format!("invalid HTTP method: {error}"))?;
    let response = client
        .request(method, &healthcheck.http.url)
        .timeout(healthcheck.timeout)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status().as_u16();

    if healthcheck.http.expected_status.contains(status) {
        Ok(status)
    } else {
        Err(format!(
            "unexpected HTTP status {status}; expected {}..{}",
            healthcheck.http.expected_status.start, healthcheck.http.expected_status.end
        ))
    }
}

fn visit_restart_dependents(
    service_id: &ServiceId,
    dependents: &BTreeMap<ServiceId, Vec<ServiceId>>,
    scope: &mut BTreeSet<ServiceId>,
) {
    if !scope.insert(service_id.clone()) {
        return;
    }

    if let Some(children) = dependents.get(service_id) {
        for child in children {
            visit_restart_dependents(child, dependents, scope);
        }
    }
}

fn visit_dependents(
    service_id: &ServiceId,
    dependents: &BTreeMap<ServiceId, Vec<ServiceId>>,
    seen: &mut BTreeSet<ServiceId>,
    ordered: &mut Vec<ServiceId>,
) {
    if !seen.insert(service_id.clone()) {
        return;
    }

    if let Some(children) = dependents.get(service_id) {
        for child in children {
            visit_dependents(child, dependents, seen, ordered);
        }
    }

    ordered.push(service_id.clone());
}
