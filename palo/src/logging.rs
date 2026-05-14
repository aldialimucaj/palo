use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use palo_core::config::{LogSettings, PaloConfig};
use palo_core::domain::ServiceId;
use palo_core::events::{EventBus, EventPayload, LogEvent, LogOrigin, LogStream};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLogConfig {
    run_directory: Option<PathBuf>,
    palo_log_path: Option<PathBuf>,
    app_logs_enabled: bool,
}

impl RuntimeLogConfig {
    pub fn from_config(config: &PaloConfig, started_at: SystemTime) -> Self {
        Self::from_settings(&config.settings.logs, started_at)
    }

    pub fn default_for_workspace(workspace_root: impl AsRef<Path>, started_at: SystemTime) -> Self {
        let run_directory =
            palo_tui::logging::default_run_log_directory(workspace_root, started_at);
        let palo_log_path = palo_tui::logging::palo_log_path(&run_directory);

        Self {
            run_directory: Some(run_directory),
            palo_log_path: Some(palo_log_path),
            app_logs_enabled: false,
        }
    }

    pub fn disabled() -> Self {
        Self {
            run_directory: None,
            palo_log_path: None,
            app_logs_enabled: false,
        }
    }

    fn from_settings(settings: &LogSettings, started_at: SystemTime) -> Self {
        if !settings.enabled {
            return Self::disabled();
        }

        let run_directory = palo_tui::logging::run_log_directory(&settings.directory, started_at);
        let palo_log_path = settings
            .palo
            .then(|| palo_tui::logging::palo_log_path(&run_directory));
        let app_logs_enabled = settings.apps;
        let run_directory = (palo_log_path.is_some() || app_logs_enabled).then_some(run_directory);

        Self {
            run_directory,
            palo_log_path,
            app_logs_enabled,
        }
    }

    pub fn run_directory(&self) -> Option<&Path> {
        self.run_directory.as_deref()
    }

    pub fn palo_log_path(&self) -> Option<PathBuf> {
        self.palo_log_path.clone()
    }

    pub fn app_logs_enabled(&self) -> bool {
        self.app_logs_enabled
    }
}

pub struct AppLogCollector {
    task: JoinHandle<()>,
    shutdown: CancellationToken,
}

impl AppLogCollector {
    pub fn spawn(events: EventBus, logging: &RuntimeLogConfig) -> Option<Self> {
        if !logging.app_logs_enabled {
            return None;
        }

        let Some(run_directory) = logging.run_directory.clone() else {
            return None;
        };

        let mut receiver = events.subscribe();
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            run_app_log_collector(&mut receiver, run_directory, task_shutdown).await;
        });

        Some(Self { task, shutdown })
    }

    pub async fn shutdown(self) {
        self.shutdown.cancel();
        if let Err(error) = self.task.await {
            warn!(error = %error, "app log file collector task failed during shutdown");
        }
    }
}

