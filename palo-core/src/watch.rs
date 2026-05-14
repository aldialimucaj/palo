use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use globset::{Glob, GlobSet, GlobSetBuilder};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use regex::Regex;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::domain::{ServiceDefinition, ServiceId, WatchConfiguration};
use crate::error::{PaloError, WatchError};
use crate::orchestration::Orchestrator;

#[derive(Clone, Default)]
pub struct WatchRegistry {
    tasks: Arc<Mutex<BTreeMap<ServiceId, WatchTask>>>,
}

struct WatchTask {
    _watcher: RecommendedWatcher,
    shutdown: Sender<()>,
    task: thread::JoinHandle<()>,
}

impl WatchRegistry {
    pub async fn register(
        &self,
        service: &ServiceDefinition,
        orchestrator: Orchestrator,
    ) -> Result<bool, PaloError> {
        if !service.watch.enabled
            || !matches!(service.restart, crate::domain::RestartPolicy::OnChange)
        {
            debug!(service_id = %service.id, "skipping watch registration because service is not on-change");
            self.unregister(&service.id).await;
            return Ok(false);
        }

        if service.watch.paths.is_empty() {
            warn!(service_id = %service.id, "watch is enabled but no paths are configured");
            self.unregister(&service.id).await;
            return Ok(false);
        }

        let matcher = ServiceWatcher::new(service.id.clone(), service.watch.clone())?;
        let service_id = service.id.clone();
        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |result| {
            let _ = tx.send(result);
        })
        .map_err(|error| {
            WatchError::new(format!("failed to create watcher backend: {error}"))
                .for_service(service_id.clone())
        })?;

        for path in &matcher.config.paths {
            watcher
                .watch(path, RecursiveMode::Recursive)
                .map_err(|error| {
                    WatchError::new(format!("failed to watch path: {error}"))
                        .for_service(service.id.clone())
                        .with_path(path.clone())
                })?;
        }

        let task_service_id = service.id.clone();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let runtime = Handle::current();
        let task = thread::spawn(move || {
            info!(
                service_id = %task_service_id,
                watch_path_count = matcher.config.paths.len(),
                include_rule_count = matcher.config.include.len(),
                exclude_rule_count = matcher.config.exclude.len(),
                ignore_path_count = matcher.config.ignore_paths.len(),
                ignore_regex_count = matcher.config.ignore_regex.len(),
                "watching service files for changes"
            );

            loop {
                if shutdown_rx.try_recv().is_ok() {
                    break;
                }

                let result = match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(result) => result,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                };

                match result {
                    Ok(event) => {
                        if !is_actionable_event(&event.kind) {
                            continue;
                        }

                        let matched_paths = matcher.match_event_paths(&event.paths);
                        let Some(changed_path) = matched_paths.first() else {
                            continue;
                        };

                        debug!(
                            service_id = %task_service_id,
                            path = %changed_path.display(),
                            matched_path_count = matched_paths.len(),
                            "watch event matched service scope"
                        );

                        if let Err(error) = runtime.block_on(orchestrator.trigger_watch_restart(
                            &task_service_id,
                            Some(changed_path.display().to_string()),
                        )) {
                            warn!(
                                service_id = %task_service_id,
                                error = %error,
                                "watch-triggered restart failed"
                            );
                        }
                    }
                    Err(error) => {
                        let watch_error = PaloError::Watch(
                            WatchError::new(format!("watch backend event error: {error}"))
                                .for_service(task_service_id.clone()),
                        );
                        orchestrator.publish_runtime_error(&watch_error);
                    }
                }
            }
        });

        self.unregister(&service.id).await;
        self.tasks.lock().await.insert(
            service.id.clone(),
            WatchTask {
                _watcher: watcher,
                shutdown: shutdown_tx,
                task,
            },
        );

        Ok(true)
    }

    pub async fn unregister(&self, service_id: &ServiceId) {
        if let Some(task) = self.tasks.lock().await.remove(service_id) {
            info!(service_id = %service_id, "stopping service file watcher");
            let _ = task.shutdown.send(());
            let current_thread = thread::current().id();
            let watcher_thread = task.task.thread().id();
            if current_thread != watcher_thread {
                let _ = task.task.join();
            }
        }
    }
}

#[derive(Clone)]
pub struct ServiceWatcher {
    service_id: ServiceId,
    config: WatchConfiguration,
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
    ignore_regex: Vec<Regex>,
}

