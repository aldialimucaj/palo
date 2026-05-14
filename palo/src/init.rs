use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use palo_core::config::DEFAULT_CONFIG_FILE_NAME;
use palo_core::domain::DEFAULT_SERVICE_LOG_RETENTION;
use palo_core::error::DiscoveryError;
use palo_projects::{
    DiscoveredCommand, DiscoveredProject, ProjectKind, adapter_for_kind, detect_project_kind,
};
use tracing::{debug, info, warn};

const DEFAULT_WATCH_DEBOUNCE: &str = "250ms";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitOptions {
    pub workspace_root: PathBuf,
    pub project_kind: Option<ProjectKind>,
    pub overwrite: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitOutcome {
    pub path: PathBuf,
    pub project_kind: ProjectKind,
    pub service_count: usize,
    pub overwritten: bool,
}

#[derive(Debug)]
pub enum InitError {
    ConfigAlreadyExists { path: PathBuf },
    Io(std::io::Error),
    Discovery(DiscoveryError),
    InvalidDiscoveredProject { kind: ProjectKind, message: String },
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConfigAlreadyExists { path } => write!(
                f,
                "refusing to overwrite existing config `{}`; rerun with `palo init --overwrite`",
                path.display()
            ),
            Self::Io(error) => write!(f, "failed to write palo config: {error}"),
            Self::Discovery(error) => error.fmt(f),
            Self::InvalidDiscoveredProject { kind, message } => {
                write!(f, "cannot generate {kind} config: {message}")
            }
        }
    }
}

impl std::error::Error for InitError {}

impl From<std::io::Error> for InitError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<DiscoveryError> for InitError {
    fn from(value: DiscoveryError) -> Self {
        Self::Discovery(value)
    }
}

pub fn run_init(options: InitOptions) -> Result<InitOutcome, InitError> {
    let config_path = options.workspace_root.join(DEFAULT_CONFIG_FILE_NAME);
    if config_path.exists() && !options.overwrite {
        warn!(path = %config_path.display(), "refusing to overwrite existing palo config");
        return Err(InitError::ConfigAlreadyExists { path: config_path });
    }

    let project_kind = match options.project_kind {
        Some(kind) => kind,
        None => detect_project_kind(&options.workspace_root)?.unwrap_or(ProjectKind::Generic),
    };

    info!(
        workspace_root = %options.workspace_root.display(),
        project_kind = %project_kind,
        overwrite = options.overwrite,
        "initializing palo workspace",
    );

    let discovered = adapter_for_kind(project_kind).discover(&options.workspace_root)?;
    validate_project_for_init(&discovered)?;
    debug!(
        project_kind = %project_kind,
        service_count = discovered.services.len(),
        issue_count = discovered.issues.len(),
        "discovery completed for palo init",
    );

    let rendered = render_config(&options.workspace_root, &discovered)?;
    fs::write(&config_path, rendered)?;

    info!(
        path = %config_path.display(),
        project_kind = %project_kind,
        service_count = discovered.services.len(),
        "wrote palo initialization config",
    );

    Ok(InitOutcome {
        path: config_path,
        project_kind,
        service_count: discovered.services.len(),
        overwritten: options.overwrite,
    })
}

fn validate_project_for_init(project: &DiscoveredProject) -> Result<(), InitError> {
    if !project.services.is_empty() || project.kind == ProjectKind::Generic {
        return Ok(());
    }

    let message = if project.issues.is_empty() {
        "no runnable services were discovered".to_string()
    } else {
        project
            .issues
            .iter()
            .map(|issue| issue.message.as_str())
            .collect::<Vec<_>>()
            .join("; ")
    };

    warn!(
        project_kind = %project.kind,
        issue_count = project.issues.len(),
        "init discovery did not produce runnable services",
    );
    Err(InitError::InvalidDiscoveredProject {
        kind: project.kind,
        message,
    })
}

