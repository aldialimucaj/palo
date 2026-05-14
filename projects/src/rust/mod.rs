use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use palo_core::error::DiscoveryError;
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::{
    DiscoveredCommand, DiscoveredProject, DiscoveredService, DiscoveryIssue, ExecutableArtifact,
    ProjectAdapter, ProjectKind,
};

const MANIFEST_FILE_NAME: &str = "Cargo.toml";

#[derive(Debug, Default, Clone, Copy)]
pub struct RustProjectAdapter;

impl ProjectAdapter for RustProjectAdapter {
    fn kind(&self) -> ProjectKind {
        ProjectKind::Rust
    }

    fn discover(&self, workspace_root: &Path) -> Result<DiscoveredProject, DiscoveryError> {
        let manifest_path = workspace_root.join(MANIFEST_FILE_NAME);
        info!(
            workspace_root = %workspace_root.display(),
            manifest_path = %manifest_path.display(),
            "discovering rust project metadata",
        );

        let root_manifest = read_manifest(&manifest_path)?;
        let member_manifests =
            resolve_workspace_members(workspace_root, &manifest_path, &root_manifest)?;
        let workspace_name = root_manifest
            .package
            .as_ref()
            .map(|package| package.name.clone());
        let target_dir = workspace_root.join("target").join("debug");

        let mut project = DiscoveredProject::new(ProjectKind::Rust, workspace_root, &manifest_path);
        project.workspace_name = workspace_name;

        for member_manifest_path in member_manifests {
            let manifest = read_manifest(&member_manifest_path)?;
            let package = match manifest.package.as_ref() {
                Some(package) => package,
                None => {
                    debug!(
                        manifest_path = %member_manifest_path.display(),
                        "skipping workspace manifest without package section",
                    );
                    continue;
                }
            };

            let package_root = member_manifest_path
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| {
                    DiscoveryError::new("package manifest does not have a parent directory")
                        .with_project_type("rust")
                        .with_path(&member_manifest_path)
                })?;

            let binary_targets = infer_binary_targets(&manifest, &package_root, &package.name);

            match choose_binary_target(&package, &member_manifest_path, binary_targets) {
                Ok(binary_target) => {
                    let service = build_service(
                        workspace_root,
                        &target_dir,
                        &member_manifest_path,
                        &package_root,
                        &package.name,
                        &binary_target.name,
                    );

                    debug!(
                        package = package.name,
                        binary = binary_target.name,
                        manifest_path = %member_manifest_path.display(),
                        "discovered rust service",
                    );
                    project.services.push(service);
                }
                Err(issue) => {
                    warn!(
                        package = package.name,
                        manifest_path = %member_manifest_path.display(),
                        issue = issue.message,
                        "rust package could not be converted into a service",
                    );
                    project.issues.push(issue);
                }
            }
        }

        debug!(
            service_count = project.services.len(),
            issue_count = project.issues.len(),
            "completed rust project discovery",
        );

        Ok(project)
    }
}

