use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::time::Duration;

use palo_core::domain::{
    AppState, BuildDefinition, CommandSpec, DEFAULT_SERVICE_LOG_RETENTION, DependencyCondition,
    ExpectedStatusRange, HealthCheck, HookDefinition, HookPhase, HttpHealthProbe, LifecycleState,
    ReadinessCheck, RestartPolicy, ServiceDefinition, ServiceDependency, ServiceHealth, ServiceId,
    WatchConfiguration,
};
use palo_core::events::{
    CommandKind, CommandOutcome, CommandRequest, Event, EventBus, EventPayload,
};
use palo_core::orchestration::Orchestrator;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::runtime::Builder;
use tokio::sync::broadcast::Receiver;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

fn service_with_command(service_id: &str, script: &str) -> ServiceDefinition {
    ServiceDefinition {
        id: ServiceId::new(service_id),
        name: service_id.to_string(),
        command: CommandSpec::new("sh").with_args(["-c", script]),
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

fn app_state(services: Vec<ServiceDefinition>) -> AppState {
    let mut state = AppState::default();
    for service in services {
        state.insert_service(service);
    }
    state
}

fn runtime() -> tokio::runtime::Runtime {
    Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime should build")
}

async fn wait_for_lifecycle(
    orchestrator: &Orchestrator,
    service_id: &str,
    expected: LifecycleState,
) {
    timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = orchestrator.snapshot_state().await;
            let runtime = snapshot
                .runtime
                .get(&ServiceId::new(service_id))
                .expect("runtime should exist");

            if runtime.lifecycle == expected {
                break;
            }

            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("timed out waiting for lifecycle state");
}

async fn wait_for_health(orchestrator: &Orchestrator, service_id: &str, expected: ServiceHealth) {
    timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = orchestrator.snapshot_state().await;
            let runtime = snapshot
                .runtime
                .get(&ServiceId::new(service_id))
                .expect("runtime should exist");

            if runtime.health == expected {
                break;
            }

            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("timed out waiting for health state");
}

async fn wait_for_file(path: &std::path::Path) {
    timeout(Duration::from_secs(5), async {
        loop {
            if path.exists() {
                break;
            }

            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("timed out waiting for file");
}

async fn wait_for_file_contents(path: &std::path::Path, expected: &str) {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(contents) = fs::read_to_string(path) {
                if contents == expected {
                    break;
                }
            }

            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("timed out waiting for file contents");
}

fn counted_long_running_script(path: &std::path::Path) -> String {
    format!(
        "count=$(cat '{path}' 2>/dev/null || printf 0); count=$((count + 1)); printf \"%s\" \"$count\" > '{path}'; trap 'exit 0' TERM; while :; do sleep 1; done",
        path = path.display()
    )
}

fn append_token_command(path: &std::path::Path, token: &str) -> CommandSpec {
    CommandSpec::new("sh").with_args([
        "-c".to_string(),
        format!("printf '{token}\\n' >> '{}'", path.display()),
    ])
}

async fn spawn_test_http_server(
    status: Arc<AtomicU16>,
    request_count: Arc<AtomicUsize>,
) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test HTTP listener should bind");
    let addr = listener
        .local_addr()
        .expect("listener address should exist");
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let status = status.clone();
            let request_count = request_count.clone();
            tokio::spawn(async move {
                let mut buffer = [0_u8; 1024];
                let _ = socket.read(&mut buffer).await;
                request_count.fetch_add(1, Ordering::SeqCst);
                let code = status.load(Ordering::SeqCst);
                let reason = if (200..400).contains(&code) {
                    "OK"
                } else {
                    "ERROR"
                };
                let response = format!(
                    "HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });

    (format!("http://{addr}/health"), handle)
}

fn http_healthcheck(url: String, interval: Duration, retries: u32) -> HealthCheck {
    HealthCheck {
        http: HttpHealthProbe {
            url,
            method: "GET".to_string(),
            expected_status: ExpectedStatusRange::new(200, 399),
        },
        initial_delay: Duration::ZERO,
        interval,
        timeout: Duration::from_millis(250),
        retries,
    }
}

async fn collect_stopped_services(receiver: &mut Receiver<Event>, expected: usize) -> Vec<String> {
    let mut stopped = Vec::new();

    while stopped.len() < expected {
        let event = timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("timed out waiting for stop event")
            .expect("event bus should remain open");

        if let EventPayload::ServiceStateChanged(change) = event.payload {
            if change.current == LifecycleState::Stopped {
                stopped.push(change.service_id.to_string());
            }
        }
    }

    stopped
}

async fn wait_for_telemetry(
    receiver: &mut Receiver<Event>,
    service_id: &str,
) -> palo_core::telemetry::TelemetrySnapshot {
    timeout(Duration::from_secs(5), async {
        loop {
            let event = receiver.recv().await.expect("event bus should remain open");
            if let EventPayload::TelemetryUpdated(update) = event.payload {
                if update.service_id == ServiceId::new(service_id) {
                    return update.snapshot;
                }
            }
        }
    })
    .await
    .expect("timed out waiting for telemetry event")
}

async fn wait_for_command_outcome(
    receiver: &mut Receiver<Event>,
    expected_command: CommandKind,
    expected_outcome: CommandOutcome,
) -> String {
    timeout(Duration::from_secs(5), async {
        loop {
            let event = receiver.recv().await.expect("event bus should remain open");
            if let EventPayload::CommandStatusUpdated(status) = event.payload {
                if status.request.command == expected_command && status.outcome == expected_outcome
                {
                    return status.message;
                }
            }
        }
    })
    .await
    .expect("timed out waiting for command status event")
}

#[test]
fn starts_dependencies_first_and_stops_dependents_first() {
    runtime().block_on(async {
        let bus = EventBus::new(128);
        let mut receiver = bus.subscribe();
        let db = service_with_command("db", r#"trap 'exit 0' TERM; while :; do sleep 1; done"#);
        let mut api =
            service_with_command("api", r#"trap 'exit 0' TERM; while :; do sleep 1; done"#);
        api.depends_on = vec![ServiceId::new("db")];

        let orchestrator =
            Orchestrator::with_event_bus(app_state(vec![api.clone(), db.clone()]), bus);

        orchestrator
            .start_service(&ServiceId::new("api"))
            .await
            .expect("startup should succeed");

        wait_for_lifecycle(&orchestrator, "db", LifecycleState::Running).await;
        wait_for_lifecycle(&orchestrator, "api", LifecycleState::Running).await;

        let snapshot = orchestrator.snapshot_state().await;
        assert_eq!(
            snapshot.runtime[&ServiceId::new("db")].lifecycle,
            LifecycleState::Running
        );
        assert_eq!(
            snapshot.runtime[&ServiceId::new("api")].lifecycle,
            LifecycleState::Running
        );

        orchestrator
            .stop_service(&ServiceId::new("db"))
            .await
            .expect("stop should succeed");

        let stopped = collect_stopped_services(&mut receiver, 2).await;
        assert_eq!(stopped, vec!["api".to_string(), "db".to_string()]);
    });
}

#[test]
fn runtime_hooks_run_in_phase_order_around_service_lifecycle() {
    runtime().block_on(async {
        let tempdir = tempfile::tempdir().expect("tempdir should exist");
        let hook_log = tempdir.path().join("hook-order");
        let app_script = format!(
            "grep -qx pre-start-1 '{path}' || exit 17; grep -qx pre-start-2 '{path}' || exit 18; while ! grep -qx post-start-2 '{path}' 2>/dev/null; do sleep 0.01; done; printf 'app-start\\n' >> '{path}'; trap \"printf 'app-stop\\n' >> '{path}'; exit 0\" TERM; while :; do sleep 1; done",
            path = hook_log.display()
        );
        let mut service = service_with_command("api", &app_script);
        service.hooks = vec![
            HookDefinition {
                name: "pre-start-1".to_string(),
                phase: HookPhase::PreStart,
                command: append_token_command(&hook_log, "pre-start-1"),
                required: true,
            },
            HookDefinition {
                name: "pre-start-2".to_string(),
                phase: HookPhase::PreStart,
                command: append_token_command(&hook_log, "pre-start-2"),
                required: true,
            },
            HookDefinition {
                name: "post-start-1".to_string(),
                phase: HookPhase::PostStart,
                command: append_token_command(&hook_log, "post-start-1"),
                required: true,
            },
            HookDefinition {
                name: "post-start-2".to_string(),
                phase: HookPhase::PostStart,
                command: append_token_command(&hook_log, "post-start-2"),
                required: true,
            },
            HookDefinition {
                name: "pre-stop-1".to_string(),
                phase: HookPhase::PreStop,
                command: append_token_command(&hook_log, "pre-stop-1"),
                required: true,
            },
            HookDefinition {
                name: "pre-stop-2".to_string(),
                phase: HookPhase::PreStop,
                command: append_token_command(&hook_log, "pre-stop-2"),
                required: true,
            },
            HookDefinition {
                name: "post-stop-1".to_string(),
                phase: HookPhase::PostStop,
                command: append_token_command(&hook_log, "post-stop-1"),
                required: true,
            },
            HookDefinition {
                name: "post-stop-2".to_string(),
                phase: HookPhase::PostStop,
                command: append_token_command(&hook_log, "post-stop-2"),
                required: true,
            },
        ];

        let orchestrator = Orchestrator::new(app_state(vec![service]));
        orchestrator
            .start_service(&ServiceId::new("api"))
            .await
            .expect("startup should succeed");
        wait_for_lifecycle(&orchestrator, "api", LifecycleState::Running).await;
        wait_for_file_contents(
            &hook_log,
            "pre-start-1\npre-start-2\npost-start-1\npost-start-2\napp-start\n",
        )
        .await;

        orchestrator
            .stop_service(&ServiceId::new("api"))
            .await
            .expect("stop should succeed");

        wait_for_file_contents(
            &hook_log,
            "pre-start-1\npre-start-2\npost-start-1\npost-start-2\napp-start\npre-stop-1\npre-stop-2\napp-stop\npost-stop-1\npost-stop-2\n",
        )
        .await;
    });
}

#[test]
fn readiness_check_blocks_dependents_until_dependency_is_ready() {
    runtime().block_on(async {
        let tempdir = tempfile::tempdir().expect("tempdir should exist");
        let ready_path = tempdir.path().join("db-ready");
        let probe_count_path = tempdir.path().join("probe-count");
        let api_start_path = tempdir.path().join("api-started");

        let db_script = format!(
            "(sleep 0.2; touch '{ready}') & trap 'exit 0' TERM; while :; do sleep 1; done",
            ready = ready_path.display()
        );
        let readiness_script = format!(
            "count=$(cat '{count}' 2>/dev/null || printf 0); count=$((count + 1)); printf \"%s\" \"$count\" > '{count}'; test -f '{ready}'",
            count = probe_count_path.display(),
            ready = ready_path.display()
        );

        let mut db = service_with_command("db", &db_script);
        db.readiness = Some(ReadinessCheck {
            command: CommandSpec::new("sh").with_args(["-c".to_string(), readiness_script]),
            initial_delay: Duration::ZERO,
            interval: Duration::from_millis(50),
            timeout: Duration::from_secs(1),
            retries: 10,
        });

        let api_script = format!(
            "printf started > '{}'; trap 'exit 0' TERM; while :; do sleep 1; done",
            api_start_path.display()
        );
        let mut api = service_with_command("api", &api_script);
        api.depends_on = vec![ServiceId::new("db")];

        let orchestrator = Orchestrator::new(app_state(vec![api, db]));
        orchestrator
            .start_service(&ServiceId::new("api"))
            .await
            .expect("startup should wait for readiness");

        wait_for_lifecycle(&orchestrator, "db", LifecycleState::Running).await;
        wait_for_lifecycle(&orchestrator, "api", LifecycleState::Running).await;
        wait_for_file_contents(&api_start_path, "started").await;

        let probe_count = fs::read_to_string(probe_count_path).expect("probe count should exist");
        let probes = probe_count
            .parse::<u32>()
            .expect("probe count should be numeric");
        assert!(probes > 1, "readiness should retry before success");
        let _ = orchestrator.stop_all().await;
    });
}

#[test]
fn http_healthcheck_blocks_dependents_until_success() {
    runtime().block_on(async {
        let tempdir = tempfile::tempdir().expect("tempdir should exist");
        let api_start_path = tempdir.path().join("api-started");
        let status = Arc::new(AtomicU16::new(503));
        let request_count = Arc::new(AtomicUsize::new(0));
        let (url, server) = spawn_test_http_server(status.clone(), request_count.clone()).await;

        let mut db = service_with_command("db", r#"trap 'exit 0' TERM; while :; do sleep 1; done"#);
        db.healthcheck = Some(http_healthcheck(url, Duration::from_millis(40), 10));

        let api_script = format!(
            "printf started > '{}'; trap 'exit 0' TERM; while :; do sleep 1; done",
            api_start_path.display()
        );
        let mut api = service_with_command("api", &api_script);
        api.depends_on = vec![ServiceId::new("db")];

        let orchestrator = Orchestrator::new(app_state(vec![api, db]));
        let starter = {
            let orchestrator = orchestrator.clone();
            tokio::spawn(async move { orchestrator.start_service(&ServiceId::new("api")).await })
        };

        timeout(Duration::from_secs(5), async {
            while request_count.load(Ordering::SeqCst) < 2 {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("health endpoint should be probed before success");
        assert!(!api_start_path.exists());

        status.store(200, Ordering::SeqCst);
        starter
            .await
            .expect("startup task should complete")
            .expect("startup should wait for HTTP health");

        wait_for_lifecycle(&orchestrator, "db", LifecycleState::Running).await;
        wait_for_health(&orchestrator, "db", ServiceHealth::Healthy).await;
        wait_for_file_contents(&api_start_path, "started").await;
        assert!(
            request_count.load(Ordering::SeqCst) >= 2,
            "HTTP health should retry before success"
        );

        let _ = orchestrator.stop_all().await;
        server.abort();
    });
}

#[test]
fn http_health_monitor_marks_degraded_unhealthy_and_restores_healthy() {
    runtime().block_on(async {
        let status = Arc::new(AtomicU16::new(200));
        let request_count = Arc::new(AtomicUsize::new(0));
        let (url, server) = spawn_test_http_server(status.clone(), request_count.clone()).await;

        let mut api =
            service_with_command("api", r#"trap 'exit 0' TERM; while :; do sleep 1; done"#);
        api.healthcheck = Some(http_healthcheck(url, Duration::from_millis(35), 2));

        let orchestrator = Orchestrator::new(app_state(vec![api]));
        orchestrator
            .start_service(&ServiceId::new("api"))
            .await
            .expect("startup should succeed");
        wait_for_health(&orchestrator, "api", ServiceHealth::Healthy).await;

        status.store(500, Ordering::SeqCst);
        wait_for_health(&orchestrator, "api", ServiceHealth::Degraded).await;
        wait_for_health(&orchestrator, "api", ServiceHealth::Unhealthy).await;

        status.store(200, Ordering::SeqCst);
        wait_for_health(&orchestrator, "api", ServiceHealth::Healthy).await;

        let _ = orchestrator.stop_all().await;
        server.abort();
    });
}

#[test]
fn stopping_service_stops_http_health_monitor() {
    runtime().block_on(async {
        let status = Arc::new(AtomicU16::new(200));
        let request_count = Arc::new(AtomicUsize::new(0));
        let (url, server) = spawn_test_http_server(status.clone(), request_count.clone()).await;

        let mut api =
            service_with_command("api", r#"trap 'exit 0' TERM; while :; do sleep 1; done"#);
        api.healthcheck = Some(http_healthcheck(url, Duration::from_millis(25), 2));

        let orchestrator = Orchestrator::new(app_state(vec![api]));
        orchestrator
            .start_service(&ServiceId::new("api"))
            .await
            .expect("startup should succeed");
        wait_for_health(&orchestrator, "api", ServiceHealth::Healthy).await;

        timeout(Duration::from_secs(5), async {
            while request_count.load(Ordering::SeqCst) < 3 {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("monitor should run while service is active");

        orchestrator
            .stop_service(&ServiceId::new("api"))
            .await
            .expect("stop should succeed");
        wait_for_lifecycle(&orchestrator, "api", LifecycleState::Stopped).await;

        let after_stop = request_count.load(Ordering::SeqCst);
        sleep(Duration::from_millis(120)).await;
        assert_eq!(
            request_count.load(Ordering::SeqCst),
            after_stop,
            "health monitor should stop probing after service stop"
        );

        server.abort();
    });
}

#[test]
fn restarting_dependency_restarts_active_dependents() {
    runtime().block_on(async {
        let tempdir = tempfile::tempdir().expect("tempdir should exist");
        let db_count_path = tempdir.path().join("db-count");
        let api_count_path = tempdir.path().join("api-count");

        let db_script = counted_long_running_script(&db_count_path);
        let api_script = counted_long_running_script(&api_count_path);
        let db = service_with_command("db", &db_script);
        let mut api = service_with_command("api", &api_script);
        api.depends_on = vec![ServiceId::new("db")];

        let orchestrator = Orchestrator::new(app_state(vec![api, db]));
        orchestrator
            .start_service(&ServiceId::new("api"))
            .await
            .expect("startup should succeed");
        wait_for_file_contents(&db_count_path, "1").await;
        wait_for_file_contents(&api_count_path, "1").await;

        orchestrator
            .restart_service(&ServiceId::new("db"))
            .await
            .expect("dependency restart should succeed");

        wait_for_file_contents(&db_count_path, "2").await;
        wait_for_file_contents(&api_count_path, "2").await;
        let snapshot = orchestrator.snapshot_state().await;
        assert_eq!(
            snapshot
                .runtime
                .get(&ServiceId::new("db"))
                .map(|runtime| runtime.restart_count),
            Some(1)
        );
        assert_eq!(
            snapshot
                .runtime
                .get(&ServiceId::new("api"))
                .map(|runtime| runtime.restart_count),
            Some(1)
        );
        let _ = orchestrator.stop_all().await;
    });
}

#[test]
fn restart_propagation_can_be_disabled_per_dependency() {
    runtime().block_on(async {
        let tempdir = tempfile::tempdir().expect("tempdir should exist");
        let db_count_path = tempdir.path().join("db-count");
        let api_count_path = tempdir.path().join("api-count");

        let db_script = counted_long_running_script(&db_count_path);
        let api_script = counted_long_running_script(&api_count_path);
        let db = service_with_command("db", &db_script);
        let mut api = service_with_command("api", &api_script);
        api.depends_on = vec![ServiceId::new("db")];
        api.dependencies = vec![ServiceDependency {
            service_id: ServiceId::new("db"),
            condition: DependencyCondition::Ready,
            restart: false,
            required: true,
            wait_timeout: Duration::from_secs(1),
        }];

        let orchestrator = Orchestrator::new(app_state(vec![api, db]));
        orchestrator
            .start_service(&ServiceId::new("api"))
            .await
            .expect("startup should succeed");
        wait_for_file_contents(&db_count_path, "1").await;
        wait_for_file_contents(&api_count_path, "1").await;

        orchestrator
            .restart_service(&ServiceId::new("db"))
            .await
            .expect("dependency restart should succeed");

        wait_for_file_contents(&db_count_path, "2").await;
        let api_runs = fs::read_to_string(api_count_path).expect("api count should exist");
        assert_eq!(api_runs, "1");
        let _ = orchestrator.stop_all().await;
    });
}

#[test]
fn reports_unknown_dependencies_clearly() {
    runtime().block_on(async {
        let bus = EventBus::new(32);
        let mut api =
            service_with_command("api", r#"trap 'exit 0' TERM; while :; do sleep 1; done"#);
        api.depends_on = vec![ServiceId::new("db")];

        let orchestrator = Orchestrator::with_event_bus(app_state(vec![api]), bus);
        let error = orchestrator
            .start_service(&ServiceId::new("api"))
            .await
            .expect_err("startup should fail");

        assert!(
            error
                .to_string()
                .contains("depends on unknown service `db`")
        );
    });
}

#[test]
fn restarts_crashing_service_until_retry_limit_is_reached() {
    runtime().block_on(async {
        let tempdir = tempfile::tempdir().expect("tempdir should exist");
        let counter_path = tempdir.path().join("crash-count");
        let script = format!(
            "count=$(cat {path} 2>/dev/null || printf 0); count=$((count + 1)); printf \"%s\" \"$count\" > {path}; exit 1",
            path = counter_path.display()
        );

        let mut worker = service_with_command("worker", &script);
        worker.restart = RestartPolicy::OnCrash {
            max_retries: Some(2),
            backoff: Duration::from_millis(50),
        };

        let orchestrator = Orchestrator::new(app_state(vec![worker]));

        orchestrator
            .start_service(&ServiceId::new("worker"))
            .await
            .expect("initial start should succeed");

        wait_for_lifecycle(&orchestrator, "worker", LifecycleState::Failed).await;

        let runs = fs::read_to_string(counter_path).expect("counter file should exist");
        assert_eq!(runs, "3");
    });
}

#[test]
fn watch_restart_honors_debounce_for_on_change_services() {
    runtime().block_on(async {
        let tempdir = tempfile::tempdir().expect("tempdir should exist");
        let counter_path = tempdir.path().join("watch-count");
        let script = format!(
            "count=$(cat {path} 2>/dev/null || printf 0); count=$((count + 1)); printf \"%s\" \"$count\" > {path}; trap 'exit 0' TERM; while :; do sleep 1; done",
            path = counter_path.display()
        );

        let mut worker = service_with_command("worker", &script);
        worker.restart = RestartPolicy::OnChange;
        worker.watch = WatchConfiguration {
            enabled: true,
            paths: vec![tempdir.path().to_path_buf()],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            ignore_paths: Vec::new(),
            ignore_regex: Vec::new(),
            debounce: Duration::from_secs(5),
        };

        let orchestrator = Orchestrator::new(app_state(vec![worker]));
        orchestrator
            .start_service(&ServiceId::new("worker"))
            .await
            .expect("initial start should succeed");
        wait_for_lifecycle(&orchestrator, "worker", LifecycleState::Running).await;
        wait_for_file(&counter_path).await;

        let first = orchestrator
            .trigger_watch_restart(&ServiceId::new("worker"), Some("src/main.rs".to_string()))
            .await
            .expect("first restart should succeed");
        let second = orchestrator
            .trigger_watch_restart(&ServiceId::new("worker"), Some("src/lib.rs".to_string()))
            .await
            .expect("debounced restart should not fail");

        wait_for_lifecycle(&orchestrator, "worker", LifecycleState::Running).await;
        wait_for_file_contents(&counter_path, "2").await;

        let runs = fs::read_to_string(counter_path).expect("counter file should exist");
        assert!(first);
        assert!(!second);
        assert_eq!(runs, "2");
        let _ = orchestrator.stop_all().await;
    });
}

#[test]
fn watch_restart_is_ignored_for_mcp_controlled_services() {
    runtime().block_on(async {
        let mut worker =
            service_with_command("worker", "trap 'exit 0' TERM; while :; do sleep 1; done");
        worker.restart = RestartPolicy::Mcp;
        worker.watch = WatchConfiguration {
            enabled: true,
            paths: Vec::new(),
            include: Vec::new(),
            exclude: Vec::new(),
            ignore_paths: Vec::new(),
            ignore_regex: Vec::new(),
            debounce: Duration::from_millis(1),
        };

        let orchestrator = Orchestrator::new(app_state(vec![worker]));
        orchestrator
            .start_service(&ServiceId::new("worker"))
            .await
            .expect("initial start should succeed");
        wait_for_lifecycle(&orchestrator, "worker", LifecycleState::Running).await;

        let restarted = orchestrator
            .trigger_watch_restart(&ServiceId::new("worker"), Some("src/main.rs".to_string()))
            .await
            .expect("mcp-controlled watch event should not fail");

        assert!(!restarted);
        let state = orchestrator.snapshot_state().await;
        let runtime = state
            .runtime
            .get(&ServiceId::new("worker"))
            .expect("runtime should exist");
        assert_eq!(runtime.lifecycle, LifecycleState::Running);

        orchestrator
            .stop_service(&ServiceId::new("worker"))
            .await
            .expect("stop should succeed");
    });
}

#[test]
fn command_handler_routes_start_and_stop_requests() {
    runtime().block_on(async {
        let bus = EventBus::new(128);
        let mut receiver = bus.subscribe();
        let service =
            service_with_command("api", r#"trap 'exit 0' TERM; while :; do sleep 1; done"#);
        let orchestrator = Orchestrator::with_event_bus(app_state(vec![service]), bus);
        orchestrator.spawn_command_handler();

        orchestrator
            .events()
            .publish(EventPayload::CommandRequested(CommandRequest::for_service(
                "api",
                CommandKind::Start,
            )))
            .expect("command should be published");

        let start_message =
            wait_for_command_outcome(&mut receiver, CommandKind::Start, CommandOutcome::Completed)
                .await;
        assert_eq!(start_message, "started service `api`");
        wait_for_lifecycle(&orchestrator, "api", LifecycleState::Running).await;

        orchestrator
            .events()
            .publish(EventPayload::CommandRequested(CommandRequest::for_service(
                "api",
                CommandKind::Stop,
            )))
            .expect("command should be published");

        let stop_message =
            wait_for_command_outcome(&mut receiver, CommandKind::Stop, CommandOutcome::Completed)
                .await;
        assert_eq!(stop_message, "stopped service `api`");
        wait_for_lifecycle(&orchestrator, "api", LifecycleState::Stopped).await;
    });
}

#[test]
fn standalone_build_runs_build_pipeline_without_starting_service() {
    runtime().block_on(async {
        let tempdir = tempfile::tempdir().expect("tempdir should exist");
        let marker_path = tempdir.path().join("build-marker");
        let check_script = format!("printf check >> {}", marker_path.display());
        let build_script = format!("printf build >> {}", marker_path.display());

        let mut service = service_with_command("api", "exit 99");
        service.build = BuildDefinition {
            check: Some(CommandSpec::new("sh").with_args(["-c".to_string(), check_script])),
            build: Some(CommandSpec::new("sh").with_args(["-c".to_string(), build_script])),
            hooks: Vec::new(),
        };
        let orchestrator = Orchestrator::new(app_state(vec![service]));

        let commands_run = orchestrator
            .build_service(&ServiceId::new("api"))
            .await
            .expect("standalone build should succeed");

        let marker = fs::read_to_string(marker_path).expect("build marker should exist");
        let snapshot = orchestrator.snapshot_state().await;
        let runtime = snapshot
            .runtime
            .get(&ServiceId::new("api"))
            .expect("runtime should exist");

        assert_eq!(commands_run, 2);
        assert_eq!(marker, "checkbuild");
        assert_eq!(runtime.lifecycle, LifecycleState::Built);
        assert_eq!(runtime.pid, None);
    });
}

#[test]
fn command_handler_rejects_restart_during_transition() {
    runtime().block_on(async {
        let bus = EventBus::new(128);
        let mut receiver = bus.subscribe();
        let service =
            service_with_command("api", r#"trap 'exit 0' TERM; while :; do sleep 1; done"#);
        let mut state = app_state(vec![service]);
        state
            .runtime
            .get_mut(&ServiceId::new("api"))
            .unwrap()
            .lifecycle = LifecycleState::Restarting;
        let orchestrator = Orchestrator::with_event_bus(state, bus);
        orchestrator.spawn_command_handler();

        orchestrator
            .events()
            .publish(EventPayload::CommandRequested(CommandRequest::for_service(
                "api",
                CommandKind::Restart,
            )))
            .expect("command should be published");

        let message = wait_for_command_outcome(
            &mut receiver,
            CommandKind::Restart,
            CommandOutcome::Rejected,
        )
        .await;
        assert!(
            message.contains("service is transitioning and cannot be restarted"),
            "unexpected rejection message: {message}"
        );
    });
}

#[test]
fn publishes_telemetry_updates_and_persists_recent_snapshots() {
    runtime().block_on(async {
        let bus = EventBus::new(64);
        let mut receiver = bus.subscribe();
        let worker =
            service_with_command("worker", r#"trap 'exit 0' TERM; while :; do sleep 1; done"#);

        let orchestrator = Orchestrator::with_event_bus_and_telemetry_interval(
            app_state(vec![worker]),
            bus,
            Duration::from_millis(100),
        );

        orchestrator
            .start_service(&ServiceId::new("worker"))
            .await
            .expect("service should start");
        wait_for_lifecycle(&orchestrator, "worker", LifecycleState::Running).await;

        let snapshot = wait_for_telemetry(&mut receiver, "worker").await;
        let state = orchestrator.snapshot_state().await;
        let runtime = state
            .runtime
            .get(&ServiceId::new("worker"))
            .expect("runtime should exist");

        assert_eq!(runtime.pid, Some(snapshot.pid));
        assert_eq!(
            runtime.telemetry.latest.as_ref().map(|latest| latest.pid),
            Some(snapshot.pid)
        );
        assert!(
            !runtime.telemetry.recent.is_empty(),
            "telemetry history should contain at least one snapshot"
        );

        orchestrator
            .stop_service(&ServiceId::new("worker"))
            .await
            .expect("stop should succeed");
    });
}