fn render_config(workspace_root: &Path, project: &DiscoveredProject) -> Result<String, InitError> {
    let rendered = match project.kind {
        ProjectKind::Generic => render_generic_config(),
        ProjectKind::Rust => render_rust_config(workspace_root, project),
    };

    Ok(render_config_yaml(&rendered))
}

fn render_generic_config() -> RenderedConfig {
    let mut services = BTreeMap::new();
    services.insert(
        "app".to_string(),
        RenderedService {
            project_type: ProjectKind::Generic.to_string(),
            autostart: false,
            target: None,
            executable: None,
            command: Some(vec!["./run-local-service".to_string()]),
            working_dir: ".".to_string(),
            log_retention: DEFAULT_SERVICE_LOG_RETENTION,
            build: None,
            restart: Some(RenderedRestart {
                on: "manual".to_string(),
                debounce: None,
            }),
            watch: None,
            depends_on: Vec::new(),
        },
    );

    RenderedConfig { services }
}

fn render_rust_config(workspace_root: &Path, project: &DiscoveredProject) -> RenderedConfig {
    let mut services = BTreeMap::new();

    for service in &project.services {
        services.insert(
            service.id.clone(),
            RenderedService {
                project_type: ProjectKind::Rust.to_string(),
                autostart: true,
                target: Some("debug".to_string()),
                executable: Some(vec![path_for_yaml(
                    workspace_root,
                    &service.executable.debug_path,
                )]),
                command: None,
                working_dir: ".".to_string(),
                log_retention: DEFAULT_SERVICE_LOG_RETENTION,
                build: Some(RenderedBuild {
                    check: command_for_yaml(&service.check),
                    cmd: command_for_yaml(&service.build),
                }),
                restart: Some(RenderedRestart {
                    on: "change".to_string(),
                    debounce: Some(DEFAULT_WATCH_DEBOUNCE.to_string()),
                }),
                watch: Some(RenderedWatch {
                    paths: service
                        .watch_paths
                        .iter()
                        .map(|path| path_for_yaml(workspace_root, path))
                        .collect(),
                    ignore_paths: vec![".palo".to_string(), "target".to_string()],
                    ignore_regex: Vec::new(),
                }),
                depends_on: Vec::new(),
            },
        );
    }

    RenderedConfig { services }
}

fn command_for_yaml(command: &DiscoveredCommand) -> Vec<String> {
    std::iter::once(command.program.clone())
        .chain(command.args.iter().cloned())
        .collect()
}

fn path_for_yaml(workspace_root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(workspace_root).ok();
    normalize_path_string(relative.unwrap_or(path))
}

fn normalize_path_string(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    if raw.is_empty() { ".".to_string() } else { raw }
}

