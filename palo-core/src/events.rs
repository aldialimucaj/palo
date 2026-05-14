use std::time::SystemTime;

use tokio::sync::broadcast;

use crate::domain::{LifecycleState, ServiceHealth, ServiceId};
use crate::telemetry::TelemetrySnapshot;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub emitted_at: SystemTime,
    pub payload: EventPayload,
}

#[derive(Debug, Clone)]
pub struct EventBus {
    sender: broadcast::Sender<Event>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(256)
    }
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    pub fn publish(
        &self,
        payload: EventPayload,
    ) -> Result<usize, broadcast::error::SendError<Event>> {
        self.sender.send(Event::new(payload))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }
}

impl Event {
    pub fn new(payload: EventPayload) -> Self {
        Self {
            emitted_at: SystemTime::now(),
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventPayload {
    ServiceStateChanged(ServiceStateChanged),
    LogEmitted(LogEvent),
    CommandRequested(CommandRequest),
    CommandStatusUpdated(CommandStatusEvent),
    TelemetryUpdated(TelemetryUpdate),
    OrchestrationError(OrchestrationErrorEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStateChanged {
    pub service_id: ServiceId,
    pub previous: LifecycleState,
    pub current: LifecycleState,
    pub health: ServiceHealth,
    pub restart_count: u64,
    pub reason: Option<StateChangeReason>,
}

impl ServiceStateChanged {
    pub fn new(
        service_id: impl Into<ServiceId>,
        previous: LifecycleState,
        current: LifecycleState,
    ) -> Self {
        Self {
            service_id: service_id.into(),
            previous,
            current,
            health: ServiceHealth::Unknown,
            restart_count: 0,
            reason: None,
        }
    }

    pub fn with_health(mut self, health: ServiceHealth) -> Self {
        self.health = health;
        self
    }

    pub fn with_reason(mut self, reason: StateChangeReason) -> Self {
        self.reason = Some(reason);
        self
    }

    pub fn with_restart_count(mut self, restart_count: u64) -> Self {
        self.restart_count = restart_count;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateChangeReason {
    Supervisor,
    DependencyReady,
    DependencyFailed,
    BuildCompleted,
    ProcessExited { exit_code: Option<i32> },
    ProcessCrashed { message: String },
    WatchTriggered { path: Option<String> },
    Command(CommandKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogOrigin {
    App,
    PaloInternal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEvent {
    pub service_id: ServiceId,
    pub origin: LogOrigin,
    pub stream: LogStream,
    pub message: String,
}

impl LogEvent {
    pub fn new(
        service_id: impl Into<ServiceId>,
        origin: LogOrigin,
        stream: LogStream,
        message: impl Into<String>,
    ) -> Self {
        Self {
            service_id: service_id.into(),
            origin,
            stream,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRequest {
    pub target: CommandTarget,
    pub command: CommandKind,
}

impl CommandRequest {
    pub fn for_service(service_id: impl Into<ServiceId>, command: CommandKind) -> Self {
        Self {
            target: CommandTarget::Service(service_id.into()),
            command,
        }
    }

    pub fn for_all(command: CommandKind) -> Self {
        Self {
            target: CommandTarget::AllServices,
            command,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandStatusEvent {
    pub request: CommandRequest,
    pub outcome: CommandOutcome,
    pub message: String,
}

impl CommandStatusEvent {
    pub fn new(
        request: CommandRequest,
        outcome: CommandOutcome,
        message: impl Into<String>,
    ) -> Self {
        Self {
            request,
            outcome,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandTarget {
    Service(ServiceId),
    AllServices,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    Start,
    Stop,
    Restart,
    Validate,
    Check,
    Build,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutcome {
    Accepted,
    Completed,
    Rejected,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryUpdate {
    pub service_id: ServiceId,
    pub snapshot: TelemetrySnapshot,
}

impl TelemetryUpdate {
    pub fn new(service_id: impl Into<ServiceId>, snapshot: TelemetrySnapshot) -> Self {
        Self {
            service_id: service_id.into(),
            snapshot,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestrationErrorEvent {
    pub service_id: Option<ServiceId>,
    pub stage: OrchestrationStage,
    pub message: String,
}

impl OrchestrationErrorEvent {
    pub fn new(stage: OrchestrationStage, message: impl Into<String>) -> Self {
        Self {
            service_id: None,
            stage,
            message: message.into(),
        }
    }

    pub fn for_service(mut self, service_id: impl Into<ServiceId>) -> Self {
        self.service_id = Some(service_id.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrchestrationStage {
    Validation,
    Check,
    Build,
    Start,
    Runtime,
    Stop,
    Restart,
    Watch,
    DependencyResolution,
    CommandHandling,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{LifecycleState, ServiceHealth};

    #[test]
    fn event_records_emission_time() {
        let before = SystemTime::now();
        let event = Event::new(EventPayload::CommandRequested(CommandRequest::for_all(
            CommandKind::Restart,
        )));
        let after = SystemTime::now();

        assert!(event.emitted_at >= before);
        assert!(event.emitted_at <= after);
    }

    #[test]
    fn service_state_change_captures_optional_metadata() {
        let event =
            ServiceStateChanged::new("api", LifecycleState::Starting, LifecycleState::Running)
                .with_health(ServiceHealth::Healthy)
                .with_reason(StateChangeReason::BuildCompleted);

        assert_eq!(event.service_id, ServiceId::new("api"));
        assert_eq!(event.previous, LifecycleState::Starting);
        assert_eq!(event.current, LifecycleState::Running);
        assert_eq!(event.health, ServiceHealth::Healthy);
        assert_eq!(event.restart_count, 0);
        assert_eq!(event.reason, Some(StateChangeReason::BuildCompleted));
    }

    #[test]
    fn command_requests_support_service_and_global_targets() {
        let service_request = CommandRequest::for_service("api", CommandKind::Start);
        let global_request = CommandRequest::for_all(CommandKind::Stop);

        assert_eq!(
            service_request.target,
            CommandTarget::Service(ServiceId::new("api"))
        );
        assert_eq!(service_request.command, CommandKind::Start);
        assert_eq!(global_request.target, CommandTarget::AllServices);
        assert_eq!(global_request.command, CommandKind::Stop);
    }

    #[test]
    fn orchestration_errors_can_be_bound_to_a_service() {
        let event = OrchestrationErrorEvent::new(
            OrchestrationStage::Build,
            "build command exited unsuccessfully",
        )
        .for_service("api");

        assert_eq!(event.service_id, Some(ServiceId::new("api")));
        assert_eq!(event.stage, OrchestrationStage::Build);
        assert_eq!(event.message, "build command exited unsuccessfully");
    }

    #[test]
    fn event_bus_broadcasts_published_events() {
        let bus = EventBus::new(8);
        let mut receiver = bus.subscribe();

        bus.publish(EventPayload::CommandRequested(CommandRequest::for_all(
            CommandKind::Start,
        )))
        .expect("event should be published");

        let event = receiver.try_recv().expect("event should be received");
        assert!(matches!(
            event.payload,
            EventPayload::CommandRequested(CommandRequest {
                target: CommandTarget::AllServices,
                command: CommandKind::Start,
            })
        ));
    }

    #[test]
    fn command_status_event_preserves_request_and_outcome() {
        let request = CommandRequest::for_service("api", CommandKind::Restart);
        let event = CommandStatusEvent::new(
            request.clone(),
            CommandOutcome::Rejected,
            "service is restarting",
        );

        assert_eq!(event.request, request);
        assert_eq!(event.outcome, CommandOutcome::Rejected);
        assert_eq!(event.message, "service is restarting");
    }
}
