use std::fmt;
use std::path::{Path, PathBuf};

use palo_core::error::DiscoveryError;

pub mod generic;
pub mod rust;

pub use generic::GenericProjectAdapter;
pub use rust::RustProjectAdapter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    Generic,
    Rust,
}

impl fmt::Display for ProjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Generic => f.write_str("generic"),
            Self::Rust => f.write_str("rust"),
        }
    }
}

pub trait ProjectAdapter {
    fn kind(&self) -> ProjectKind;

    fn discover(&self, workspace_root: &Path) -> Result<DiscoveredProject, DiscoveryError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredProject {
    pub kind: ProjectKind,
    pub workspace_root: PathBuf,
    pub manifest_path: PathBuf,
    pub workspace_name: Option<String>,
    pub services: Vec<DiscoveredService>,
    pub issues: Vec<DiscoveryIssue>,
}

impl DiscoveredProject {
    pub fn new(
        kind: ProjectKind,
        workspace_root: impl Into<PathBuf>,
        manifest_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            kind,
            workspace_root: workspace_root.into(),
            manifest_path: manifest_path.into(),
            workspace_name: None,
            services: Vec::new(),
            issues: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredService {
    pub id: String,
    pub name: String,
    pub package_name: String,
    pub binary_name: String,
    pub manifest_path: PathBuf,
    pub package_root: PathBuf,
    pub run: DiscoveredCommand,
    pub check: DiscoveredCommand,
    pub build: DiscoveredCommand,
    pub executable: ExecutableArtifact,
    pub watch_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCommand {
    pub program: String,
    pub args: Vec<String>,
    pub working_dir: PathBuf,
}

impl DiscoveredCommand {
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
        working_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            working_dir: working_dir.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutableArtifact {
    pub name: String,
    pub debug_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryIssue {
    pub package_name: Option<String>,
    pub manifest_path: PathBuf,
    pub message: String,
}

impl DiscoveryIssue {
    pub fn new(manifest_path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self {
            package_name: None,
            manifest_path: manifest_path.into(),
            message: message.into(),
        }
    }

    pub fn with_package_name(mut self, package_name: impl Into<String>) -> Self {
        self.package_name = Some(package_name.into());
        self
    }
}

pub fn adapter_for_kind(kind: ProjectKind) -> Box<dyn ProjectAdapter> {
    match kind {
        ProjectKind::Generic => Box::new(GenericProjectAdapter),
        ProjectKind::Rust => Box::new(RustProjectAdapter::default()),
    }
}

pub fn detect_project_kind(workspace_root: &Path) -> Result<Option<ProjectKind>, DiscoveryError> {
    if workspace_root.join("Cargo.toml").is_file() {
        RustProjectAdapter::default().discover(workspace_root)?;
        return Ok(Some(ProjectKind::Rust));
    }

    Ok(None)
}