fn render_config_yaml(config: &RenderedConfig) -> String {
    let mut output = String::new();
    output.push_str("palo:\n");
    output.push_str("  settings:\n");
    writeln!(output, "    log_retention: {DEFAULT_SERVICE_LOG_RETENTION}")
        .expect("write to string");
    output.push_str("    logs:\n");
    output.push_str("      enabled: true\n");
    output.push_str("      directory: .palo/logs\n");
    output.push_str("      palo: true\n");
    output.push_str("      apps: true\n");
    output.push_str("  mcp:\n");
    output.push_str("    enabled: false\n");
    output.push_str("    host: 127.0.0.1\n");
    output.push_str("    port: 9464\n");
    output.push_str("    path: /mcp\n\n");
    output.push_str("services:\n");

    for (service_id, service) in &config.services {
        write_key(&mut output, 2, service_id);
        output.push('\n');
        write_field(&mut output, 4, "type", &service.project_type);
        write_bool_field(&mut output, 4, "autostart", service.autostart);
        if let Some(target) = &service.target {
            write_field(&mut output, 4, "target", target);
        }
        if let Some(executable) = &service.executable {
            write_flow_sequence_field(&mut output, 4, "executable", executable);
        }
        if let Some(command) = &service.command {
            write_flow_sequence_field(&mut output, 4, "command", command);
        }
        write_field(&mut output, 4, "working_dir", &service.working_dir);
        write_usize_field(&mut output, 4, "log_retention", service.log_retention);
        if let Some(build) = &service.build {
            output.push_str("    build:\n");
            write_flow_sequence_field(&mut output, 6, "check", &build.check);
            write_flow_sequence_field(&mut output, 6, "cmd", &build.cmd);
        }
        if let Some(restart) = &service.restart {
            output.push_str("    restart:\n");
            write_field(&mut output, 6, "on", &restart.on);
            if let Some(debounce) = &restart.debounce {
                write_field(&mut output, 6, "debounce", debounce);
            }
        }
        if let Some(watch) = &service.watch {
            output.push_str("    watch:\n");
            output.push_str("      paths:\n");
            for path in &watch.paths {
                writeln!(output, "        - {}", yaml_scalar(path)).expect("write to string");
            }
            if !watch.ignore_paths.is_empty() {
                output.push_str("      ignore_paths:\n");
                for path in &watch.ignore_paths {
                    writeln!(output, "        - {}", yaml_scalar(path)).expect("write to string");
                }
            }
            if !watch.ignore_regex.is_empty() {
                output.push_str("      ignore_regex:\n");
                for pattern in &watch.ignore_regex {
                    writeln!(output, "        - {}", yaml_scalar(pattern))
                        .expect("write to string");
                }
            }
        }
        if !service.depends_on.is_empty() {
            output.push_str("    depends_on:\n");
            for dependency in &service.depends_on {
                writeln!(output, "      - {}", yaml_scalar(dependency)).expect("write to string");
            }
        }
    }

    output
}

fn write_key(output: &mut String, indent: usize, key: &str) {
    write!(output, "{}{}:", " ".repeat(indent), yaml_key(key)).expect("write to string");
}

fn write_field(output: &mut String, indent: usize, key: &str, value: &str) {
    writeln!(
        output,
        "{}{}: {}",
        " ".repeat(indent),
        key,
        yaml_scalar(value)
    )
    .expect("write to string");
}

fn write_bool_field(output: &mut String, indent: usize, key: &str, value: bool) {
    writeln!(output, "{}{}: {}", " ".repeat(indent), key, value).expect("write to string");
}

fn write_usize_field(output: &mut String, indent: usize, key: &str, value: usize) {
    writeln!(output, "{}{}: {}", " ".repeat(indent), key, value).expect("write to string");
}

fn write_flow_sequence_field(output: &mut String, indent: usize, key: &str, values: &[String]) {
    writeln!(
        output,
        "{}{}: [{}]",
        " ".repeat(indent),
        key,
        values
            .iter()
            .map(|value| yaml_double_quoted(value))
            .collect::<Vec<_>>()
            .join(", ")
    )
    .expect("write to string");
}

fn yaml_key(value: &str) -> String {
    if is_plain_yaml_key(value) {
        value.to_string()
    } else {
        yaml_double_quoted(value)
    }
}

fn yaml_scalar(value: &str) -> String {
    if is_plain_yaml_scalar(value) {
        value.to_string()
    } else {
        yaml_double_quoted(value)
    }
}

fn is_plain_yaml_key(value: &str) -> bool {
    !value.is_empty()
        && !is_reserved_yaml_scalar(value)
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

fn is_plain_yaml_scalar(value: &str) -> bool {
    !value.is_empty()
        && !is_reserved_yaml_scalar(value)
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | '/')
        })
}

fn is_reserved_yaml_scalar(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "false" | "null" | "~" | "yes" | "no" | "on" | "off"
    )
}

