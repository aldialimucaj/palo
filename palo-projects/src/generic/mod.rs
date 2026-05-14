use std::path::Path;

use palo_core::error::DiscoveryError;

use crate::{DiscoveredProject, ProjectAdapter, ProjectKind};

#[derive(Debug, Default, Clone, Copy)]
pub struct GenericProjectAdapter;

impl ProjectAdapter for GenericProjectAdapter {
    fn kind(&self) -> ProjectKind {
        ProjectKind::Generic
    }

    fn discover(&self, workspace_root: &Path) -> Result<DiscoveredProject, DiscoveryError> {
        let manifest_path = workspace_root.join("palo.yml");
        Ok(DiscoveredProject::new(
            ProjectKind::Generic,
            workspace_root,
            manifest_path,
        ))
    }
}
