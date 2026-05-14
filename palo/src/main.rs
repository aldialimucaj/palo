use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::SystemTime;

use palo::init::{InitOptions, run_init};
use palo::logging::RuntimeLogConfig;
use palo::r#new::{NewOptions, run_new};
use palo::run::{RunOptions, load_config, run_app_with_config};
use palo_tui::logging::init_tui_tracing;
use projects::ProjectKind;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(error) = color_eyre::install() {
        eprintln!("failed to initialize palo error reporting: {error}");
        return ExitCode::FAILURE;
    }

    match parse_cli(env::args().skip(1)) {
        Ok(Command::Init(options)) => {
            init_terminal_tracing();
            match run_init(options) {
                Ok(outcome) => {
                    println!(
                        "Initialized {} config at {} with {} service(s).",
                        outcome.project_kind,
                        outcome.path.display(),
                        outcome.service_count,
                    );
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    error!(error = %error, "palo init failed");
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(Command::New(options)) => {
            init_terminal_tracing();
            match run_new(options) {
                Ok(outcome) => {
                    info!(
                        path = %outcome.path.display(),
                        bytes_written = outcome.bytes_written,
                        overwritten = outcome.overwritten,
                        "created palo template",
                    );
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    error!(error = %error, "palo new failed");
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(Command::Run(options)) => {
            let started_at = SystemTime::now();
            match load_config(&options) {
                Ok(config) => {
                    let logging = RuntimeLogConfig::from_config(&config, started_at);
                    init_run_tracing(&logging);
                    info!(
                        service_count = config.services.len(),
                        "loaded palo runtime configuration",
                    );

                    match run_app_with_config(options, config, logging).await {
                        Ok(()) => ExitCode::SUCCESS,
                        Err(error) => {
                            error!(error = %error, "palo run failed");
                            eprintln!("{error}");
                            ExitCode::FAILURE
                        }
                    }
                }
                Err(error) => {
                    let logging = RuntimeLogConfig::default_for_workspace(
                        &options.workspace_root,
                        started_at,
                    );
                    init_run_tracing(&logging);
                    error!(error = %error, "palo run failed");
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(Command::Help) => {
            print_help();
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            print_help();
            ExitCode::FAILURE
        }
    }
}

fn init_terminal_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,palo=info,projects=info"));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn init_run_tracing(logging: &RuntimeLogConfig) {
    init_tui_tracing(logging.palo_log_path());

    if let Some(run_directory) = logging.run_directory() {
        info!(
            run_log_directory = %run_directory.display(),
            capture_app_logs = logging.app_logs_enabled(),
            "palo run logging initialized",
        );
    } else {
        info!("palo run file logging disabled");
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Init(InitOptions),
    New(NewOptions),
    Run(RunOptions),
    Help,
}

fn parse_cli(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut args = args.into_iter();
    let Some(command) = args.next() else {
        return Ok(Command::Help);
    };

    match command.as_str() {
        "init" => parse_init(args),
        "new" => parse_new(args),
        "run" => parse_run(args),
        "-h" | "--help" | "help" => Ok(Command::Help),
        other => Err(format!("unknown command `{other}`")),
    }
}

fn parse_init(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut project_kind = None;
    let mut overwrite = false;
    let mut args = args.into_iter().peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--overwrite" => overwrite = true,
            "-h" | "--help" => return Ok(Command::Help),
            "--type" => {
                let Some(value) = args.next() else {
                    return Err("`--type` requires a value".to_string());
                };
                project_kind = Some(parse_project_kind(&value)?);
            }
            _ if arg.starts_with("--type=") => {
                let value = arg.trim_start_matches("--type=");
                project_kind = Some(parse_project_kind(value)?);
            }
            other => return Err(format!("unsupported init argument `{other}`")),
        }
    }

    let workspace_root = env::current_dir()
        .map_err(|error| format!("failed to resolve current working directory: {error}"))?;

    Ok(Command::Init(InitOptions {
        workspace_root: PathBuf::from(workspace_root),
        project_kind,
        overwrite,
    }))
}

fn parse_new(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut template = false;
    let mut overwrite = false;

    for arg in args {
        match arg.as_str() {
            "--template" => template = true,
            "--overwrite" => overwrite = true,
            "-h" | "--help" => return Ok(Command::Help),
            other => return Err(format!("unsupported new argument `{other}`")),
        }
    }

    if !template {
        return Err("`palo new` currently requires `--template`".to_string());
    }

    let workspace_root = env::current_dir()
        .map_err(|error| format!("failed to resolve current working directory: {error}"))?;

    Ok(Command::New(NewOptions {
        workspace_root,
        template,
        overwrite,
    }))
}

fn parse_run(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut config_path = None;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--config" => {
                let Some(value) = args.next() else {
                    return Err("`--config` requires a value".to_string());
                };
                config_path = Some(PathBuf::from(value));
            }
            _ if arg.starts_with("--config=") => {
                let value = arg.trim_start_matches("--config=");
                config_path = Some(PathBuf::from(value));
            }
            other => return Err(format!("unsupported run argument `{other}`")),
        }
    }

    let workspace_root = env::current_dir()
        .map_err(|error| format!("failed to resolve current working directory: {error}"))?;

    Ok(Command::Run(RunOptions {
        workspace_root,
        config_path,
    }))
}

fn parse_project_kind(value: &str) -> Result<ProjectKind, String> {
    match value {
        "generic" => Ok(ProjectKind::Generic),
        "rust" => Ok(ProjectKind::Rust),
        other => Err(format!(
            "unsupported project type `{other}`; expected `rust` or `generic`"
        )),
    }
}

fn print_help() {
    println!(
        "Usage:\n  palo init [--type rust|generic] [--overwrite]\n  palo new --template [--overwrite]\n  palo run [--config path]\n\nCommands:\n  init    Generate a palo.yml file for the current workspace\n  new     Create starter assets such as a commented palo.yml template\n  run     Load palo.yml, autostart configured services, and open the Palo dashboard"
    );
}

#[cfg(test)]
mod tests {
    use super::{Command, parse_cli};
    use palo::run::RunOptions;
    use projects::ProjectKind;
    use std::path::PathBuf;

    #[test]
    fn parse_init_command_with_type_and_overwrite() {
        let command = parse_cli([
            "init".to_string(),
            "--type".to_string(),
            "rust".to_string(),
            "--overwrite".to_string(),
        ])
        .expect("cli parse should succeed");

        match command {
            Command::Init(options) => {
                assert_eq!(options.project_kind, Some(ProjectKind::Rust));
                assert!(options.overwrite);
            }
            other => panic!("expected init command, got {other:?}"),
        }
    }

    #[test]
    fn parse_help_without_args() {
        assert_eq!(parse_cli(Vec::new()).unwrap(), Command::Help);
    }

    #[test]
    fn parse_new_template_command() {
        let command = parse_cli([
            "new".to_string(),
            "--template".to_string(),
            "--overwrite".to_string(),
        ])
        .expect("cli parse should succeed");

        match command {
            Command::New(options) => {
                assert!(options.template);
                assert!(options.overwrite);
            }
            other => panic!("expected new command, got {other:?}"),
        }
    }

    #[test]
    fn parse_new_requires_template_flag() {
        let error = parse_cli(["new".to_string()]).expect_err("new mode should be required");

        assert_eq!(error, "`palo new` currently requires `--template`");
    }

    #[test]
    fn parse_run_command_with_explicit_config_path() {
        let command = parse_cli([
            "run".to_string(),
            "--config".to_string(),
            "configs/dev.yml".to_string(),
        ])
        .expect("cli parse should succeed");

        match command {
            Command::Run(RunOptions { config_path, .. }) => {
                assert_eq!(config_path, Some(PathBuf::from("configs/dev.yml")));
            }
            other => panic!("expected run command, got {other:?}"),
        }
    }
}