fn yaml_double_quoted(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            '\0' => quoted.push_str("\\0"),
            character if character.is_control() => {
                write!(quoted, "\\u{:04X}", character as u32).expect("write to string");
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

#[derive(Debug)]
struct RenderedConfig {
    services: BTreeMap<String, RenderedService>,
}

#[derive(Debug)]
struct RenderedService {
    project_type: String,
    autostart: bool,
    target: Option<String>,
    executable: Option<Vec<String>>,
    command: Option<Vec<String>>,
    working_dir: String,
    log_retention: usize,
    build: Option<RenderedBuild>,
    restart: Option<RenderedRestart>,
    watch: Option<RenderedWatch>,
    depends_on: Vec<String>,
}

#[derive(Debug)]
struct RenderedBuild {
    check: Vec<String>,
    cmd: Vec<String>,
}

#[derive(Debug)]
struct RenderedRestart {
    on: String,
    debounce: Option<String>,
}

#[derive(Debug)]
struct RenderedWatch {
    paths: Vec<String>,
    ignore_paths: Vec<String>,
    ignore_regex: Vec<String>,
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use palo_core::config::PaloConfig;
    use palo_core::domain::{LifecycleState, ServiceId};
    use palo_core::orchestration::Orchestrator;
    use palo_projects::ProjectKind;
    use tempfile::TempDir;
    use tokio::runtime::Builder;
    use tokio::time::{sleep, timeout};

    use super::{InitError, InitOptions, run_init};

    fn fixture_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("palo-projects")
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn temp_workspace_from_fixture(name: &str) -> TempDir {
        let source = fixture_path(name);
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        copy_dir(&source, tempdir.path());
        tempdir
    }

    fn copy_dir(source: &Path, destination: &Path) {
        fs::create_dir_all(destination).expect("destination dir should exist");
        for entry in fs::read_dir(source).expect("source dir should be readable") {
            let entry = entry.expect("dir entry should read");
            let source_path = entry.path();
            let destination_path = destination.join(entry.file_name());
            let file_type = entry.file_type().expect("file type should read");

            if file_type.is_dir() {
                copy_dir(&source_path, &destination_path);
            } else {
                fs::copy(&source_path, &destination_path).expect("file should copy");
            }
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime should build")
    }

    #[test]
    fn rust_init_generates_valid_config_from_discovery() {
        let tempdir = temp_workspace_from_fixture("workspace");
        let outcome = run_init(InitOptions {
            workspace_root: tempdir.path().to_path_buf(),
            project_kind: Some(ProjectKind::Rust),
            overwrite: false,
        })
        .expect("rust init should succeed");

        assert_eq!(outcome.project_kind, ProjectKind::Rust);
        assert_eq!(outcome.service_count, 2);

        let rendered = fs::read_to_string(tempdir.path().join("palo.yml"))
            .expect("generated palo.yml should be readable");
        assert!(rendered.contains("type: rust"));
        assert!(rendered.contains("worker-daemon"));
        assert!(rendered.contains(&format!(
            r#"executable: ["target/debug/api{}"]"#,
            std::env::consts::EXE_SUFFIX
        )));
        assert!(
            rendered.contains(r#"check: ["cargo", "check", "--package", "api", "--bin", "api"]"#)
        );
        assert!(
            rendered.contains(r#"cmd: ["cargo", "build", "--package", "api", "--bin", "api"]"#)
        );
        assert!(rendered.contains("log_retention: 500"));

        let loaded = PaloConfig::from_workspace(tempdir.path())
            .expect("generated rust config should load through core schema");
        assert_eq!(loaded.services.len(), 2);
        assert_eq!(loaded.settings.log_retention, Some(500));
        assert_eq!(
            loaded.services[&ServiceId::new("api")]
                .definition
                .log_retention,
            500
        );
    }

    #[test]
    fn auto_detection_prefers_rust_when_cargo_manifest_exists() {
        let tempdir = temp_workspace_from_fixture("single_crate");
        let outcome = run_init(InitOptions {
            workspace_root: tempdir.path().to_path_buf(),
            project_kind: None,
            overwrite: false,
        })
        .expect("auto init should detect rust");

        assert_eq!(outcome.project_kind, ProjectKind::Rust);
    }

    #[test]
    fn auto_detection_falls_back_to_generic_template() {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let outcome = run_init(InitOptions {
            workspace_root: tempdir.path().to_path_buf(),
            project_kind: None,
            overwrite: false,
        })
        .expect("generic fallback should succeed");

        assert_eq!(outcome.project_kind, ProjectKind::Generic);

        let loaded = PaloConfig::from_workspace(tempdir.path())
            .expect("generated generic config should load");
        assert_eq!(loaded.services.len(), 1);
        assert!(
            loaded
                .services
                .contains_key(&palo_core::domain::ServiceId::new("app"))
        );
    }

    #[test]
    fn init_refuses_to_overwrite_existing_config_without_flag() {
        let tempdir = temp_workspace_from_fixture("single_crate");
        let config_path = tempdir.path().join("palo.yml");
        fs::write(&config_path, "services: {}\n").expect("seed config should write");

        let error = run_init(InitOptions {
            workspace_root: tempdir.path().to_path_buf(),
            project_kind: Some(ProjectKind::Rust),
            overwrite: false,
        })
        .expect_err("init should refuse overwrite");

        match error {
            InitError::ConfigAlreadyExists { path } => assert_eq!(path, config_path),
            other => panic!("expected overwrite error, got {other:?}"),
        }
    }

    #[test]
    fn overwrite_flag_replaces_existing_config() {
        let tempdir = temp_workspace_from_fixture("single_crate");
        let config_path = tempdir.path().join("palo.yml");
        fs::write(&config_path, "services: {}\n").expect("seed config should write");

        let outcome = run_init(InitOptions {
            workspace_root: tempdir.path().to_path_buf(),
            project_kind: Some(ProjectKind::Rust),
            overwrite: true,
        })
        .expect("overwrite init should succeed");

        assert!(outcome.overwritten);
        let rendered = fs::read_to_string(config_path).expect("config should be readable");
        assert!(rendered.contains("single-app"));
        assert!(!rendered.contains("services: {}"));
    }

    #[test]
    fn rust_init_generated_config_can_boot_a_service_end_to_end() {
        runtime().block_on(async {
            let tempdir = tempfile::tempdir().expect("tempdir should be created");
            fs::write(
                tempdir.path().join("Cargo.toml"),
                r#"
[package]
name = "smoke-app"
version = "0.5.5"
edition = "2024"
"#,
            )
            .expect("manifest should write");

            fs::create_dir_all(tempdir.path().join("src")).expect("src dir should be created");
            fs::write(
                tempdir.path().join("src/main.rs"),
                r#"
use std::thread;
use std::time::Duration;

fn main() {
    println!("smoke-app started");
    loop {
        thread::sleep(Duration::from_millis(50));
    }
}
"#,
            )
            .expect("main should write");

            let outcome = run_init(InitOptions {
                workspace_root: tempdir.path().to_path_buf(),
                project_kind: Some(ProjectKind::Rust),
                overwrite: false,
            })
            .expect("rust init should succeed");
            assert_eq!(outcome.service_count, 1);

            let config =
                PaloConfig::from_workspace(tempdir.path()).expect("generated config should load");
            let orchestrator = Orchestrator::new(config.into_app_state());

            orchestrator
                .start_service(&ServiceId::new("smoke-app"))
                .await
                .expect("service should start");

            timeout(Duration::from_secs(30), async {
                loop {
                    let snapshot = orchestrator.snapshot_state().await;
                    let runtime = snapshot
                        .runtime
                        .get(&ServiceId::new("smoke-app"))
                        .expect("runtime should exist");
                    if runtime.lifecycle == LifecycleState::Running {
                        break;
                    }

                    sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .expect("service should reach running state");

            let snapshot = orchestrator.snapshot_state().await;
            let runtime = snapshot
                .runtime
                .get(&ServiceId::new("smoke-app"))
                .expect("runtime should exist");
            assert_eq!(runtime.lifecycle, LifecycleState::Running);
            assert!(
                tempdir
                    .path()
                    .join("target")
                    .join("debug")
                    .join(format!("smoke-app{}", std::env::consts::EXE_SUFFIX))
                    .exists()
            );

            orchestrator
                .stop_service(&ServiceId::new("smoke-app"))
                .await
                .expect("service should stop cleanly");
        });
    }
}
