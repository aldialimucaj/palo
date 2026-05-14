use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use tracing::{debug, info};
use tracing_subscriber::{EnvFilter, fmt::MakeWriter};

pub const PALO_LOG_FILE_ENV: &str = "PALO_LOG_FILE";
pub const DEFAULT_LOG_DIRECTORY: &str = ".palo/logs";

#[derive(Clone, Debug)]
struct AppendLogWriter {
    path: PathBuf,
}

#[derive(Debug)]
struct AppendLogFile {
    path: PathBuf,
    file: Option<File>,
}

impl AppendLogWriter {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl AppendLogFile {
    fn file(&mut self) -> io::Result<&mut File> {
        if self.file.is_none() {
            self.file = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?,
            );
        }

        Ok(self
            .file
            .as_mut()
            .expect("log file should be opened before use"))
    }
}

impl<'a> MakeWriter<'a> for AppendLogWriter {
    type Writer = AppendLogFile;

    fn make_writer(&'a self) -> Self::Writer {
        AppendLogFile {
            path: self.path.clone(),
            file: None,
        }
    }
}

impl Write for AppendLogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.file() {
            Ok(file) => file.write(buf),
            Err(_) => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file() {
            Ok(file) => file.flush(),
            Err(_) => Ok(()),
        }
    }
}

pub fn default_workspace_log_path(workspace_root: impl AsRef<Path>) -> PathBuf {
    palo_log_path(default_run_log_directory(workspace_root, SystemTime::now()))
}

pub fn default_workspace_log_directory(workspace_root: impl AsRef<Path>) -> PathBuf {
    workspace_root.as_ref().join(DEFAULT_LOG_DIRECTORY)
}

pub fn default_run_log_directory(
    workspace_root: impl AsRef<Path>,
    started_at: SystemTime,
) -> PathBuf {
    run_log_directory(default_workspace_log_directory(workspace_root), started_at)
}

pub fn run_log_directory(base_directory: impl AsRef<Path>, started_at: SystemTime) -> PathBuf {
    base_directory
        .as_ref()
        .join(format_run_timestamp(started_at))
}

pub fn palo_log_path(run_directory: impl AsRef<Path>) -> PathBuf {
    run_directory.as_ref().join("palo.log")
}

pub fn format_run_timestamp(started_at: SystemTime) -> String {
    let duration = started_at.duration_since(UNIX_EPOCH).unwrap_or_default();
    format!("{}_{:03}", duration.as_secs(), duration.subsec_millis())
}

pub fn init_tui_tracing(default_log_path: Option<PathBuf>) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,palo=info,palo_core=info,palo_tui=info"));

    if let Some(path) = tui_log_path(default_log_path) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let writer = AppendLogWriter::new(path.clone());
        let initialized = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_ansi(false)
            .with_writer(writer)
            .try_init()
            .is_ok();

        if initialized {
            info!(
                path = %path.display(),
                "palo tui tracing redirected away from terminal"
            );
        }
    } else {
        let initialized = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_ansi(false)
            .with_writer(io::sink)
            .try_init()
            .is_ok();

        if initialized {
            debug!("palo tui tracing discarded because no log file path was available");
        }
    }
}

fn tui_log_path(default_log_path: Option<PathBuf>) -> Option<PathBuf> {
    match std::env::var_os(PALO_LOG_FILE_ENV) {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        Some(_) => None,
        None => default_log_path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_run_log_directory_uses_timestamped_palo_directory() {
        let started_at = UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_042);

        assert_eq!(
            default_run_log_directory("/workspace", started_at),
            PathBuf::from("/workspace/.palo/logs/1700000000_042")
        );
    }

    #[test]
    fn palo_log_path_lives_inside_run_directory() {
        assert_eq!(
            palo_log_path("/workspace/.palo/logs/1700000000_042"),
            PathBuf::from("/workspace/.palo/logs/1700000000_042/palo.log")
        );
    }
}
