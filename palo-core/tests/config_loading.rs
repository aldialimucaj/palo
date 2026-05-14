use std::path::PathBuf;
use std::time::Duration;

use palo_core::config::{PaloConfig, ProjectType, TargetMode};
use palo_core::domain::{
    DependencyCondition, ExpectedStatusRange, HookPhase, RestartPolicy, ServiceId,
};

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
        .join("palo.yml")
}

#[test]
fn loads_valid_config_fixture() {
    let config = PaloConfig::from_path(fixture_path("valid/basic")).expect("fixture should load");
    let api = config
        .services
        .get(&ServiceId::new("api"))
        .expect("api service should exist");

    assert_eq!(
        config.workspace_root,
        fixture_path("valid/basic").parent().unwrap()
    );
    assert_eq!(config.settings.log_retention, Some(2000));
    assert!(config.settings.logs.enabled);
    assert_eq!(
        config.settings.logs.directory,
        fixture_path("valid/basic")
            .parent()
            .unwrap()
            .join(".palo/logs")
    );
    assert!(config.settings.logs.palo);
    assert!(config.settings.logs.apps);
    assert!(config.settings.mcp.enabled);
    assert_eq!(config.settings.mcp.host, "127.0.0.1");
    assert_eq!(config.settings.mcp.port, 9464);
    assert_eq!(config.settings.mcp.path, "/mcp");
    assert_eq!(config.settings.mcp.log_retention, 256);
    assert_eq!(api.project_type, ProjectType::Rust);
    assert_eq!(api.target, TargetMode::Debug);
    assert_eq!(api.autostart, true);
    assert_eq!(api.definition.log_retention, 750);
    assert_eq!(api.definition.command.program, "cargo");
    assert_eq!(api.definition.command.args, vec!["run", "-p", "api"]);
    assert_eq!(
        api.definition
            .command
            .env
            .get("RUST_LOG")
            .map(String::as_str),
        Some("info")
    );
    assert_eq!(api.definition.depends_on, vec![ServiceId::new("db")]);
    assert_eq!(
        api.definition
            .build
            .check
            .as_ref()
            .map(|value| value.program.as_str()),
        Some("cargo")
    );
    assert_eq!(api.definition.hooks.len(), 2);
    assert_eq!(api.definition.watch.enabled, true);
    assert_eq!(api.definition.watch.debounce, Duration::from_millis(500));
    assert_eq!(
        api.definition.watch.ignore_paths,
        vec![fixture_path("valid/basic").parent().unwrap().join(".palo")]
    );
    assert_eq!(
        api.definition.watch.ignore_regex,
        vec!["(^|/)\\.cache/".to_string()]
    );
    assert_eq!(api.definition.restart, RestartPolicy::OnChange);

    let db = config
        .services
        .get(&ServiceId::new("db"))
        .expect("db service should exist");
    assert_eq!(db.definition.log_retention, 2000);
    assert_eq!(db.definition.command.program, "postgres");
    assert_eq!(db.definition.command.args, vec!["-D", "data"]);
    assert_eq!(
        db.executable,
        Some(vec![
            "postgres".to_string(),
            "-D".to_string(),
            "data".to_string()
        ])
    );
}

#[test]
fn services_default_to_500_in_memory_log_lines() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    std::fs::write(
        tempdir.path().join("palo.yml"),
        "services:\n  app:\n    command: [\"echo\", \"hello\"]\n",
    )
    .expect("config should be written");

    let config = PaloConfig::from_workspace(tempdir.path()).expect("config should load");
    let app = config
        .services
        .get(&ServiceId::new("app"))
        .expect("app service should exist");

    assert_eq!(app.definition.log_retention, 500);
}