#[derive(Debug, Deserialize)]
struct CargoManifest {
    package: Option<CargoPackage>,
    workspace: Option<CargoWorkspace>,
    #[serde(default, rename = "bin")]
    bins: Vec<CargoBinaryTarget>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    name: String,
    #[serde(rename = "default-run")]
    default_run: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CargoWorkspace {
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CargoBinaryTarget {
    name: Option<String>,
    path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BinaryTarget {
    name: String,
}

fn read_manifest(manifest_path: &Path) -> Result<CargoManifest, DiscoveryError> {
    let contents = fs::read_to_string(manifest_path).map_err(|error| {
        DiscoveryError::new(format!("failed to read Cargo manifest: {error}"))
            .with_project_type("rust")
            .with_path(manifest_path)
    })?;

    toml::from_str(&contents).map_err(|error| {
        DiscoveryError::new(format!("failed to parse Cargo manifest: {error}"))
            .with_project_type("rust")
            .with_path(manifest_path)
    })
}

fn resolve_workspace_members(
    workspace_root: &Path,
    root_manifest_path: &Path,
    root_manifest: &CargoManifest,
) -> Result<Vec<PathBuf>, DiscoveryError> {
    match root_manifest.workspace.as_ref() {
        Some(workspace) => {
            let excludes = compile_glob_set(&workspace.exclude, root_manifest_path)?;
            let candidates = collect_manifest_candidates(workspace_root)?;
            let members = if workspace.members.is_empty() {
                vec![root_manifest_path.to_path_buf()]
            } else {
                filter_member_manifests(
                    workspace_root,
                    &candidates,
                    &workspace.members,
                    excludes.as_ref(),
                )?
            };

            if members.is_empty() {
                return Err(
                    DiscoveryError::new("workspace did not resolve to any Cargo members")
                        .with_project_type("rust")
                        .with_path(root_manifest_path),
                );
            }

            debug!(
                workspace_root = %workspace_root.display(),
                member_count = members.len(),
                "resolved rust workspace members",
            );
            Ok(members)
        }
        None => Ok(vec![root_manifest_path.to_path_buf()]),
    }
}

fn collect_manifest_candidates(workspace_root: &Path) -> Result<Vec<PathBuf>, DiscoveryError> {
    let mut manifests = Vec::new();
    collect_manifests_recursive(workspace_root, &mut manifests).map_err(|error| {
        DiscoveryError::new(format!("failed to scan workspace members: {error}"))
            .with_project_type("rust")
            .with_path(workspace_root.join(MANIFEST_FILE_NAME))
    })?;
    manifests.sort();
    debug!(
        workspace_root = %workspace_root.display(),
        candidate_count = manifests.len(),
        "scanned rust manifest candidates",
    );
    Ok(manifests)
}

fn collect_manifests_recursive(
    directory: &Path,
    manifests: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            if should_skip_directory(&path) {
                continue;
            }

            collect_manifests_recursive(&path, manifests)?;
            continue;
        }

        if file_type.is_file()
            && path
                .file_name()
                .is_some_and(|name| name == MANIFEST_FILE_NAME)
        {
            manifests.push(path);
        }
    }

    Ok(())
}

fn should_skip_directory(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        matches!(
            name.to_string_lossy().as_ref(),
            "target" | ".git" | ".hg" | ".svn"
        )
    })
}

fn compile_glob_set(
    patterns: &[String],
    manifest_path: &Path,
) -> Result<Option<GlobSet>, DiscoveryError> {
    if patterns.is_empty() {
        return Ok(None);
    }

    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|error| {
            DiscoveryError::new(format!(
                "invalid workspace member glob `{pattern}`: {error}"
            ))
            .with_project_type("rust")
            .with_path(manifest_path)
        })?;
        builder.add(glob);
    }

    let set = builder.build().map_err(|error| {
        DiscoveryError::new(format!("failed to compile workspace member globs: {error}"))
            .with_project_type("rust")
            .with_path(manifest_path)
    })?;

    Ok(Some(set))
}

fn filter_member_manifests(
    workspace_root: &Path,
    candidates: &[PathBuf],
    members: &[String],
    excludes: Option<&GlobSet>,
) -> Result<Vec<PathBuf>, DiscoveryError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in members {
        let glob = Glob::new(pattern).map_err(|error| {
            DiscoveryError::new(format!(
                "invalid workspace member glob `{pattern}`: {error}"
            ))
            .with_project_type("rust")
            .with_path(workspace_root.join(MANIFEST_FILE_NAME))
        })?;
        builder.add(glob);
    }
    let member_set = builder.build().map_err(|error| {
        DiscoveryError::new(format!("failed to compile workspace member globs: {error}"))
            .with_project_type("rust")
            .with_path(workspace_root.join(MANIFEST_FILE_NAME))
    })?;

    let mut manifests = BTreeSet::new();
    for candidate in candidates {
        let Some(member_dir) = candidate.parent() else {
            continue;
        };

        let Ok(relative_dir) = member_dir.strip_prefix(workspace_root) else {
            continue;
        };

        let relative = normalize_relative_path(relative_dir);
        if relative.is_empty() {
            continue;
        }

        if member_set.is_match(&relative) && !excludes.is_some_and(|set| set.is_match(&relative)) {
            manifests.insert(candidate.clone());
        }
    }

    Ok(manifests.into_iter().collect())
}

