use std::time::Duration;

use palo_core::domain::{
    BuildDefinition, CommandSpec, DEFAULT_SERVICE_LOG_RETENTION, HookDefinition, HookPhase,
    RestartPolicy, ServiceDefinition, ServiceId, WatchConfiguration,
};
use palo_core::events::{EventBus, EventPayload, LogOrigin, LogStream};
use palo_core::execution::ProcessManager;
use std::sync::Arc;
use tokio::runtime::Builder;
use tokio::sync::{Barrier, broadcast::Receiver};
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

async fn collect_messages(
    receiver: &mut Receiver<palo_core::events::Event>,
    expected: usize,
) -> Vec<(LogOrigin, LogStream, String)> {
    let mut messages = Vec::new();

    while messages.len() < expected {
        let event = timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("timed out waiting for event")
            .expect("event bus should remain open");

        if let EventPayload::LogEmitted(log) = event.payload {
            messages.push((log.origin, log.stream, log.message));
        }
    }

    messages
}

fn runtime() -> tokio::runtime::Runtime {
    Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime should build")
}

#[test]
fn startup_pipeline_streams_stdout_and_stderr_events() {
    runtime().block_on(async {
        let bus = EventBus::new(64);
        let mut receiver = bus.subscribe();
        let manager = ProcessManager::new(bus.clone()).with_shutdown_timeout(Duration::from_secs(1));
        let mut service = service_with_command(
            "api",
            r#"printf "run-out\n"; printf "run-err\n" >&2; trap 'exit 0' TERM; while :; do sleep 1; done"#,
        );
        service.build.check =
            Some(CommandSpec::new("sh").with_args(["-c", r#"printf "check-ok\n""#]));
        service.build.build =
            Some(CommandSpec::new("sh").with_args(["-c", r#"printf "build-ok\n""#]));
        service.hooks = vec![
            HookDefinition {
                name: "pre-build".to_string(),
                phase: HookPhase::PreBuild,
                command: CommandSpec::new("sh").with_args(["-c", r#"printf "pre-ok\n""#]),
                required: true,
            },
            HookDefinition {
                name: "post-build".to_string(),
                phase: HookPhase::PostBuild,
                command: CommandSpec::new("sh").with_args(["-c", r#"printf "post-ok\n""#]),
                required: true,
            },
            HookDefinition {
                name: "post-start".to_string(),
                phase: HookPhase::PostStart,
                command: CommandSpec::new("sh").with_args(["-c", r#"printf "ready-ok\n""#]),
                required: true,
            },
        ];

        manager
            .run_startup_pipeline(&service)
            .await
            .expect("startup pipeline should succeed");

        let messages = collect_messages(&mut receiver, 7).await;
        manager
            .stop_service(&service.id)
            .await
            .expect("stop should succeed")
            .expect("service should be active");

        assert!(messages.iter().any(|(origin, _, message)| {
            *origin == LogOrigin::PaloInternal && message == "check-ok"
        }));
        assert!(messages.iter().any(|(origin, _, message)| {
            *origin == LogOrigin::PaloInternal && message == "pre-ok"
        }));
        assert!(messages.iter().any(|(origin, _, message)| {
            *origin == LogOrigin::PaloInternal && message == "build-ok"
        }));
        assert!(messages.iter().any(|(origin, _, message)| {
            *origin == LogOrigin::PaloInternal && message == "post-ok"
        }));
        assert!(messages.iter().any(|(origin, _, message)| {
            *origin == LogOrigin::PaloInternal && message == "ready-ok"
        }));
        assert!(
            messages
                .iter()
                .any(|(origin, stream, message)| {
                    *origin == LogOrigin::App
                        && *stream == LogStream::Stdout
                        && message == "run-out"
                })
        );
        assert!(
            messages
                .iter()
                .any(|(origin, stream, message)| {
                    *origin == LogOrigin::App
                        && *stream == LogStream::Stderr
                        && message == "run-err"
                })
        );
    });
}

#[test]
fn stop_service_gracefully_terminates_child_process() {
    runtime().block_on(async {
        let bus = EventBus::new(32);
        let mut receiver = bus.subscribe();
        let manager =
            ProcessManager::new(bus.clone()).with_shutdown_timeout(Duration::from_secs(1));
        let service = service_with_command(
            "worker",
            r#"printf "worker-ready\n"; while :; do sleep 1; done"#,
        );

        manager
            .spawn_service(&service)
            .await
            .expect("service should spawn");

        let messages = collect_messages(&mut receiver, 1).await;

        let result = manager
            .stop_service(&service.id)
            .await
            .expect("stop should succeed")
            .expect("service should be active");

        assert_eq!(result.service_id, service.id);
        assert!(result.success);
        assert!(messages.iter().any(|(origin, _, message)| {
            *origin == LogOrigin::App && message == "worker-ready"
        }));
    });
}

#[test]
fn spawn_service_passes_configured_environment_to_child_process() {
    runtime().block_on(async {
        let bus = EventBus::new(32);
        let mut receiver = bus.subscribe();
        let manager = ProcessManager::new(bus);
        let mut service = service_with_command("api", r#"printf "token=%s\n" "$PALO_TEST_TOKEN""#);
        service
            .command
            .env
            .insert("PALO_TEST_TOKEN".to_string(), "service-env".to_string());

        manager
            .spawn_service(&service)
            .await
            .expect("service should spawn");

        let messages = collect_messages(&mut receiver, 1).await;

        assert!(messages.iter().any(|(origin, stream, message)| {
            *origin == LogOrigin::App
                && *stream == LogStream::Stdout
                && message == "token=service-env"
        }));
    });
}

#[test]
fn stop_all_cancels_all_running_services() {
    runtime().block_on(async {
        let bus = EventBus::new(32);
        let manager = ProcessManager::new(bus).with_shutdown_timeout(Duration::from_secs(1));
        let api = service_with_command("api", r#"trap 'exit 0' TERM; while :; do sleep 1; done"#);
        let worker =
            service_with_command("worker", r#"trap 'exit 0' TERM; while :; do sleep 1; done"#);

        manager.spawn_service(&api).await.expect("api should spawn");
        manager
            .spawn_service(&worker)
            .await
            .expect("worker should spawn");

        let results = manager.stop_all().await;
        let active = manager.active_services().await;

        assert_eq!(results.len(), 2);
        assert!(
            results
                .into_iter()
                .all(|result| result.expect("stop result").success)
        );
        assert!(active.is_empty());
    });
}

#[test]
fn concurrent_start_attempts_only_spawn_one_service_process() {
    runtime().block_on(async {
        let bus = EventBus::new(32);
        let manager = ProcessManager::new(bus).with_shutdown_timeout(Duration::from_secs(1));
        let service =
            service_with_command("api", r#"trap 'exit 0' TERM; while :; do sleep 1; done"#);
        let barrier = Arc::new(Barrier::new(32));
        let mut tasks = Vec::new();

        for _ in 0..32 {
            let manager = manager.clone();
            let service = service.clone();
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                manager.spawn_service(&service).await
            }));
        }

        let mut started = 0;
        let mut already_running = 0;
        for task in tasks {
            match task.await.expect("spawn task should finish") {
                Ok(_) => started += 1,
                Err(error)
                    if error
                        .to_string()
                        .contains("service process is already running") =>
                {
                    already_running += 1
                }
                Err(error) => panic!("unexpected spawn error: {error}"),
            }
        }

        assert_eq!(started, 1);
        assert_eq!(already_running, 31);
        assert_eq!(manager.active_services().await, vec![service.id.clone()]);

        manager
            .stop_service(&service.id)
            .await
            .expect("stop should succeed")
            .expect("service should be active");
    });
}

#[cfg(unix)]
#[test]
fn stop_service_terminates_children_spawned_by_shell_wrapper() {
    runtime().block_on(async {
        let tempdir = tempfile::tempdir().expect("tempdir should exist");
        let child_pid_path = tempdir.path().join("child-pid");
        let script = format!(
            "sleep 30 >/dev/null 2>&1 & printf \"%s\" \"$!\" > {path}; trap 'exit 0' TERM; while :; do sleep 1; done",
            path = child_pid_path.display()
        );
        let bus = EventBus::new(32);
        let manager = ProcessManager::new(bus).with_shutdown_timeout(Duration::from_secs(1));
        let service = service_with_command("api", &script);

        manager
            .spawn_service(&service)
            .await
            .expect("service should spawn");

        wait_for_file(&child_pid_path).await;
        let child_pid = std::fs::read_to_string(&child_pid_path)
            .expect("child pid should be readable")
            .parse::<i32>()
            .expect("child pid should parse");

        manager
            .stop_service(&service.id)
            .await
            .expect("stop should succeed")
            .expect("service should be active");

        if !wait_for_process_exit(child_pid).await {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(child_pid),
                nix::sys::signal::Signal::SIGKILL,
            );
            panic!("wrapper child process was still running after service stop");
        }
    });
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

#[cfg(unix)]
async fn wait_for_process_exit(pid: i32) -> bool {
    timeout(Duration::from_secs(5), async {
        loop {
            match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
                Ok(()) => sleep(Duration::from_millis(25)).await,
                Err(nix::errno::Errno::ESRCH) => return true,
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}