#[test]
fn loads_ordered_runtime_service_hooks() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    std::fs::write(
        tempdir.path().join("palo.yml"),
        concat!(
            "services:\n",
            "  app:\n",
            "    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
            "    hooks:\n",
            "      pre_start:\n",
            "        - [\"echo\", \"first\"]\n",
            "        - echo second\n",
            "      post_start: echo started\n",
            "      pre_stop:\n",
            "        - echo draining\n",
            "      post_stop:\n",
            "        - echo cleaned\n",
        ),
    )
    .expect("config should be written");

    let config = PaloConfig::from_workspace(tempdir.path()).expect("config should load");
    let app = config
        .services
        .get(&ServiceId::new("app"))
        .expect("app service should exist");
    let hooks = &app.definition.hooks;

    assert_eq!(hooks.len(), 5);
    assert_eq!(hooks[0].phase, HookPhase::PreStart);
    assert_eq!(hooks[0].name, "pre_start-0");
    assert_eq!(hooks[0].command.args, vec!["first"]);
    assert_eq!(hooks[1].phase, HookPhase::PreStart);
    assert_eq!(hooks[1].name, "pre_start-1");
    assert_eq!(hooks[1].command.args, vec!["second"]);
    assert_eq!(hooks[2].phase, HookPhase::PostStart);
    assert_eq!(hooks[3].phase, HookPhase::PreStop);
    assert_eq!(hooks[4].phase, HookPhase::PostStop);
}

#[test]
fn invalid_config_fixture_reports_path_level_errors() {
    let error = PaloConfig::from_path(fixture_path("invalid/bad_restart"))
        .expect_err("fixture should fail validation");
    let rendered = error.to_string();

    assert!(rendered.contains("services.api.restart.backoff"));
    assert!(rendered.contains("services.api.restart.max_crash_retries"));
    assert!(rendered.contains("services.worker.command"));
    assert!(rendered.contains("remediation:"));
}

#[test]
fn invalid_type_fixture_reports_exact_field() {
    let error = PaloConfig::from_path(fixture_path("invalid/bad_type"))
        .expect_err("fixture should fail validation");

    assert_eq!(
        error.errors()[0].path.as_deref(),
        Some("services.worker.type")
    );
}

#[test]
fn mcp_settings_default_to_disabled_loopback_server() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    std::fs::write(
        tempdir.path().join("palo.yml"),
        "services:\n  app:\n    command: [\"echo\", \"hello\"]\n",
    )
    .expect("config should be written");

    let config = PaloConfig::from_workspace(tempdir.path()).expect("config should load");

    assert!(!config.settings.mcp.enabled);
    assert!(config.settings.logs.enabled);
    assert_eq!(
        config.settings.logs.directory,
        tempdir.path().join(".palo/logs")
    );
    assert!(config.settings.logs.palo);
    assert!(config.settings.logs.apps);
    assert_eq!(config.settings.mcp.host, "127.0.0.1");
    assert_eq!(config.settings.mcp.port, 9464);
    assert_eq!(config.settings.mcp.path, "/mcp");
    assert_eq!(
        config.settings.mcp.allowed_hosts,
        vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string()
        ]
    );
}

#[test]
fn log_settings_can_disable_file_capture() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    std::fs::write(
        tempdir.path().join("palo.yml"),
        concat!(
            "palo:\n",
            "  settings:\n",
            "    logs: false\n",
            "services:\n",
            "  app:\n",
            "    command: [\"echo\", \"hello\"]\n",
        ),
    )
    .expect("config should be written");

    let config = PaloConfig::from_workspace(tempdir.path()).expect("config should load");

    assert!(!config.settings.logs.enabled);
    assert_eq!(
        config.settings.logs.directory,
        tempdir.path().join(".palo/logs")
    );
}

#[test]
fn invalid_log_settings_report_exact_path() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    std::fs::write(
        tempdir.path().join("palo.yml"),
        concat!(
            "palo:\n",
            "  settings:\n",
            "    logs:\n",
            "      directory: \"\"\n",
            "services:\n",
            "  app:\n",
            "    command: [\"echo\", \"hello\"]\n",
        ),
    )
    .expect("config should be written");

    let error = PaloConfig::from_workspace(tempdir.path()).expect_err("config should fail");
    let rendered = error.to_string();

    assert!(rendered.contains("palo.settings.logs.directory"));
}