fn normalize_relative_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn infer_binary_targets(
    manifest: &CargoManifest,
    package_root: &Path,
    package_name: &str,
) -> Vec<BinaryTarget> {
    let mut names = BTreeSet::new();

    for binary in &manifest.bins {
        if let Some(name) = binary.name.as_ref() {
            names.insert(name.clone());
            continue;
        }

        if let Some(path) = binary.path.as_ref()
            && let Some(stem) = path.file_stem()
        {
            names.insert(stem.to_string_lossy().into_owned());
        }
    }

    if package_root.join("src").join("main.rs").is_file() {
        names.insert(package_name.to_string());
    }

    let src_bin_dir = package_root.join("src").join("bin");
    if let Ok(entries) = fs::read_dir(&src_bin_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|extension| extension == "rs")
                && let Some(stem) = path.file_stem()
            {
                names.insert(stem.to_string_lossy().into_owned());
            }

            if path.is_dir()
                && path.join("main.rs").is_file()
                && let Some(name) = path.file_name()
            {
                names.insert(name.to_string_lossy().into_owned());
            }
        }
    }

    names
        .into_iter()
        .map(|name| BinaryTarget { name })
        .collect()
}

fn choose_binary_target(
    package: &CargoPackage,
    manifest_path: &Path,
    binary_targets: Vec<BinaryTarget>,
) -> Result<BinaryTarget, DiscoveryIssue> {
    match binary_targets.as_slice() {
        [] => Err(DiscoveryIssue::new(
            manifest_path,
            "no runnable binary target was found; define `src/main.rs` or a `[[bin]]` target",
        )
        .with_package_name(&package.name)),
        [binary] => Ok(binary.clone()),
        binaries => {
            if let Some(default_run) = package.default_run.as_ref()
                && let Some(binary) = binaries.iter().find(|binary| &binary.name == default_run)
            {
                return Ok(binary.clone());
            }

            let names = binaries
                .iter()
                .map(|binary| binary.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(
                DiscoveryIssue::new(
                    manifest_path,
                    format!(
                        "multiple runnable binaries were found ({names}); set `package.default-run` or refine the generated config manually"
                    ),
                )
                .with_package_name(&package.name),
            )
        }
    }
}

fn build_service(
    workspace_root: &Path,
    target_dir: &Path,
    manifest_path: &Path,
    package_root: &Path,
    package_name: &str,
    binary_name: &str,
) -> DiscoveredService {
    let mut watch_paths = vec![
        workspace_root.join(MANIFEST_FILE_NAME),
        manifest_path.to_path_buf(),
    ];
    let src_dir = package_root.join("src");
    if src_dir.exists() {
        watch_paths.push(src_dir);
    }

    DiscoveredService {
        id: binary_name.to_string(),
        name: binary_name.to_string(),
        package_name: package_name.to_string(),
        binary_name: binary_name.to_string(),
        manifest_path: manifest_path.to_path_buf(),
        package_root: package_root.to_path_buf(),
        run: DiscoveredCommand::new(
            target_dir.join(binary_name).to_string_lossy().into_owned(),
            Vec::<String>::new(),
            workspace_root,
        ),
        check: DiscoveredCommand::new(
            "cargo",
            ["check", "--package", package_name, "--bin", binary_name],
            workspace_root,
        ),
        build: DiscoveredCommand::new(
            "cargo",
            ["build", "--package", package_name, "--bin", binary_name],
            workspace_root,
        ),
        executable: ExecutableArtifact {
            name: binary_name.to_string(),
            debug_path: target_dir.join(binary_name),
        },
        watch_paths,
    }
}