impl ServiceWatcher {
    pub fn new(service_id: ServiceId, config: WatchConfiguration) -> Result<Self, PaloError> {
        let include = compile_globs(&service_id, "include", &config.include)?;
        let exclude = compile_globs(&service_id, "exclude", &config.exclude)?;
        let ignore_regex = compile_regexes(&service_id, &config.ignore_regex)?;

        Ok(Self {
            service_id,
            config,
            include,
            exclude,
            ignore_regex,
        })
    }

    pub fn match_event_paths(&self, paths: &[PathBuf]) -> Vec<PathBuf> {
        let mut matches = Vec::new();
        for path in paths {
            let Some(matched) = self.match_path(path) else {
                continue;
            };

            if matches.iter().any(|existing| existing == &matched) {
                continue;
            }

            matches.push(matched);
        }
        matches
    }

    pub fn match_path(&self, path: &Path) -> Option<PathBuf> {
        let normalized = normalize_path(path);
        let relative = self
            .config
            .paths
            .iter()
            .find_map(|root| relative_to_root(root, &normalized));
        let Some(relative) = relative else {
            debug!(
                service_id = %self.service_id,
                path = %path.display(),
                "watch path ignored because it is outside configured watch roots"
            );
            return None;
        };

        if self.is_ignored_path(&normalized, &relative) {
            debug!(
                service_id = %self.service_id,
                path = %path.display(),
                "watch path ignored by explicit path rule"
            );
            return None;
        }

        let candidate = relative.as_path();
        if let Some(exclude) = &self.exclude {
            if exclude.is_match(candidate) {
                debug!(
                    service_id = %self.service_id,
                    path = %path.display(),
                    "watch path excluded by glob rule"
                );
                return None;
            }
        }

        let candidate_text = path_to_pattern_text(candidate);
        if self
            .ignore_regex
            .iter()
            .any(|pattern| pattern.is_match(&candidate_text))
        {
            debug!(
                service_id = %self.service_id,
                path = %path.display(),
                "watch path ignored by regex rule"
            );
            return None;
        }

        if let Some(include) = &self.include {
            if !include.is_match(candidate) {
                return None;
            }
        }

        Some(relative)
    }

    fn is_ignored_path(&self, absolute_path: &Path, relative_path: &Path) -> bool {
        self.config.ignore_paths.iter().any(|ignored| {
            if ignored.is_absolute() {
                let ignored = normalize_path(ignored);
                absolute_path == ignored || absolute_path.starts_with(&ignored)
            } else {
                relative_path == ignored || relative_path.starts_with(ignored)
            }
        })
    }
}

fn compile_globs(
    service_id: &ServiceId,
    kind: &str,
    patterns: &[String],
) -> Result<Option<GlobSet>, PaloError> {
    if patterns.is_empty() {
        return Ok(None);
    }

    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|error| {
            PaloError::Watch(
                WatchError::new(format!("invalid {kind} glob `{pattern}`: {error}"))
                    .for_service(service_id.clone()),
            )
        })?;
        builder.add(glob);
    }

    builder.build().map(Some).map_err(|error| {
        PaloError::Watch(
            WatchError::new(format!("failed to build {kind} glob set: {error}"))
                .for_service(service_id.clone()),
        )
    })
}

fn compile_regexes(service_id: &ServiceId, patterns: &[String]) -> Result<Vec<Regex>, PaloError> {
    let mut regexes = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let regex = Regex::new(pattern).map_err(|error| {
            PaloError::Watch(
                WatchError::new(format!("invalid ignore regex `{pattern}`: {error}"))
                    .for_service(service_id.clone()),
            )
        })?;
        regexes.push(regex);
    }

    Ok(regexes)
}

fn is_actionable_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    )
}

fn normalize_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn relative_to_root(root: &Path, path: &Path) -> Option<PathBuf> {
    let normalized_root = normalize_path(root);
    path.strip_prefix(&normalized_root).ok().map(PathBuf::from)
}