#[test]
fn invalid_mcp_settings_report_exact_paths() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    std::fs::write(
        tempdir.path().join("palo.yml"),
        concat!(
            "palo:\n",
            "  mcp:\n",
            "    enabled: true\n",
            "    host: \"\"\n",
            "    path: mcp\n",
            "    log_retention: 0\n",
            "services:\n",
            "  app:\n",
            "    command: [\"echo\", \"hello\"]\n",
        ),
    )
    .expect("config should be written");

    let error = PaloConfig::from_workspace(tempdir.path()).expect_err("config should fail");
    let rendered = error.to_string();

    assert!(rendered.contains("palo.mcp.host"));
    assert!(rendered.contains("palo.mcp.path"));
    assert!(rendered.contains("palo.mcp.log_retention"));
}

#[test]
fn invalid_log_retention_reports_exact_paths() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    std::fs::write(
        tempdir.path().join("palo.yml"),
        concat!(
            "palo:\n",
            "  settings:\n",
            "    log_retention: 0\n",
            "services:\n",
            "  app:\n",
            "    command: [\"echo\", \"hello\"]\n",
            "    log_retention: 0\n",
        ),
    )
    .expect("config should be written");

    let error = PaloConfig::from_workspace(tempdir.path()).expect_err("config should fail");
    let rendered = error.to_string();

    assert!(rendered.contains("palo.settings.log_retention"));
    assert!(rendered.contains("services.app.log_retention"));
}

#[test]
fn loads_structured_dependencies_and_readiness_checks() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    std::fs::write(
        tempdir.path().join("palo.yml"),
        concat!(
            "services:\n",
            "  db:\n",
            "    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
            "    readiness:\n",
            "      command: [\"sh\", \"-c\", \"exit 0\"]\n",
            "      initial_delay: 10ms\n",
            "      interval: 25ms\n",
            "      timeout: 100ms\n",
            "      retries: 3\n",
            "  cache:\n",
            "    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
            "  api:\n",
            "    command: [\"echo\", \"api\"]\n",
            "    depends_on:\n",
            "      db:\n",
            "        condition: ready\n",
            "        restart: false\n",
            "        timeout: 250ms\n",
            "      cache:\n",
            "        condition: started\n",
            "        required: false\n",
        ),
    )
    .expect("config should be written");

    let config = PaloConfig::from_workspace(tempdir.path()).expect("config should load");
    let db = config
        .services
        .get(&ServiceId::new("db"))
        .expect("db service should exist");
    let readiness = db
        .definition
        .readiness
        .as_ref()
        .expect("readiness should parse");
    assert_eq!(readiness.interval, Duration::from_millis(25));
    assert_eq!(readiness.timeout, Duration::from_millis(100));
    assert_eq!(readiness.retries, 3);

    let api = config
        .services
        .get(&ServiceId::new("api"))
        .expect("api service should exist");
    assert_eq!(
        api.definition.depends_on,
        vec![ServiceId::new("cache"), ServiceId::new("db")]
    );
    let dependencies = &api.definition.dependencies;
    assert_eq!(dependencies.len(), 2);
    assert_eq!(dependencies[0].service_id, ServiceId::new("cache"));
    assert_eq!(dependencies[0].condition, DependencyCondition::Started);
    assert!(!dependencies[0].required);
    assert!(dependencies[0].restart);
    assert_eq!(dependencies[1].service_id, ServiceId::new("db"));
    assert_eq!(dependencies[1].condition, DependencyCondition::Ready);
    assert!(!dependencies[1].restart);
    assert_eq!(dependencies[1].wait_timeout, Duration::from_millis(250));
}

#[test]
fn loads_full_http_healthcheck_config() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    std::fs::write(
        tempdir.path().join("palo.yml"),
        concat!(
            "services:\n",
            "  api:\n",
            "    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
            "    healthcheck:\n",
            "      http:\n",
            "        url: http://127.0.0.1:8080/health\n",
            "        method: HEAD\n",
            "        expected_status: 204..299\n",
            "      initial_delay: 10ms\n",
            "      interval: 25ms\n",
            "      timeout: 100ms\n",
            "      retries: 3\n",
        ),
    )
    .expect("config should be written");

    let config = PaloConfig::from_workspace(tempdir.path()).expect("config should load");
    let healthcheck = config.services[&ServiceId::new("api")]
        .definition
        .healthcheck
        .as_ref()
        .expect("healthcheck should parse");

    assert_eq!(healthcheck.http.url, "http://127.0.0.1:8080/health");
    assert_eq!(healthcheck.http.method, "HEAD");
    assert_eq!(
        healthcheck.http.expected_status,
        ExpectedStatusRange::new(204, 299)
    );
    assert_eq!(healthcheck.initial_delay, Duration::from_millis(10));
    assert_eq!(healthcheck.interval, Duration::from_millis(25));
    assert_eq!(healthcheck.timeout, Duration::from_millis(100));
    assert_eq!(healthcheck.retries, 3);
}

