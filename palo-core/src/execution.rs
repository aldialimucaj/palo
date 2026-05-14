use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
#[cfg(unix)]
use tokio::time::Instant;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::domain::{CommandSpec, HookDefinition, HookPhase, ServiceDefinition, ServiceId};
use crate::error::{BuildError, BuildStage, PaloError, ProcessError, ProcessOperation};
use crate::events::{
    EventBus, EventPayload, LogEvent, LogOrigin, LogStream, OrchestrationErrorEvent,
    OrchestrationStage,
};

const DEFAULT_EVENT_BUS_CAPACITY: usize = 512;
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessResult {
    pub service_id: ServiceId,
    pub stage: PipelineStage,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub success: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineCommand {
    pub stage: PipelineStage,
    pub command: CommandSpec,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPipeline {
    pub service_id: ServiceId,
    pub commands: Vec<PipelineCommand>,
}

impl CommandPipeline {
    pub fn check(service: &ServiceDefinition) -> Self {
        let mut commands = Vec::new();

        if let Some(command) = &service.build.check {
            commands.push(PipelineCommand {
                stage: PipelineStage::Check,
                command: command.clone(),
                required: true,
            });
        }

        Self {
            service_id: service.id.clone(),
            commands,
        }
    }

    pub fn build(service: &ServiceDefinition) -> Self {
        let mut commands = Vec::new();

        if let Some(command) = &service.build.check {
            commands.push(PipelineCommand {
                stage: PipelineStage::Check,
                command: command.clone(),
                required: true,
            });
        }

        commands.extend(phase_commands(
            &service.id,
            &service.hooks,
            HookPhase::PreBuild,
        ));

        commands.extend(phase_commands(
            &service.id,
            &service.build.hooks,
            HookPhase::PreBuild,
        ));

        if let Some(command) = &service.build.build {
            commands.push(PipelineCommand {
                stage: PipelineStage::Build,
                command: command.clone(),
                required: true,
            });
        }

        commands.extend(phase_commands(
            &service.id,
            &service.build.hooks,
            HookPhase::PostBuild,
        ));

        commands.extend(phase_commands(
            &service.id,
            &service.hooks,
            HookPhase::PostBuild,
        ));

        Self {
            service_id: service.id.clone(),
            commands,
        }
    }

    pub fn startup(service: &ServiceDefinition) -> Self {
        let mut commands = Vec::new();

        commands.extend(Self::build(service).commands);

        commands.extend(phase_commands(
            &service.id,
            &service.hooks,
            HookPhase::PreStart,
        ));

        commands.push(PipelineCommand {
            stage: PipelineStage::Run,
            command: service.command.clone(),
            required: true,
        });

        commands.extend(phase_commands(
            &service.id,
            &service.build.hooks,
            HookPhase::PostStart,
        ));

        commands.extend(phase_commands(
            &service.id,
            &service.hooks,
            HookPhase::PostStart,
        ));

        Self {
            service_id: service.id.clone(),
            commands,
        }
    }

    pub fn shutdown(service: &ServiceDefinition) -> Self {
        let commands = phase_commands(&service.id, &service.build.hooks, HookPhase::PreStop)
            .into_iter()
            .chain(phase_commands(
                &service.id,
                &service.hooks,
                HookPhase::PreStop,
            ))
            .collect();

        Self {
            service_id: service.id.clone(),
            commands,
        }
    }

    pub fn post_shutdown(service: &ServiceDefinition) -> Self {
        let commands = phase_commands(&service.id, &service.hooks, HookPhase::PostStop);

        Self {
            service_id: service.id.clone(),
            commands,
        }
    }
}

fn phase_commands(
    _service_id: &ServiceId,
    hooks: &[HookDefinition],
    phase: HookPhase,
) -> Vec<PipelineCommand> {
    hooks
        .iter()
        .filter(|hook| hook.phase == phase)
        .map(|hook| PipelineCommand {
            stage: PipelineStage::Hook {
                phase,
                name: hook.name.clone(),
            },
            command: hook.command.clone(),
            required: hook.required,
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineStage {
    Check,
    Build,
    Run,
    Readiness,
    Hook { phase: HookPhase, name: String },
}

impl PipelineStage {
    fn description(&self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Build => "build",
            Self::Run => "run",
            Self::Readiness => "readiness check",
            Self::Hook {
                phase: HookPhase::PreBuild,
                ..
            } => "pre-build hook",
            Self::Hook {
                phase: HookPhase::PostBuild,
                ..
            } => "post-build hook",
            Self::Hook {
                phase: HookPhase::PreStart,
                ..
            } => "pre-start hook",
            Self::Hook {
                phase: HookPhase::PostStart,
                ..
            } => "post-start hook",
            Self::Hook {
                phase: HookPhase::PreStop,
                ..
            } => "pre-stop hook",
            Self::Hook {
                phase: HookPhase::PostStop,
                ..
            } => "post-stop hook",
        }
    }

    fn hook_name(&self) -> Option<&str> {
        match self {
            Self::Hook { name, .. } => Some(name),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct ProcessManager {
    events: EventBus,
    shutdown_timeout: Duration,
    processes: Arc<Mutex<BTreeMap<ServiceId, ManagedProcess>>>,
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new(EventBus::new(DEFAULT_EVENT_BUS_CAPACITY))
    }
}

impl ProcessManager {
    pub fn new(events: EventBus) -> Self {
        Self {
            events,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            processes: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn with_shutdown_timeout(mut self, shutdown_timeout: Duration) -> Self {
        self.shutdown_timeout = shutdown_timeout;
        self
    }

    pub fn events(&self) -> EventBus {
        self.events.clone()
    }

    pub async fn run_startup_pipeline(
        &self,
        service: &ServiceDefinition,
    ) -> Result<Option<ProcessResult>, PaloError> {
        let pipeline = CommandPipeline::startup(service);
        let mut run_result = None;

        for command in pipeline.commands {
            match command.stage {
                PipelineStage::Run => {
                    run_result = Some(self.spawn_service(service).await?);
                }
                _ => {
                    let result = self
                        .run_oneshot_command(
                            &pipeline.service_id,
                            command.stage.clone(),
                            &command.command,
                            CancellationToken::new(),
                        )
                        .await?;

                    if !result.success && command.required {
                        return Err(stage_error(
                            &pipeline.service_id,
                            command.stage.clone(),
                            format!(
                                "{} command exited unsuccessfully",
                                command.stage.description()
                            ),
                            result.exit_code,
                        ));
                    }
                }
            }
        }

        Ok(run_result)
    }

    pub async fn run_pipeline_command(
        &self,
        service_id: &ServiceId,
        command: &PipelineCommand,
    ) -> Result<ProcessResult, PaloError> {
        self.run_oneshot_command(
            service_id,
            command.stage.clone(),
            &command.command,
            CancellationToken::new(),
        )
        .await
    }

    pub async fn run_readiness_command(
        &self,
        service_id: &ServiceId,
        command: &CommandSpec,
        command_timeout: Duration,
    ) -> Result<ProcessResult, PaloError> {
        let cancellation = CancellationToken::new();
        let run = self.run_oneshot_command(
            service_id,
            PipelineStage::Readiness,
            command,
            cancellation.clone(),
        );
        tokio::pin!(run);

        tokio::select! {
            result = &mut run => result,
            _ = sleep(command_timeout) => {
                warn!(
                    service_id = %service_id,
                    timeout_ms = command_timeout.as_millis(),
                    "readiness command timed out",
                );
                cancellation.cancel();
                run.await
            }
        }
    }

    pub async fn spawn_service(
        &self,
        service: &ServiceDefinition,
    ) -> Result<ProcessResult, PaloError> {
        let service_id = service.id.clone();
        info!(service_id = %service_id, "spawning service process");

        let managed = ManagedProcess::new();
        let child = {
            let mut processes = self.processes.lock().await;
            if processes.contains_key(&service_id) {
                let process_error = PaloError::Process(ProcessError::new(
                    service_id.clone(),
                    ProcessOperation::Spawn,
                    "service process is already running",
                ));
                self.publish_error(&process_error);
                return Err(process_error);
            }

            let child = spawn_child(&service_id, &PipelineStage::Run, &service.command).map_err(
                |error| {
                    let process_error = PaloError::Process(ProcessError::new(
                        service_id.clone(),
                        ProcessOperation::Spawn,
                        error,
                    ));
                    self.publish_error(&process_error);
                    process_error
                },
            )?;

            processes.insert(service_id.clone(), managed.clone());
            child
        };
        let pid = child.id();

        let result_service_id = service_id.clone();
        let spawned_service_id = service_id.clone();
        let events = self.events.clone();
        let processes = Arc::clone(&self.processes);
        let shutdown_timeout = self.shutdown_timeout;
        tokio::spawn(async move {
            let result = monitor_process(
                spawned_service_id.clone(),
                PipelineStage::Run,
                child,
                events.clone(),
                managed.cancel.clone(),
                shutdown_timeout,
            )
            .await;

            managed.store_result(result).await;
            processes.lock().await.remove(&spawned_service_id);
        });

        Ok(ProcessResult {
            service_id: result_service_id,
            stage: PipelineStage::Run,
            pid,
            exit_code: None,
            success: true,
        })
    }

    pub async fn stop_service(
        &self,
        service_id: &ServiceId,
    ) -> Result<Option<ProcessResult>, PaloError> {
        let process = {
            let processes = self.processes.lock().await;
            processes.get(service_id).cloned()
        };

        let Some(process) = process else {
            return Ok(None);
        };

        info!(service_id = %service_id, "stopping managed service process");
        process.cancel.cancel();
        process.wait_for_result().await.map(Some)
    }

    pub async fn stop_all(&self) -> Vec<Result<ProcessResult, PaloError>> {
        let processes: Vec<(ServiceId, ManagedProcess)> = {
            let processes = self.processes.lock().await;
            processes
                .iter()
                .map(|(service_id, process)| (service_id.clone(), process.clone()))
                .collect()
        };

        for (service_id, process) in &processes {
            debug!(service_id = %service_id, "issuing stop-all cancellation");
            process.cancel.cancel();
        }

        let mut results = Vec::with_capacity(processes.len());
        for (_, process) in processes {
            results.push(process.wait_for_result().await);
        }

        results
    }

    pub async fn active_services(&self) -> Vec<ServiceId> {
        self.processes.lock().await.keys().cloned().collect()
    }

    pub async fn wait_for_service(
        &self,
        service_id: &ServiceId,
    ) -> Result<Option<ProcessResult>, PaloError> {
        let process = {
            let processes = self.processes.lock().await;
            processes.get(service_id).cloned()
        };

        let Some(process) = process else {
            return Ok(None);
        };

        process.wait_for_result().await.map(Some)
    }

    async fn run_oneshot_command(
        &self,
        service_id: &ServiceId,
        stage: PipelineStage,
        command: &CommandSpec,
        cancellation: CancellationToken,
    ) -> Result<ProcessResult, PaloError> {
        info!(
            service_id = %service_id,
            stage = stage.description(),
            hook = stage.hook_name(),
            "running one-shot process command",
        );
        let child = spawn_child(service_id, &stage, command).map_err(|error| {
            let mapped = stage_error(service_id, stage.clone(), error, None);
            self.publish_error(&mapped);
            mapped
        })?;

        let result = monitor_process(
            service_id.clone(),
            stage,
            child,
            self.events.clone(),
            cancellation,
            self.shutdown_timeout,
        )
        .await;

        match result {
            Ok(result) => Ok(result),
            Err(error) => {
                self.publish_error(&error);
                Err(error)
            }
        }
    }

    fn publish_error(&self, error: &PaloError) {
        let mut event = OrchestrationErrorEvent::new(error.stage(), error.to_string());
        if let Some(service_id) = error.service_id() {
            event = event.for_service(service_id.clone());
        }

        let _ = self.events.publish(EventPayload::OrchestrationError(event));
    }
}

#[derive(Clone)]
struct ManagedProcess {
    cancel: CancellationToken,
    finished: Arc<Notify>,
    result: Arc<Mutex<Option<Result<ProcessResult, PaloError>>>>,
}

impl ManagedProcess {
    fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            finished: Arc::new(Notify::new()),
            result: Arc::new(Mutex::new(None)),
        }
    }

    async fn store_result(&self, result: Result<ProcessResult, PaloError>) {
        *self.result.lock().await = Some(result);
        self.finished.notify_waiters();
    }

    async fn wait_for_result(&self) -> Result<ProcessResult, PaloError> {
        loop {
            if let Some(result) = self.result.lock().await.clone() {
                return result;
            }

            self.finished.notified().await;
        }
    }
}

async fn monitor_process(
    service_id: ServiceId,
    stage: PipelineStage,
    mut child: Child,
    events: EventBus,
    cancellation: CancellationToken,
    shutdown_timeout: Duration,
) -> Result<ProcessResult, PaloError> {
    let mut cancelled = false;
    let origin = if matches!(stage, PipelineStage::Run) {
        LogOrigin::App
    } else {
        LogOrigin::PaloInternal
    };
    let stdout_task = spawn_output_forwarder(
        service_id.clone(),
        origin,
        LogStream::Stdout,
        child.stdout.take().map(BufReader::new),
        events.clone(),
    );
    let stderr_task = spawn_output_forwarder(
        service_id.clone(),
        origin,
        LogStream::Stderr,
        child.stderr.take().map(BufReader::new),
        events.clone(),
    );

    let wait_result = tokio::select! {
        result = child.wait() => result.map_err(|error| {
            PaloError::Process(ProcessError::new(
                service_id.clone(),
                ProcessOperation::Wait,
                format!("failed waiting on child process: {error}"),
            ))
        }),
        _ = cancellation.cancelled() => {
            cancelled = true;
            info!(service_id = %service_id, stage = stage.description(), "cancellation received for child process");
            terminate_child(&service_id, &stage, &mut child, shutdown_timeout).await
        }
    }?;

    if let Some(task) = stdout_task {
        let _ = task.await;
    }
    if let Some(task) = stderr_task {
        let _ = task.await;
    }

    let result = ProcessResult {
        service_id,
        stage,
        pid: child.id(),
        exit_code: wait_result.code(),
        success: wait_result.success() || cancelled,
    };

    debug!(
        service_id = %result.service_id,
        stage = result.stage.description(),
        exit_code = result.exit_code,
        success = result.success,
        "process completed",
    );

    Ok(result)
}

fn spawn_output_forwarder<R>(
    service_id: ServiceId,
    origin: LogOrigin,
    stream: LogStream,
    reader: Option<R>,
    events: EventBus,
) -> Option<JoinHandle<()>>
where
    R: AsyncBufRead + Unpin + Send + 'static,
{
    reader.map(|reader| {
        tokio::spawn(async move {
            forward_output(service_id, origin, stream, reader, events).await;
        })
    })
}

async fn forward_output<R>(
    service_id: ServiceId,
    origin: LogOrigin,
    stream: LogStream,
    reader: R,
    events: EventBus,
) where
    R: AsyncBufRead + Unpin,
{
    let mut lines = reader.lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let _ = events.publish(EventPayload::LogEmitted(LogEvent::new(
                    service_id.clone(),
                    origin,
                    stream,
                    line,
                )));
            }
            Ok(None) => break,
            Err(error) => {
                warn!(
                    service_id = %service_id,
                    stream = ?stream,
                    error = %error,
                    "failed reading process output stream",
                );
                let _ = events.publish(EventPayload::OrchestrationError(
                    OrchestrationErrorEvent::new(
                        OrchestrationStage::Runtime,
                        format!("failed reading process output: {error}"),
                    )
                    .for_service(service_id.clone()),
                ));
                break;
            }
        }
    }
}

fn spawn_child(
    service_id: &ServiceId,
    stage: &PipelineStage,
    command: &CommandSpec,
) -> Result<Child, String> {
    debug!(
        service_id = %service_id,
        stage = stage.description(),
        program = %command.program,
        args = ?command.args,
        working_dir = ?command.working_dir,
        env_var_count = command.env.len(),
        "spawning child process",
    );

    let program = program_for_spawn(command);
    debug!(
        service_id = %service_id,
        stage = stage.description(),
        configured_program = %command.program,
        resolved_program = %program.display(),
        "resolved child process program",
    );

    let mut child = Command::new(&program);
    child.args(&command.args);
    child.envs(&command.env);
    child.stdin(Stdio::null());
    child.stdout(Stdio::piped());
    child.stderr(Stdio::piped());

    if let Some(working_dir) = &command.working_dir {
        child.current_dir(working_dir);
    }

    #[cfg(unix)]
    {
        child.process_group(0);
    }

    child.spawn().map_err(|error| {
        format!(
            "failed to spawn `{}` from `{}`: {error}",
            command.program,
            command
                .working_dir
                .as_deref()
                .unwrap_or(Path::new("."))
                .display()
        )
    })
}

fn program_for_spawn(command: &CommandSpec) -> PathBuf {
    let program = Path::new(&command.program);
    if program.is_absolute() || !program_has_path_component(&command.program) {
        return program.to_path_buf();
    }

    command
        .working_dir
        .as_ref()
        .map(|working_dir| working_dir.join(program))
        .unwrap_or_else(|| program.to_path_buf())
}

fn program_has_path_component(program: &str) -> bool {
    program.contains('/') || program.contains('\\')
}

async fn terminate_child(
    service_id: &ServiceId,
    stage: &PipelineStage,
    child: &mut Child,
    shutdown_timeout: Duration,
) -> Result<std::process::ExitStatus, PaloError> {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            info!(
                service_id = %service_id,
                stage = stage.description(),
                pid,
                "sending SIGTERM to service process group",
            );

            send_signal_to_process_group(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            )
            .map_err(|error| {
                PaloError::Process(ProcessError::new(
                    service_id.clone(),
                    ProcessOperation::Stop,
                    format!("failed to send SIGTERM: {error}"),
                ))
            })?;

            return wait_for_group_shutdown(service_id, stage, child, pid, shutdown_timeout).await;
        }
    }

    #[cfg(not(unix))]
    {
        child.start_kill().map_err(|error| {
            PaloError::Process(ProcessError::new(
                service_id.clone(),
                ProcessOperation::Stop,
                format!("failed to stop child process: {error}"),
            ))
        })?;
    }

    match timeout(shutdown_timeout, child.wait()).await {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(error)) => Err(PaloError::Process(ProcessError::new(
            service_id.clone(),
            ProcessOperation::Wait,
            format!("failed waiting on child process shutdown: {error}"),
        ))),
        Err(_) => {
            warn!(
                service_id = %service_id,
                stage = stage.description(),
                "graceful shutdown timed out, forcing kill",
            );
            child.start_kill().map_err(|error| {
                PaloError::Process(ProcessError::new(
                    service_id.clone(),
                    ProcessOperation::Stop,
                    format!("failed to force kill child process: {error}"),
                ))
            })?;

            child.wait().await.map_err(|error| {
                PaloError::Process(ProcessError::new(
                    service_id.clone(),
                    ProcessOperation::Wait,
                    format!("failed waiting on force-killed child process: {error}"),
                ))
            })
        }
    }
}