fn path_to_pattern_text(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch_config(root: &Path) -> WatchConfiguration {
        WatchConfiguration {
            enabled: true,
            paths: vec![root.to_path_buf()],
            include: Vec::new(),
            exclude: Vec::new(),
            ignore_paths: Vec::new(),
            ignore_regex: Vec::new(),
            debounce: Duration::from_millis(2500),
        }
    }

    #[test]
    fn matches_paths_inside_watch_root_without_globs() {
        let root = PathBuf::from("/tmp/workspace");
        let watcher =
            ServiceWatcher::new(ServiceId::new("api"), watch_config(&root)).expect("watcher");

        let matched = watcher.match_path(&root.join("src/main.rs"));
        assert_eq!(matched, Some(PathBuf::from("src/main.rs")));
    }

    #[test]
    fn include_globs_limit_matches() {
        let root = PathBuf::from("/tmp/workspace");
        let mut config = watch_config(&root);
        config.include = vec!["src/**/*.rs".to_string()];
        let watcher =
            ServiceWatcher::new(ServiceId::new("api"), config).expect("watcher should build");

        assert_eq!(
            watcher.match_path(&root.join("src/main.rs")),
            Some(PathBuf::from("src/main.rs"))
        );
        assert_eq!(watcher.match_path(&root.join("README.md")), None);
    }

    #[test]
    fn exclude_globs_override_includes() {
        let root = PathBuf::from("/tmp/workspace");
        let mut config = watch_config(&root);
        config.include = vec!["src/**".to_string()];
        config.exclude = vec!["src/generated/**".to_string()];
        let watcher =
            ServiceWatcher::new(ServiceId::new("api"), config).expect("watcher should build");

        assert_eq!(
            watcher.match_path(&root.join("src/lib.rs")),
            Some(PathBuf::from("src/lib.rs"))
        );
        assert_eq!(
            watcher.match_path(&root.join("src/generated/code.rs")),
            None
        );
    }

    #[test]
    fn explicit_ignored_paths_match_files_and_directory_descendants() {
        let root = PathBuf::from("/tmp/workspace");
        let mut config = watch_config(&root);
        config.ignore_paths = vec![PathBuf::from("src/generated"), root.join("README.md")];
        let watcher =
            ServiceWatcher::new(ServiceId::new("api"), config).expect("watcher should build");

        assert_eq!(
            watcher.match_path(&root.join("src/lib.rs")),
            Some(PathBuf::from("src/lib.rs"))
        );
        assert_eq!(
            watcher.match_path(&root.join("src/generated/code.rs")),
            None
        );
        assert_eq!(watcher.match_path(&root.join("README.md")), None);
        assert_eq!(
            watcher.match_path(&root.join("README.md.bak")),
            Some(PathBuf::from("README.md.bak"))
        );
    }

    #[test]
    fn regex_ignore_rules_match_relative_paths() {
        let root = PathBuf::from("/tmp/workspace");
        let mut config = watch_config(&root);
        config.ignore_regex = vec![r"(^|/)generated/.*\.rs$".to_string(), r"\.tmp$".to_string()];
        let watcher =
            ServiceWatcher::new(ServiceId::new("api"), config).expect("watcher should build");

        assert_eq!(
            watcher.match_path(&root.join("src/lib.rs")),
            Some(PathBuf::from("src/lib.rs"))
        );
        assert_eq!(
            watcher.match_path(&root.join("src/generated/code.rs")),
            None
        );
        assert_eq!(watcher.match_path(&root.join("notes.tmp")), None);
    }

    #[test]
    fn event_matching_deduplicates_paths() {
        let root = PathBuf::from("/tmp/workspace");
        let watcher =
            ServiceWatcher::new(ServiceId::new("api"), watch_config(&root)).expect("watcher");

        let matched = watcher.match_event_paths(&[
            root.join("src/main.rs"),
            root.join("src/main.rs"),
            root.join("src/lib.rs"),
        ]);

        assert_eq!(
            matched,
            vec![PathBuf::from("src/main.rs"), PathBuf::from("src/lib.rs")]
        );
    }

    #[test]
    fn paths_outside_watch_roots_do_not_match() {
        let root = PathBuf::from("/tmp/workspace");
        let watcher =
            ServiceWatcher::new(ServiceId::new("api"), watch_config(&root)).expect("watcher");

        assert_eq!(
            watcher.match_path(Path::new("/tmp/other/src/main.rs")),
            None
        );
    }

    #[test]
    fn invalid_regex_ignore_rule_fails_watcher_build() {
        let root = PathBuf::from("/tmp/workspace");
        let mut config = watch_config(&root);
        config.ignore_regex = vec!["[".to_string()];

        assert!(ServiceWatcher::new(ServiceId::new("api"), config).is_err());
    }
}