#[test]
fn loads_short_form_http_healthcheck_url_with_defaults() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    std::fs::write(
        tempdir.path().join("palo.yml"),
        concat!(
            "services:\n",
            "  api:\n",
            "    command: [\"echo\", \"api\"]\n",
            "    healthcheck:\n",
            "      url: https://localhost/health\n",
        ),
    )
    .expect("config should be written");

    let config = PaloConfig::from_workspace(tempdir.path()).expect("config should load");
    let healthcheck = config.services[&ServiceId::new("api")]
        .definition
        .healthcheck
        .as_ref()
        .expect("healthcheck should parse");

    assert_eq!(healthcheck.http.url, "https://localhost/health");
    assert_eq!(healthcheck.http.method, "GET");
    assert_eq!(
        healthcheck.http.expected_status,
        ExpectedStatusRange::new(200, 399)
    );
    assert_eq!(healthcheck.interval, Duration::from_secs(1));
    assert_eq!(healthcheck.timeout, Duration::from_secs(5));
    assert_eq!(healthcheck.retries, 30);
}

#[test]
fn rejects_invalid_http_healthcheck_url() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    std::fs::write(
        tempdir.path().join("palo.yml"),
        concat!(
            "services:\n",
            "  api:\n",
            "    command: [\"echo\", \"api\"]\n",
            "    healthcheck:\n",
            "      url: /health\n",
        ),
    )
    .expect("config should be written");

    let error = PaloConfig::from_workspace(tempdir.path()).expect_err("config should fail");
    let rendered = error.to_string();

    assert!(rendered.contains("healthcheck URL must be an absolute HTTP or HTTPS URL"));
    assert!(rendered.contains("services.api.healthcheck.url"));
}

#[test]
fn legacy_readiness_command_still_loads() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    std::fs::write(
        tempdir.path().join("palo.yml"),
        concat!(
            "services:\n",
            "  db:\n",
            "    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
            "    readiness:\n",
            "      command: [\"sh\", \"-c\", \"exit 0\"]\n",
            "      interval: 25ms\n",
            "      timeout: 100ms\n",
            "      retries: 3\n",
        ),
    )
    .expect("config should be written");

    let config = PaloConfig::from_workspace(tempdir.path()).expect("config should load");
    let db = &config.services[&ServiceId::new("db")];

    assert!(db.definition.healthcheck.is_none());
    assert!(db.definition.readiness.is_some());
    assert_eq!(
        db.definition
            .readiness
            .as_ref()
            .expect("readiness should parse")
            .command
            .program,
        "sh"
    );
}

#[test]
fn dependency_graph_errors_are_reported_during_config_loading() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    std::fs::write(
        tempdir.path().join("palo.yml"),
        concat!(
            "services:\n",
            "  api:\n",
            "    command: [\"echo\", \"api\"]\n",
            "    depends_on:\n",
            "      - worker\n",
            "  worker:\n",
            "    command: [\"echo\", \"worker\"]\n",
            "    depends_on:\n",
            "      - api\n",
            "  web:\n",
            "    command: [\"echo\", \"web\"]\n",
            "    depends_on:\n",
            "      - missing\n",
        ),
    )
    .expect("config should be written");

    let error = PaloConfig::from_workspace(tempdir.path()).expect_err("config should fail");
    let rendered = error.to_string();

    assert!(rendered.contains("dependency cycle detected"));
    assert!(rendered.contains("depends on unknown service `missing`"));
    assert!(rendered.contains("services.web.depends_on"));
}
