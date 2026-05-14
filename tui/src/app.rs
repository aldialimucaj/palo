use crate::event::{AppEvent, Event, EventHandler};
use crate::ui::{
    DashboardPane, dashboard_content_area, dashboard_layout,
    service_index_at_content_row_with_offset, service_viewport_start, services_content_area,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use palo_core::domain::{
    AppState as CoreAppState, DEFAULT_SERVICE_LOG_RETENTION, LifecycleState, ServiceHealth,
    ServiceId, ServiceRuntime,
};
use palo_core::events::{
    CommandKind, CommandOutcome, CommandRequest, CommandStatusEvent, Event as CoreEvent, EventBus,
    EventPayload, LogEvent, LogOrigin, LogStream, OrchestrationErrorEvent, ServiceStateChanged,
    StateChangeReason, TelemetryUpdate,
};
use palo_core::orchestration::Orchestrator;
use palo_core::telemetry::TelemetrySnapshot;
use ratatui::DefaultTerminal;
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

const MAX_HISTORY_ENTRIES: usize = 32;
const MAX_RESOURCE_HISTORY_ENTRIES: usize = 64;

/// Application.
#[derive(Debug)]
pub struct App {
    /// Is the application running?
    pub running: bool,
    /// Event handler.
    pub events: EventHandler,
    /// UI-facing application state.
    pub state: TuiState,
    core_events: EventBus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiState {
    pub services: BTreeMap<ServiceId, ServiceViewState>,
    pub service_order: Vec<ServiceId>,
    pub selected_service: Option<ServiceId>,
    pub focused_pane: FocusedPane,
    pub command_mode: CommandMode,
    pub command_buffer: String,
    pub summary: GlobalSummary,
    pub resource_history: VecDeque<ResourceHistorySample>,
    pub global_errors: VecDeque<String>,
    pub status_line: Option<StatusLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusedPane {
    Services,
    Details,
    Logs,
}

impl FocusedPane {
    fn next(self) -> Self {
        match self {
            Self::Services => Self::Details,
            Self::Details => Self::Logs,
            Self::Logs => Self::Services,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Services => Self::Logs,
            Self::Details => Self::Services,
            Self::Logs => Self::Details,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandMode {
    Normal,
    Command,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GlobalSummary {
    pub total_services: usize,
    pub running_services: usize,
    pub failed_services: usize,
    pub unhealthy_services: usize,
    pub transitioning_services: usize,
    pub aggregate_restart_count: u64,
    pub aggregate_cpu_millis: u64,
    pub aggregate_memory_bytes: u64,
    pub aggregate_disk_read_bytes: Option<u64>,
    pub aggregate_disk_written_bytes: Option<u64>,
    pub aggregate_network_rx_bytes: Option<u64>,
    pub aggregate_network_tx_bytes: Option<u64>,
    pub open_port_count: usize,
    pub service_error_count: usize,
    pub global_error_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResourceHistorySample {
    pub cpu_millis: u64,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub network_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceViewState {
    pub id: ServiceId,
    pub name: String,
    pub lifecycle: LifecycleState,
    pub health: ServiceHealth,
    pub last_reason: Option<String>,
    pub restart_count: u64,
    pub last_exit_code: Option<i32>,
    pub last_error: Option<String>,
    pub pid: Option<u32>,
    pub uptime: Option<Duration>,
    pub cpu_millis: u64,
    pub memory_bytes: u64,
    pub open_ports: Vec<u16>,
    pub disk_read_bytes: Option<u64>,
    pub disk_written_bytes: Option<u64>,
    pub network_rx_bytes: Option<u64>,
    pub network_tx_bytes: Option<u64>,
    pub last_watch_event: Option<String>,
    pub log_retention: usize,
    pub recent_logs: VecDeque<LogLine>,
    pub lifecycle_history: VecDeque<LifecycleEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub stream: LogStream,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleEntry {
    pub state: LifecycleState,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub outcome: CommandOutcome,
    pub message: String,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            services: BTreeMap::new(),
            service_order: Vec::new(),
            selected_service: None,
            focused_pane: FocusedPane::Services,
            command_mode: CommandMode::Normal,
            command_buffer: String::new(),
            summary: GlobalSummary::default(),
            resource_history: VecDeque::new(),
            global_errors: VecDeque::new(),
            status_line: None,
        }
    }
}

impl App {
    /// Constructs a new instance of [`App`].
    pub fn new(
        snapshot: CoreAppState,
        core_events: broadcast::Receiver<CoreEvent>,
        event_bus: EventBus,
    ) -> Self {
        let state = TuiState::from_snapshot(snapshot);
        Self {
            running: true,
            events: EventHandler::with_core_events(Some(core_events)),
            state,
            core_events: event_bus,
        }
    }

    pub async fn from_orchestrator(orchestrator: &Orchestrator) -> Self {
        let snapshot = orchestrator.snapshot_state().await;
        let event_bus = orchestrator.events();
        orchestrator.spawn_command_handler();
        let core_events = event_bus.subscribe();
        Self::new(snapshot, core_events, event_bus)
    }

    /// Run the application's main loop.
    pub async fn run(mut self, mut terminal: DefaultTerminal) -> color_eyre::Result<()> {
        info!(
            service_count = self.state.summary.total_services,
            "starting palo tui"
        );
        while self.running {
            terminal.draw(|frame| frame.render_widget(&self, frame.area()))?;
            match self.events.next().await? {
                Event::Tick => self.tick(),
                Event::Crossterm(event) => match event {
                    crossterm::event::Event::Key(key_event)
                        if key_event.kind == crossterm::event::KeyEventKind::Press =>
                    {
                        self.handle_key_events(key_event)?
                    }
                    crossterm::event::Event::Mouse(mouse_event) => {
                        self.handle_mouse_events(mouse_event)
                    }
                    _ => {}
                },
                Event::Core(core_event) => self.handle_core_event(core_event),
                Event::App(app_event) => match app_event {
                    AppEvent::Quit => self.quit(),
                },
            }
        }
        info!("palo tui stopped");
        Ok(())
    }

    /// Handles the key events and updates the state of [`App`].
    pub fn handle_key_events(&mut self, key_event: KeyEvent) -> color_eyre::Result<()> {
        if self.state.command_mode == CommandMode::Command {
            self.handle_command_mode_key(key_event);
            return Ok(());
        }

        match key_event.code {
            KeyCode::Char('q') => self.request_global_command(CommandKind::Quit),
            KeyCode::Char('c' | 'C') if key_event.modifiers == KeyModifiers::CONTROL => {
                self.request_global_command(CommandKind::Quit)
            }
            KeyCode::Char(':') => {
                self.state.command_mode = CommandMode::Command;
                self.state.command_buffer.clear();
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.state.focused_pane = self.state.focused_pane.next();
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.state.focused_pane = self.state.focused_pane.previous();
            }
            KeyCode::Down | KeyCode::Char('j') => self.state.select_next_service(),
            KeyCode::Up | KeyCode::Char('k') => self.state.select_previous_service(),
            KeyCode::Char('s') => self.request_selected_service_command(CommandKind::Start),
            KeyCode::Char('x') => self.request_selected_service_command(CommandKind::Stop),
            KeyCode::Char('r') => self.request_selected_service_command(CommandKind::Restart),
            KeyCode::Char('S') => self.request_global_command(CommandKind::Start),
            KeyCode::Char('X') => self.request_global_command(CommandKind::Stop),
            KeyCode::Char('R') => self.request_global_command(CommandKind::Restart),
            _ => {}
        }
        Ok(())
    }

    pub fn handle_mouse_events(&mut self, mouse_event: MouseEvent) {
        if mouse_event.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }

        let Ok((width, height)) = crossterm::terminal::size() else {
            return;
        };
        let terminal_area = ratatui::layout::Rect::new(0, 0, width, height);
        let layout = dashboard_layout(dashboard_content_area(terminal_area));
        match layout.pane_at(mouse_event.column, mouse_event.row) {
            Some(DashboardPane::Services) => {
                self.state.focused_pane = FocusedPane::Services;
                let content = services_content_area(layout.services);
                if mouse_event.row >= content.y
                    && mouse_event.row < content.y.saturating_add(content.height)
                {
                    let row = mouse_event.row.saturating_sub(content.y);
                    let selected_index =
                        self.state.selected_service.as_ref().and_then(|selected| {
                            self.state
                                .service_order
                                .iter()
                                .position(|service_id| service_id == selected)
                        });
                    let viewport_start = service_viewport_start(
                        self.state.service_order.len(),
                        selected_index,
                        content.height,
                    );
                    if let Some(index) =
                        service_index_at_content_row_with_offset(row, viewport_start)
                    {
                        self.state.select_service_at(index);
                    }
                }
            }
            Some(DashboardPane::Details) => {
                self.state.focused_pane = FocusedPane::Details;
            }
            Some(DashboardPane::Logs) => {
                self.state.focused_pane = FocusedPane::Logs;
            }
            None => {}
        }
    }

    /// Handles the tick event of the terminal.
    ///
    /// The tick event is where you can update the state of your application with any logic that
    /// needs to be updated at a fixed frame rate. E.g. polling a server, updating an animation.
    pub fn tick(&mut self) {
        self.state.refresh_summary();
    }

    /// Set running to false to quit the application.
    pub fn quit(&mut self) {
        self.running = false;
    }

    fn handle_core_event(&mut self, event: CoreEvent) {
        debug!(payload = ?event.payload, "processing core event in tui");
        self.state.apply_core_event(event);
        if self.state.status_line.as_ref().is_some_and(|status| {
            matches!(status.outcome, CommandOutcome::Completed)
                && status.message == "stopped all services and exited"
        }) {
            self.quit();
        }
    }

    fn handle_command_mode_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc => {
                self.state.command_mode = CommandMode::Normal;
                self.state.command_buffer.clear();
            }
            KeyCode::Enter => self.submit_command_buffer(),
            KeyCode::Backspace => {
                self.state.command_buffer.pop();
            }
            KeyCode::Char(ch)
                if key_event.modifiers.is_empty() || key_event.modifiers == KeyModifiers::SHIFT =>
            {
                self.state.command_buffer.push(ch);
            }
            KeyCode::Char('c' | 'C') if key_event.modifiers == KeyModifiers::CONTROL => {
                self.state.command_mode = CommandMode::Normal;
                self.state.command_buffer.clear();
            }
            _ => {}
        }
    }

    fn submit_command_buffer(&mut self) {
        let command = self.state.command_buffer.trim().to_string();
        self.state.command_mode = CommandMode::Normal;
        self.state.command_buffer.clear();

        if command.is_empty() {
            return;
        }

        match parse_command(&command, self.state.selected_service.clone()) {
            Ok(request) => self.publish_command(request),
            Err(message) => {
                self.state.status_line = Some(StatusLine {
                    outcome: CommandOutcome::Rejected,
                    message,
                });
            }
        }
    }

    fn request_selected_service_command(&mut self, command: CommandKind) {
        match self.state.selected_service.clone() {
            Some(service_id) => {
                self.publish_command(CommandRequest::for_service(service_id, command))
            }
            None => {
                self.state.status_line = Some(StatusLine {
                    outcome: CommandOutcome::Rejected,
                    message: format!("no service selected for {}", describe_command(command)),
                });
            }
        }
    }

    fn request_global_command(&mut self, command: CommandKind) {
        self.publish_command(CommandRequest::for_all(command));
    }

    fn publish_command(&mut self, request: CommandRequest) {
        info!(command = ?request.command, target = ?request.target, "publishing tui command request");
        let _ = self
            .core_events
            .publish(EventPayload::CommandRequested(request));
    }
}

impl TuiState {
    pub fn from_snapshot(snapshot: CoreAppState) -> Self {
        let mut services = BTreeMap::new();
        let mut service_order = Vec::new();

        for (service_id, definition) in &snapshot.services {
            let runtime = snapshot
                .runtime
                .get(service_id)
                .cloned()
                .unwrap_or_default();
            service_order.push(service_id.clone());
            services.insert(
                service_id.clone(),
                ServiceViewState::from_runtime(
                    service_id.clone(),
                    definition.name.clone(),
                    definition.log_retention,
                    runtime,
                ),
            );
        }

        let mut state = Self {
            selected_service: service_order.first().cloned(),
            services,
            service_order,
            ..Self::default()
        };
        state.refresh_summary();
        state.record_resource_sample();
        state
    }

    pub fn apply_core_event(&mut self, event: CoreEvent) {
        let should_record_resource_sample = match event.payload {
            EventPayload::ServiceStateChanged(change) => {
                self.apply_state_change(change);
                false
            }
            EventPayload::LogEmitted(log) => {
                self.apply_log(log);
                false
            }
            EventPayload::CommandStatusUpdated(status) => {
                self.apply_command_status(status);
                false
            }
            EventPayload::TelemetryUpdated(update) => {
                self.apply_telemetry(update);
                true
            }
            EventPayload::OrchestrationError(error) => {
                self.apply_error(error);
                false
            }
            EventPayload::CommandRequested(_) => false,
        };

        self.ensure_selection();
        self.refresh_summary();
        if should_record_resource_sample {
            self.record_resource_sample();
        }
    }

    pub fn selected_service(&self) -> Option<&ServiceViewState> {
        self.selected_service
            .as_ref()
            .and_then(|service_id| self.services.get(service_id))
    }

    pub fn select_next_service(&mut self) {
        self.selected_service = self.offset_selection(1);
    }

    pub fn select_previous_service(&mut self) {
        self.selected_service = self.offset_selection(-1);
    }

    pub fn select_service_at(&mut self, index: usize) {
        if let Some(service_id) = self.service_order.get(index) {
            self.selected_service = Some(service_id.clone());
        }
    }

    pub fn refresh_summary(&mut self) {
        let mut summary = GlobalSummary {
            total_services: self.services.len(),
            global_error_count: self.global_errors.len(),
            ..GlobalSummary::default()
        };

        for service in self.services.values() {
            if service.lifecycle == LifecycleState::Running {
                summary.running_services += 1;
            }
            if service.lifecycle == LifecycleState::Failed {
                summary.failed_services += 1;
            }
            if matches!(
                service.health,
                ServiceHealth::Degraded | ServiceHealth::Unhealthy
            ) {
                summary.unhealthy_services += 1;
            }
            if matches!(
                service.lifecycle,
                LifecycleState::Starting | LifecycleState::Restarting
            ) {
                summary.transitioning_services += 1;
            }
            if service.last_error.is_some() {
                summary.service_error_count += 1;
            }
            summary.aggregate_restart_count += service.restart_count;
            summary.aggregate_cpu_millis += service.cpu_millis;
            summary.aggregate_memory_bytes += service.memory_bytes;
            summary.aggregate_disk_read_bytes =
                add_optional_bytes(summary.aggregate_disk_read_bytes, service.disk_read_bytes);
            summary.aggregate_disk_written_bytes = add_optional_bytes(
                summary.aggregate_disk_written_bytes,
                service.disk_written_bytes,
            );
            summary.aggregate_network_rx_bytes =
                add_optional_bytes(summary.aggregate_network_rx_bytes, service.network_rx_bytes);
            summary.aggregate_network_tx_bytes =
                add_optional_bytes(summary.aggregate_network_tx_bytes, service.network_tx_bytes);
            summary.open_port_count += service.open_ports.len();
        }

        self.summary = summary;
    }

    fn record_resource_sample(&mut self) {
        let sample = ResourceHistorySample {
            cpu_millis: self.summary.aggregate_cpu_millis,
            memory_bytes: self.summary.aggregate_memory_bytes,
            disk_bytes: self
                .summary
                .aggregate_disk_read_bytes
                .unwrap_or_default()
                .saturating_add(
                    self.summary
                        .aggregate_disk_written_bytes
                        .unwrap_or_default(),
                ),
            network_bytes: self
                .summary
                .aggregate_network_rx_bytes
                .unwrap_or_default()
                .saturating_add(self.summary.aggregate_network_tx_bytes.unwrap_or_default()),
        };
        push_bounded(
            &mut self.resource_history,
            sample,
            MAX_RESOURCE_HISTORY_ENTRIES,
        );
    }

    fn apply_state_change(&mut self, change: ServiceStateChanged) {
        let reason = change.reason.as_ref().map(describe_reason);
        let service = self.ensure_service(&change.service_id);
        service.lifecycle = change.current;
        service.health = change.health;
        if let Some(StateChangeReason::WatchTriggered { path }) = change.reason.as_ref() {
            service.last_watch_event = Some(match path {
                Some(path) => format!("file changed: {path}"),
                None => "file change detected".to_string(),
            });
        }
        if change.restart_count > service.restart_count {
            service.restart_count = change.restart_count;
        } else if change.current == LifecycleState::Restarting && change.restart_count == 0 {
            service.restart_count += 1;
        }
        service.last_reason = reason.clone();

        if let Some(reason) = reason {
            push_bounded(
                &mut service.lifecycle_history,
                LifecycleEntry {
                    state: change.current,
                    reason: Some(reason),
                },
                MAX_HISTORY_ENTRIES,
            );
        } else {
            push_bounded(
                &mut service.lifecycle_history,
                LifecycleEntry {
                    state: change.current,
                    reason: None,
                },
                MAX_HISTORY_ENTRIES,
            );
        }

        match change.reason {
            Some(StateChangeReason::ProcessExited { exit_code }) => {
                service.last_exit_code = exit_code;
                if change.current == LifecycleState::Failed {
                    service.last_error = Some(format!(
                        "process exited with status {}",
                        format_exit_code(exit_code)
                    ));
                }
            }
            Some(StateChangeReason::ProcessCrashed { message }) => {
                service.last_error = Some(message);
            }
            _ => {}
        }
    }

    fn apply_log(&mut self, log: LogEvent) {
        if log.origin != LogOrigin::App {
            debug!(
                service_id = %log.service_id,
                stream = ?log.stream,
                "ignoring non-app log entry in tui"
            );
            return;
        }

        let sanitized = sanitize_log_message_for_tui(&log.message);
        if sanitized.changed {
            debug!(
                service_id = %log.service_id,
                stream = ?log.stream,
                "sanitized app log entry for tui rendering"
            );
        }

        let service = self.ensure_service(&log.service_id);
        let log_retention = service.log_retention;
        push_bounded(
            &mut service.recent_logs,
            LogLine {
                stream: log.stream,
                message: sanitized.message,
            },
            log_retention,
        );
    }

    fn apply_telemetry(&mut self, update: TelemetryUpdate) {
        let service = self.ensure_service(&update.service_id);
        apply_snapshot(service, &update.snapshot);
    }

    fn apply_command_status(&mut self, status: CommandStatusEvent) {
        let CommandStatusEvent {
            request,
            outcome,
            message,
        } = status;

        self.status_line = Some(StatusLine {
            outcome,
            message: message.clone(),
        });

        if let palo_core::events::CommandTarget::Service(service_id) = request.target {
            match outcome {
                CommandOutcome::Rejected | CommandOutcome::Failed => {
                    let service = self.ensure_service(&service_id);
                    service.last_error = Some(message.clone());

                    push_bounded(
                        &mut service.lifecycle_history,
                        LifecycleEntry {
                            state: service.lifecycle,
                            reason: Some(message),
                        },
                        MAX_HISTORY_ENTRIES,
                    );
                }
                CommandOutcome::Accepted | CommandOutcome::Completed => {}
            }
        }
    }

    fn apply_error(&mut self, error: OrchestrationErrorEvent) {
        let message = format!("{}: {}", describe_stage(error.stage), error.message);
        match error.service_id {
            Some(service_id) => {
                let service = self.ensure_service(&service_id);
                service.last_error = Some(message.clone());
                push_bounded(
                    &mut service.lifecycle_history,
                    LifecycleEntry {
                        state: service.lifecycle,
                        reason: Some(message),
                    },
                    MAX_HISTORY_ENTRIES,
                );
            }
            None => {
                warn!(error = %message, "received global orchestration error");
                push_bounded(&mut self.global_errors, message, MAX_HISTORY_ENTRIES);
            }
        }
    }

    fn ensure_service(&mut self, service_id: &ServiceId) -> &mut ServiceViewState {
        if !self.services.contains_key(service_id) {
            self.service_order.push(service_id.clone());
            self.services.insert(
                service_id.clone(),
                ServiceViewState::placeholder(service_id.clone()),
            );
        }

        self.services
            .get_mut(service_id)
            .expect("service should exist after insertion")
    }

    fn ensure_selection(&mut self) {
        if self
            .selected_service
            .as_ref()
            .is_some_and(|service_id| self.services.contains_key(service_id))
        {
            return;
        }

        self.selected_service = self.service_order.first().cloned();
    }

    fn offset_selection(&self, delta: isize) -> Option<ServiceId> {
        if self.service_order.is_empty() {
            return None;
        }

        let current_index = self
            .selected_service
            .as_ref()
            .and_then(|selected| {
                self.service_order
                    .iter()
                    .position(|service_id| service_id == selected)
            })
            .unwrap_or_default() as isize;
        let len = self.service_order.len() as isize;
        let next_index = (current_index + delta).rem_euclid(len) as usize;
        self.service_order.get(next_index).cloned()
    }
}

impl ServiceViewState {
    fn from_runtime(
        id: ServiceId,
        name: String,
        log_retention: usize,
        runtime: ServiceRuntime,
    ) -> Self {
        let mut service = Self::placeholder(id);
        service.name = name;
        service.log_retention = log_retention;
        service.lifecycle = runtime.lifecycle;
        service.health = runtime.health;
        service.restart_count = runtime.restart_count;
        service.last_exit_code = runtime.last_exit_code;
        service.last_error = runtime.last_error;
        service.pid = runtime.pid;
        if let Some(snapshot) = runtime.telemetry.latest.as_ref() {
            apply_snapshot(&mut service, snapshot);
        }
        service
    }

    fn placeholder(id: ServiceId) -> Self {
        Self {
            name: id.as_str().to_string(),
            id,
            lifecycle: LifecycleState::Discovered,
            health: ServiceHealth::Unknown,
            last_reason: None,
            restart_count: 0,
            last_exit_code: None,
            last_error: None,
            pid: None,
            uptime: None,
            cpu_millis: 0,
            memory_bytes: 0,
            open_ports: Vec::new(),
            disk_read_bytes: None,
            disk_written_bytes: None,
            network_rx_bytes: None,
            network_tx_bytes: None,
            last_watch_event: None,
            log_retention: DEFAULT_SERVICE_LOG_RETENTION,
            recent_logs: VecDeque::new(),
            lifecycle_history: VecDeque::new(),
        }
    }
}

fn apply_snapshot(service: &mut ServiceViewState, snapshot: &TelemetrySnapshot) {
    service.pid = Some(snapshot.pid);
    service.uptime = snapshot.uptime;
    service.cpu_millis = snapshot.cpu_millis;
    service.memory_bytes = snapshot.memory_bytes;
    service.open_ports = snapshot.open_ports.clone();
    service.disk_read_bytes = snapshot.disk_read_bytes;
    service.disk_written_bytes = snapshot.disk_written_bytes;
    service.network_rx_bytes = snapshot.network_rx_bytes;
    service.network_tx_bytes = snapshot.network_tx_bytes;
}

fn add_optional_bytes(total: Option<u64>, value: Option<u64>) -> Option<u64> {
    match (total, value) {
        (Some(total), Some(value)) => Some(total.saturating_add(value)),
        (Some(total), None) => Some(total),
        (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

struct SanitizedLogMessage {
    message: String,
    changed: bool,
}

fn sanitize_log_message_for_tui(message: &str) -> SanitizedLogMessage {
    let mut sanitized = String::with_capacity(message.len());
    let bytes = message.as_bytes();
    let mut index = 0;
    let mut changed = false;

    while index < bytes.len() {
        match bytes[index] {
            b'\x1b' => {
                if let Some(end) = sgr_ansi_escape_end(bytes, index) {
                    sanitized.push_str(&message[index..end]);
                    index = end;
                } else {
                    changed = true;
                    index = skip_ansi_escape(bytes, index + 1);
                }
            }
            b'\n' => {
                sanitized.push('\n');
                index += 1;
            }
            b'\r' => {
                changed = true;
                sanitized.push('\n');
                index += if index + 1 < bytes.len() && bytes[index + 1] == b'\n' {
                    2
                } else {
                    1
                };
            }
            b'\t' => {
                changed = true;
                sanitized.push_str("    ");
                index += 1;
            }
            byte if byte < 0x20 || byte == 0x7f => {
                changed = true;
                index += 1;
            }
            _ => {
                let Some(character) = message[index..].chars().next() else {
                    break;
                };
                if character.is_control() {
                    changed = true;
                } else {
                    sanitized.push(character);
                }
                index += character.len_utf8();
            }
        }
    }

    SanitizedLogMessage {
        message: sanitized,
        changed,
    }
}

fn sgr_ansi_escape_end(bytes: &[u8], index: usize) -> Option<usize> {
    if bytes.get(index..index + 2) != Some(b"\x1b[") {
        return None;
    }

    let mut cursor = index + 2;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        cursor += 1;
        if (0x40..=0x7e).contains(&byte) {
            return (byte == b'm').then_some(cursor);
        }
    }

    None
}

fn skip_ansi_escape(bytes: &[u8], mut index: usize) -> usize {
    if index >= bytes.len() {
        return index;
    }

    match bytes[index] {
        b'[' => {
            index += 1;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
        }
        b']' => {
            index += 1;
            while index < bytes.len() {
                if bytes[index] == b'\x07' {
                    index += 1;
                    break;
                }
                if bytes[index] == b'\x1b' && index + 1 < bytes.len() && bytes[index + 1] == b'\\' {
                    index += 2;
                    break;
                }
                index += 1;
            }
        }
        b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' => {
            index = index.saturating_add(2).min(bytes.len());
        }
        _ => {
            index += 1;
        }
    }

    index
}

fn push_bounded<T>(items: &mut VecDeque<T>, item: T, max_len: usize) {
    items.push_back(item);
    while items.len() > max_len {
        items.pop_front();
    }
}

fn describe_reason(reason: &StateChangeReason) -> String {
    match reason {
        StateChangeReason::Supervisor => "supervisor".to_string(),
        StateChangeReason::DependencyReady => "dependency ready".to_string(),
        StateChangeReason::DependencyFailed => "dependency failed".to_string(),
        StateChangeReason::BuildCompleted => "build completed".to_string(),
        StateChangeReason::ProcessExited { exit_code } => {
            format!("process exited ({})", format_exit_code(*exit_code))
        }
        StateChangeReason::ProcessCrashed { message } => format!("process crashed: {message}"),
        StateChangeReason::WatchTriggered { path } => match path {
            Some(path) => format!("file changed: {path}"),
            None => "file change detected".to_string(),
        },
        StateChangeReason::Command(command) => format!("command: {:?}", command).to_lowercase(),
    }
}

fn describe_stage(stage: palo_core::events::OrchestrationStage) -> &'static str {
    match stage {
        palo_core::events::OrchestrationStage::Validation => "validation",
        palo_core::events::OrchestrationStage::Check => "check",
        palo_core::events::OrchestrationStage::Build => "build",
        palo_core::events::OrchestrationStage::Start => "start",
        palo_core::events::OrchestrationStage::Runtime => "runtime",
        palo_core::events::OrchestrationStage::Stop => "stop",
        palo_core::events::OrchestrationStage::Restart => "restart",
        palo_core::events::OrchestrationStage::Watch => "watch",
        palo_core::events::OrchestrationStage::DependencyResolution => "dependency resolution",
        palo_core::events::OrchestrationStage::CommandHandling => "command handling",
    }
}

fn format_exit_code(exit_code: Option<i32>) -> String {
    exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_string())
}

fn parse_command(
    input: &str,
    selected_service: Option<ServiceId>,
) -> Result<CommandRequest, String> {
    let normalized = input.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "q" | "quit" | "exit" => Ok(CommandRequest::for_all(CommandKind::Quit)),
        "start all" | "start-all" => Ok(CommandRequest::for_all(CommandKind::Start)),
        "stop all" | "stop-all" => Ok(CommandRequest::for_all(CommandKind::Stop)),
        "restart all" | "restart-all" => Ok(CommandRequest::for_all(CommandKind::Restart)),
        "start" => selected_service
            .map(|service_id| CommandRequest::for_service(service_id, CommandKind::Start))
            .ok_or_else(|| "no service selected for start".to_string()),
        "stop" => selected_service
            .map(|service_id| CommandRequest::for_service(service_id, CommandKind::Stop))
            .ok_or_else(|| "no service selected for stop".to_string()),
        "restart" => selected_service
            .map(|service_id| CommandRequest::for_service(service_id, CommandKind::Restart))
            .ok_or_else(|| "no service selected for restart".to_string()),
        _ => Err(format!("unknown command `{input}`")),
    }
}

fn describe_command(command: CommandKind) -> &'static str {
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
    use palo_core::domain::{
        BuildDefinition, CommandSpec, DEFAULT_SERVICE_LOG_RETENTION, RestartPolicy,
        ServiceDefinition, WatchConfiguration,
    };
    use palo_core::events::{
        CommandOutcome, CommandRequest, CommandStatusEvent, EventPayload, OrchestrationStage,
    };
    use palo_core::telemetry::{ProcessTelemetrySample, TelemetrySnapshot};
    use std::time::Duration;

    fn sample_service(service_id: &str, name: &str) -> ServiceDefinition {
        ServiceDefinition {
            id: ServiceId::new(service_id),
            name: name.to_string(),
            command: CommandSpec::new("cargo").with_args(["run"]),
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
        }
    }

    #[test]
    fn snapshot_adapter_initializes_selection_and_summary() {
        let mut snapshot = CoreAppState::default();
        snapshot.insert_service(sample_service("api", "API"));
        snapshot.insert_service(sample_service("worker", "Worker"));
        snapshot
            .runtime
            .get_mut(&ServiceId::new("api"))
            .unwrap()
            .lifecycle = LifecycleState::Running;
        snapshot
            .runtime
            .get_mut(&ServiceId::new("worker"))
            .unwrap()
            .lifecycle = LifecycleState::Failed;

        let state = TuiState::from_snapshot(snapshot);

        assert_eq!(state.selected_service, Some(ServiceId::new("api")));
        assert_eq!(state.summary.total_services, 2);
        assert_eq!(state.summary.running_services, 1);
        assert_eq!(state.summary.failed_services, 1);
    }

    #[test]
    fn reducer_applies_logs_telemetry_and_errors() {
        let mut snapshot = CoreAppState::default();
        snapshot.insert_service(sample_service("api", "API"));
        let mut state = TuiState::from_snapshot(snapshot);

        state.apply_core_event(CoreEvent {
            emitted_at: std::time::SystemTime::now(),
            payload: EventPayload::LogEmitted(LogEvent::new(
                "api",
                LogOrigin::App,
                LogStream::Stdout,
                "server ready",
            )),
        });

        state.apply_core_event(CoreEvent {
            emitted_at: std::time::SystemTime::now(),
            payload: EventPayload::TelemetryUpdated(TelemetryUpdate::new(
                "api",
                TelemetrySnapshot::from_sample(ProcessTelemetrySample {
                    pid: 42,
                    cpu_millis: 12_000,
                    memory_bytes: 4_096,
                    uptime: Some(Duration::from_secs(5)),
                    open_ports: vec![3000, 3001],
                    disk_read_bytes: Some(1_024),
                    disk_written_bytes: Some(2_048),
                    network_rx_bytes: Some(3_072),
                    network_tx_bytes: Some(4_096),
                }),
            )),
        });

        state.apply_core_event(CoreEvent {
            emitted_at: std::time::SystemTime::now(),
            payload: EventPayload::OrchestrationError(
                OrchestrationErrorEvent::new(OrchestrationStage::Start, "bind failed")
                    .for_service("api"),
            ),
        });

        let api = state.services.get(&ServiceId::new("api")).unwrap();
        assert_eq!(api.recent_logs.len(), 1);
        assert_eq!(api.pid, Some(42));
        assert_eq!(api.cpu_millis, 12_000);
        assert_eq!(api.memory_bytes, 4_096);
        assert_eq!(api.open_ports, vec![3000, 3001]);
        assert_eq!(api.disk_read_bytes, Some(1_024));
        assert_eq!(api.disk_written_bytes, Some(2_048));
        assert_eq!(api.network_rx_bytes, Some(3_072));
        assert_eq!(api.network_tx_bytes, Some(4_096));
        assert_eq!(api.last_error.as_deref(), Some("start: bind failed"));
        assert_eq!(state.summary.aggregate_disk_read_bytes, Some(1_024));
        assert_eq!(state.summary.aggregate_disk_written_bytes, Some(2_048));
        assert_eq!(state.summary.aggregate_network_rx_bytes, Some(3_072));
        assert_eq!(state.summary.aggregate_network_tx_bytes, Some(4_096));
        assert_eq!(state.summary.open_port_count, 2);
        assert_eq!(state.summary.service_error_count, 1);
        assert_eq!(state.resource_history.len(), 2);
        assert_eq!(
            state.resource_history.back(),
            Some(&ResourceHistorySample {
                cpu_millis: 12_000,
                memory_bytes: 4_096,
                disk_bytes: 3_072,
                network_bytes: 7_168,
            })
        );
    }

    #[test]
    fn reducer_keeps_service_configured_log_limit() {
        let mut snapshot = CoreAppState::default();
        let mut service = sample_service("api", "API");
        service.log_retention = 2;
        snapshot.insert_service(service);
        let mut state = TuiState::from_snapshot(snapshot);

        for message in ["one", "two", "three"] {
            state.apply_core_event(CoreEvent {
                emitted_at: std::time::SystemTime::now(),
                payload: EventPayload::LogEmitted(LogEvent::new(
                    "api",
                    LogOrigin::App,
                    LogStream::Stdout,
                    message,
                )),
            });
        }

        let api = state.services.get(&ServiceId::new("api")).unwrap();
        assert_eq!(api.log_retention, 2);
        assert_eq!(api.recent_logs.len(), 2);
        assert_eq!(api.recent_logs[0].message, "two");
        assert_eq!(api.recent_logs[1].message, "three");
    }

    #[test]
    fn placeholder_services_use_default_log_limit() {
        let mut state = TuiState::default();

        state.apply_core_event(CoreEvent {
            emitted_at: std::time::SystemTime::now(),
            payload: EventPayload::LogEmitted(LogEvent::new(
                "late",
                LogOrigin::App,
                LogStream::Stdout,
                "attached after startup",
            )),
        });

        let service = state.services.get(&ServiceId::new("late")).unwrap();
        assert_eq!(service.log_retention, DEFAULT_SERVICE_LOG_RETENTION);
    }

    #[test]
    fn reducer_ignores_palo_internal_logs() {
        let mut snapshot = CoreAppState::default();
        snapshot.insert_service(sample_service("api", "API"));
        let mut state = TuiState::from_snapshot(snapshot);

        state.apply_core_event(CoreEvent {
            emitted_at: std::time::SystemTime::now(),
            payload: EventPayload::LogEmitted(LogEvent::new(
                "api",
                LogOrigin::PaloInternal,
                LogStream::Stdout,
                "building workspace",
            )),
        });

        let api = state.services.get(&ServiceId::new("api")).unwrap();
        assert!(api.recent_logs.is_empty());
    }

    #[test]
    fn reducer_preserves_sgr_colors_while_sanitizing_controls_before_display() {
        let mut snapshot = CoreAppState::default();
        snapshot.insert_service(sample_service("api", "API"));
        let mut state = TuiState::from_snapshot(snapshot);

        state.apply_core_event(CoreEvent {
            emitted_at: std::time::SystemTime::now(),
            payload: EventPayload::LogEmitted(LogEvent::new(
                "api",
                LogOrigin::App,
                LogStream::Stdout,
                "\x1b[31mred\x1b[0m\rnext\tcol\x1b[2K\x07",
            )),
        });

        let api = state.services.get(&ServiceId::new("api")).unwrap();
        let message = &api.recent_logs.back().unwrap().message;
        assert_eq!(message, "\x1b[31mred\x1b[0m\nnext    col");
        assert!(!message.contains('\x07'));
        assert!(!message.contains('\r'));
    }

    #[test]
    fn selection_navigation_wraps_in_service_order() {
        let mut snapshot = CoreAppState::default();
        snapshot.insert_service(sample_service("api", "API"));
        snapshot.insert_service(sample_service("worker", "Worker"));
        let mut state = TuiState::from_snapshot(snapshot);

        state.select_previous_service();
        assert_eq!(state.selected_service, Some(ServiceId::new("worker")));

        state.select_next_service();
        assert_eq!(state.selected_service, Some(ServiceId::new("api")));
    }

    #[test]
    fn direct_selection_by_index_ignores_out_of_bounds() {
        let mut snapshot = CoreAppState::default();
        snapshot.insert_service(sample_service("api", "API"));
        snapshot.insert_service(sample_service("worker", "Worker"));
        let mut state = TuiState::from_snapshot(snapshot);

        state.select_service_at(1);
        assert_eq!(state.selected_service, Some(ServiceId::new("worker")));

        state.select_service_at(4);
        assert_eq!(state.selected_service, Some(ServiceId::new("worker")));
    }

    #[test]
    fn reducer_records_command_feedback_for_service() {
        let mut snapshot = CoreAppState::default();
        snapshot.insert_service(sample_service("api", "API"));
        let mut state = TuiState::from_snapshot(snapshot);

        state.apply_core_event(CoreEvent {
            emitted_at: std::time::SystemTime::now(),
            payload: EventPayload::CommandStatusUpdated(CommandStatusEvent::new(
                CommandRequest::for_service("api", CommandKind::Restart),
                CommandOutcome::Rejected,
                "service is transitioning and cannot be restarted",
            )),
        });

        let api = state.services.get(&ServiceId::new("api")).unwrap();
        assert_eq!(
            state.status_line,
            Some(StatusLine {
                outcome: CommandOutcome::Rejected,
                message: "service is transitioning and cannot be restarted".to_string(),
            })
        );
        assert_eq!(
            api.last_error.as_deref(),
            Some("service is transitioning and cannot be restarted")
        );
        assert_eq!(
            api.lifecycle_history.back().unwrap().reason.as_deref(),
            Some("service is transitioning and cannot be restarted")
        );
    }

    #[test]
    fn reducer_keeps_successful_command_feedback_out_of_service_activity() {
        let mut snapshot = CoreAppState::default();
        snapshot.insert_service(sample_service("api", "API"));
        let mut state = TuiState::from_snapshot(snapshot);

        state.apply_core_event(CoreEvent {
            emitted_at: std::time::SystemTime::now(),
            payload: EventPayload::CommandStatusUpdated(CommandStatusEvent::new(
                CommandRequest::for_service("api", CommandKind::Restart),
                CommandOutcome::Completed,
                "restarted service `api`",
            )),
        });

        let api = state.services.get(&ServiceId::new("api")).unwrap();
        assert_eq!(
            state.status_line,
            Some(StatusLine {
                outcome: CommandOutcome::Completed,
                message: "restarted service `api`".to_string(),
            })
        );
        assert_eq!(api.last_reason, None);
        assert!(api.lifecycle_history.is_empty());
    }

    #[test]
    fn reducer_records_restart_count_from_state_changes() {
        let mut snapshot = CoreAppState::default();
        snapshot.insert_service(sample_service("api", "API"));
        let mut state = TuiState::from_snapshot(snapshot);

        state.apply_core_event(CoreEvent {
            emitted_at: std::time::SystemTime::now(),
            payload: EventPayload::ServiceStateChanged(
                ServiceStateChanged::new(
                    "api",
                    LifecycleState::Running,
                    LifecycleState::Restarting,
                )
                .with_health(ServiceHealth::Healthy)
                .with_restart_count(2)
                .with_reason(StateChangeReason::Command(CommandKind::Restart)),
            ),
        });

        let api = state.services.get(&ServiceId::new("api")).unwrap();
        assert_eq!(api.restart_count, 2);
        assert_eq!(api.last_reason.as_deref(), Some("command: restart"));
    }

    #[test]
    fn reducer_records_watch_trigger_for_lifecycle_pane() {
        let mut snapshot = CoreAppState::default();
        snapshot.insert_service(sample_service("api", "API"));
        let mut state = TuiState::from_snapshot(snapshot);

        state.apply_core_event(CoreEvent {
            emitted_at: std::time::SystemTime::now(),
            payload: EventPayload::ServiceStateChanged(
                ServiceStateChanged::new(
                    "api",
                    LifecycleState::Running,
                    LifecycleState::Restarting,
                )
                .with_reason(StateChangeReason::WatchTriggered {
                    path: Some("src/main.rs".to_string()),
                }),
            ),
        });

        let api = state.services.get(&ServiceId::new("api")).unwrap();
        assert_eq!(
            api.last_watch_event.as_deref(),
            Some("file changed: src/main.rs")
        );
        assert_eq!(
            api.lifecycle_history.back().unwrap().reason.as_deref(),
            Some("file changed: src/main.rs")
        );
    }

    #[test]
    fn command_parser_supports_selected_and_global_commands() {
        let selected = Some(ServiceId::new("api"));

        assert_eq!(
            parse_command("restart", selected.clone()).unwrap(),
            CommandRequest::for_service("api", CommandKind::Restart)
        );
        assert_eq!(
            parse_command("stop all", selected).unwrap(),
            CommandRequest::for_all(CommandKind::Stop)
        );
        assert_eq!(
            parse_command("quit", None).unwrap(),
            CommandRequest::for_all(CommandKind::Quit)
        );
        assert_eq!(
            parse_command("start", None).unwrap_err(),
            "no service selected for start"
        );
    }
}