#[cfg(unix)]
fn send_signal_to_process_group(
    process_group_id: nix::unistd::Pid,
    signal: nix::sys::signal::Signal,
) -> Result<(), nix::errno::Errno> {
    match nix::sys::signal::killpg(process_group_id, signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn process_group_exists(process_group_id: nix::unistd::Pid) -> Result<bool, nix::errno::Errno> {
    match nix::sys::signal::killpg(process_group_id, None) {
        Ok(()) => Ok(true),
        Err(nix::errno::Errno::ESRCH) => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
async fn wait_for_group_shutdown(
    service_id: &ServiceId,
    stage: &PipelineStage,
    child: &mut Child,
    pid: u32,
    shutdown_timeout: Duration,
) -> Result<std::process::ExitStatus, PaloError> {
    let process_group_id = nix::unistd::Pid::from_raw(pid as i32);
    let deadline = Instant::now() + shutdown_timeout;
    let mut child_status = None;

    loop {
        if child_status.is_none() {
            child_status = child.try_wait().map_err(|error| {
                PaloError::Process(ProcessError::new(
                    service_id.clone(),
                    ProcessOperation::Wait,
                    format!("failed checking child process shutdown: {error}"),
                ))
            })?;
        }

        let group_running = match process_group_exists(process_group_id) {
            Ok(group_running) => group_running,
            Err(nix::errno::Errno::EPERM) if child_status.is_some() => {
                warn!(
                    service_id = %service_id,
                    stage = stage.description(),
                    process_group_id = pid,
                    "process group liveness check was denied after child exited; treating shutdown as complete",
                );
                false
            }
            Err(nix::errno::Errno::EPERM) => {
                warn!(
                    service_id = %service_id,
                    stage = stage.description(),
                    process_group_id = pid,
                    "process group liveness check was denied; waiting for child shutdown",
                );
                true
            }
            Err(error) => {
                return Err(PaloError::Process(ProcessError::new(
                    service_id.clone(),
                    ProcessOperation::Wait,
                    format!("failed checking process group shutdown: {error}"),
                )));
            }
        };

        if child_status.is_some() && !group_running {
            return Ok(child_status.expect("status checked above"));
        }

        if Instant::now() >= deadline {
            break;
        }

        sleep(Duration::from_millis(25)).await;
    }

    warn!(
        service_id = %service_id,
        stage = stage.description(),
        process_group_id = pid,
        "graceful process group shutdown timed out, forcing kill",
    );

    send_signal_to_process_group(process_group_id, nix::sys::signal::Signal::SIGKILL).map_err(
        |error| {
            PaloError::Process(ProcessError::new(
                service_id.clone(),
                ProcessOperation::Stop,
                format!("failed to force kill process group: {error}"),
            ))
        },
    )?;

    if let Some(status) = child_status {
        return Ok(status);
    }

    child.wait().await.map_err(|error| {
        PaloError::Process(ProcessError::new(
            service_id.clone(),
            ProcessOperation::Wait,
            format!("failed waiting on force-killed child process: {error}"),
        ))
    })
}

fn stage_error(
    service_id: &ServiceId,
    stage: PipelineStage,
    message: impl Into<String>,
    exit_code: Option<i32>,
) -> PaloError {
    match stage {
        PipelineStage::Check => {
            let mut error = BuildError::new(service_id.clone(), BuildStage::Check, message);
            if let Some(code) = exit_code {
                error = error.with_exit_code(code);
            }
            PaloError::Build(error)
        }
        PipelineStage::Build => {
            let mut error = BuildError::new(service_id.clone(), BuildStage::Build, message);
            if let Some(code) = exit_code {
                error = error.with_exit_code(code);
            }
            PaloError::Build(error)
        }
        PipelineStage::Hook { name, .. } => {
            let mut error =
                BuildError::new(service_id.clone(), BuildStage::Hook, message).with_hook_name(name);
            if let Some(code) = exit_code {
                error = error.with_exit_code(code);
            }
            PaloError::Build(error)
        }
        PipelineStage::Readiness => PaloError::Process(ProcessError::new(
            service_id.clone(),
            ProcessOperation::Readiness,
            message,
        )),
        PipelineStage::Run => PaloError::Process(ProcessError::new(
            service_id.clone(),
            ProcessOperation::Spawn,
            message,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        BuildDefinition, DEFAULT_SERVICE_LOG_RETENTION, RestartPolicy, WatchConfiguration,
    };

    fn sample_service() -> ServiceDefinition {
        ServiceDefinition {
            id: ServiceId::new("api"),
            name: "api".to_string(),
            command: CommandSpec::new("sh").with_args(["-c", "printf run"]),
            build: BuildDefinition {
                check: Some(CommandSpec::new("sh").with_args(["-c", "printf check"])),
                build: Some(CommandSpec::new("sh").with_args(["-c", "printf build"])),
                hooks: Vec::new(),
            },
            readiness: None,
            healthcheck: None,
            restart: RestartPolicy::Manual,
            watch: WatchConfiguration::disabled(),
            dependencies: Vec::new(),
            depends_on: Vec::new(),
            hooks: vec![
                HookDefinition {
                    name: "pre-build".to_string(),
                    phase: HookPhase::PreBuild,
                    command: CommandSpec::new("sh").with_args(["-c", "printf pre"]),
                    required: true,
                },
                HookDefinition {
                    name: "post-build".to_string(),
                    phase: HookPhase::PostBuild,
                    command: CommandSpec::new("sh").with_args(["-c", "printf post"]),
                    required: true,
                },
                HookDefinition {
                    name: "pre-start".to_string(),
                    phase: HookPhase::PreStart,
                    command: CommandSpec::new("sh").with_args(["-c", "printf warmup"]),
                    required: true,
                },
                HookDefinition {
                    name: "post-start".to_string(),
                    phase: HookPhase::PostStart,
                    command: CommandSpec::new("sh").with_args(["-c", "printf ready"]),
                    required: false,
                },
                HookDefinition {
                    name: "pre-stop".to_string(),
                    phase: HookPhase::PreStop,
                    command: CommandSpec::new("sh").with_args(["-c", "printf drain"]),
                    required: true,
                },
                HookDefinition {
                    name: "post-stop".to_string(),
                    phase: HookPhase::PostStop,
                    command: CommandSpec::new("sh").with_args(["-c", "printf cleanup"]),
                    required: true,
                },
            ],
            log_retention: DEFAULT_SERVICE_LOG_RETENTION,
        }
    }

    #[test]
    fn spawn_program_resolves_relative_path_against_working_dir() {
        let command = CommandSpec::new("target/debug/smoke-app.exe").with_working_dir("workspace");

        assert_eq!(
            program_for_spawn(&command),
            PathBuf::from("workspace").join("target/debug/smoke-app.exe")
        );
    }

    #[test]
    fn spawn_program_leaves_pathless_command_for_system_lookup() {
        let command = CommandSpec::new("cargo").with_working_dir("workspace");

        assert_eq!(program_for_spawn(&command), PathBuf::from("cargo"));
    }

    #[test]
    fn startup_pipeline_orders_check_build_run_and_hooks() {
        let pipeline = CommandPipeline::startup(&sample_service());
        let stages: Vec<_> = pipeline
            .commands
            .into_iter()
            .map(|command| command.stage)
            .collect();

        assert_eq!(
            stages,
            vec![
                PipelineStage::Check,
                PipelineStage::Hook {
                    phase: HookPhase::PreBuild,
                    name: "pre-build".to_string(),
                },
                PipelineStage::Build,
                PipelineStage::Hook {
                    phase: HookPhase::PostBuild,
                    name: "post-build".to_string(),
                },
                PipelineStage::Hook {
                    phase: HookPhase::PreStart,
                    name: "pre-start".to_string(),
                },
                PipelineStage::Run,
                PipelineStage::Hook {
                    phase: HookPhase::PostStart,
                    name: "post-start".to_string(),
                },
            ]
        );
    }

    #[test]
    fn shutdown_pipeline_orders_stop_hooks() {
        let service = sample_service();
        let shutdown_stages: Vec<_> = CommandPipeline::shutdown(&service)
            .commands
            .into_iter()
            .map(|command| command.stage)
            .collect();
        let post_shutdown_stages: Vec<_> = CommandPipeline::post_shutdown(&service)
            .commands
            .into_iter()
            .map(|command| command.stage)
            .collect();

        assert_eq!(
            shutdown_stages,
            vec![PipelineStage::Hook {
                phase: HookPhase::PreStop,
                name: "pre-stop".to_string(),
            }]
        );
        assert_eq!(
            post_shutdown_stages,
            vec![PipelineStage::Hook {
                phase: HookPhase::PostStop,
                name: "post-stop".to_string(),
            }]
        );
    }
}