async fn run_app_log_collector(
    receiver: &mut tokio::sync::broadcast::Receiver<palo_core::events::Event>,
    run_directory: PathBuf,
    shutdown: CancellationToken,
) {
    let apps_directory = run_directory.join("apps");
    if let Err(error) = fs::create_dir_all(&apps_directory).await {
        warn!(
            path = %apps_directory.display(),
            error = %error,
            "failed to create app log directory",
        );
        return;
    }

    info!(
        path = %apps_directory.display(),
        "capturing app stdout and stderr to log files",
    );

    let mut files = BTreeMap::<ServiceId, File>::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                drain_app_log_events(receiver, &apps_directory, &mut files).await;
                break;
            }
            received = receiver.recv() => {
                match received {
                    Ok(event) => handle_app_log_event(&apps_directory, &mut files, event).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(
                            skipped,
                            "app log file collector skipped lagged events",
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    for (service_id, mut file) in files {
        if let Err(error) = file.flush().await {
            warn!(
                service_id = %service_id,
                error = %error,
                "failed to flush app log file",
            );
        }
    }

    info!("app log file collector stopped");
}

async fn drain_app_log_events(
    receiver: &mut tokio::sync::broadcast::Receiver<palo_core::events::Event>,
    apps_directory: &Path,
    files: &mut BTreeMap<ServiceId, File>,
) {
    loop {
        match receiver.try_recv() {
            Ok(event) => handle_app_log_event(apps_directory, files, event).await,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                warn!(
                    skipped,
                    "app log file collector skipped lagged events during shutdown",
                );
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            | Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
        }
    }
}

async fn handle_app_log_event(
    apps_directory: &Path,
    files: &mut BTreeMap<ServiceId, File>,
    event: palo_core::events::Event,
) {
    if let EventPayload::LogEmitted(log) = event.payload {
        write_app_log_line(apps_directory, files, log).await;
    }
}

async fn write_app_log_line(
    apps_directory: &Path,
    files: &mut BTreeMap<ServiceId, File>,
    log: LogEvent,
) {
    if log.origin != LogOrigin::App {
        return;
    }

    if !files.contains_key(&log.service_id) {
        let path = service_log_path(apps_directory, &log.service_id);
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
        {
            Ok(file) => {
                info!(
                    service_id = %log.service_id,
                    path = %path.display(),
                    "opened app log file",
                );
                files.insert(log.service_id.clone(), file);
            }
            Err(error) => {
                warn!(
                    service_id = %log.service_id,
                    path = %path.display(),
                    error = %error,
                    "failed to open app log file",
                );
                return;
            }
        }
    }

    let line = format!("[{}] {}\n", stream_name(log.stream), log.message);
    let Some(file) = files.get_mut(&log.service_id) else {
        return;
    };

    if let Err(error) = file.write_all(line.as_bytes()).await {
        warn!(
            service_id = %log.service_id,
            error = %error,
            "failed to write app log line",
        );
        files.remove(&log.service_id);
    }
}

fn service_log_path(apps_directory: &Path, service_id: &ServiceId) -> PathBuf {
    apps_directory.join(format!(
        "{}.log",
        sanitize_log_file_component(service_id.as_str())
    ))
}

fn sanitize_log_file_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();

    match sanitized.as_str() {
        "" | "." | ".." => "service".to_string(),
        _ => sanitized,
    }
}

fn stream_name(stream: LogStream) -> &'static str {
    match stream {
        LogStream::Stdout => "stdout",
        LogStream::Stderr => "stderr",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use palo_core::events::{EventPayload, LogEvent};
    use std::time::{Duration, UNIX_EPOCH};
    use tokio::time::{sleep, timeout};

    #[test]
    fn runtime_log_config_uses_timestamped_config_directory() {
        let mut settings = LogSettings::default();
        settings.directory = PathBuf::from("/workspace/.palo/logs");
        let logging =
            RuntimeLogConfig::from_settings(&settings, UNIX_EPOCH + Duration::from_secs(42));

        assert_eq!(
            logging.run_directory(),
            Some(Path::new("/workspace/.palo/logs/42_000"))
        );
        assert_eq!(
            logging.palo_log_path(),
            Some(PathBuf::from("/workspace/.palo/logs/42_000/palo.log"))
        );
        assert!(logging.app_logs_enabled());
    }

    #[test]
    fn disabled_runtime_log_config_skips_all_paths() {
        let settings = LogSettings {
            enabled: false,
            ..LogSettings::default()
        };
        let logging =
            RuntimeLogConfig::from_settings(&settings, UNIX_EPOCH + Duration::from_secs(42));

        assert_eq!(logging.run_directory(), None);
        assert_eq!(logging.palo_log_path(), None);
        assert!(!logging.app_logs_enabled());
    }

    #[test]
    fn service_log_file_names_are_sanitized() {
        assert_eq!(
            sanitize_log_file_component("../api server"),
            ".._api_server"
        );
        assert_eq!(sanitize_log_file_component(""), "service");
    }

    #[tokio::test]
    async fn app_log_collector_writes_app_output_by_service() {
        let tempdir = tempfile::tempdir().expect("tempdir should be created");
        let logging = RuntimeLogConfig {
            run_directory: Some(tempdir.path().to_path_buf()),
            palo_log_path: None,
            app_logs_enabled: true,
        };
        let bus = EventBus::new(16);
        let collector =
            AppLogCollector::spawn(bus.clone(), &logging).expect("collector should start");

        bus.publish(EventPayload::LogEmitted(LogEvent::new(
            "api",
            LogOrigin::App,
            LogStream::Stdout,
            "ready",
        )))
        .expect("event should publish");
        bus.publish(EventPayload::LogEmitted(LogEvent::new(
            "api",
            LogOrigin::PaloInternal,
            LogStream::Stdout,
            "internal",
        )))
        .expect("event should publish");

        let path = tempdir.path().join("apps/api.log");
        timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    if contents == "[stdout] ready\n" {
                        break;
                    }
                }

                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timed out waiting for app log file");

        collector.shutdown().await;
    }
}
