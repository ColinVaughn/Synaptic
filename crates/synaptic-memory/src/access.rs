use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{AccessScope, MemoryRecord};

/// Server-configured authorization context. Tool arguments never carry this
/// value, so a caller cannot widen its own access by changing a request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryPrincipal {
    pub id: String,
    #[serde(default)]
    repositories: BTreeSet<String>,
    #[serde(default)]
    workspaces: BTreeSet<String>,
    #[serde(default)]
    allow_all_private: bool,
}

impl MemoryPrincipal {
    /// Trusted local/operator compatibility mode.
    pub fn operator() -> Self {
        Self {
            id: "operator".into(),
            repositories: BTreeSet::from(["*".into()]),
            workspaces: BTreeSet::from(["*".into()]),
            allow_all_private: true,
        }
    }

    pub fn restricted(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            repositories: BTreeSet::new(),
            workspaces: BTreeSet::new(),
            allow_all_private: false,
        }
    }

    pub fn with_repository(mut self, repository: impl Into<String>) -> Self {
        self.repositories.insert(repository.into());
        self
    }

    pub fn with_workspace(mut self, workspace: impl Into<String>) -> Self {
        self.workspaces.insert(workspace.into());
        self
    }

    pub fn with_all_private(mut self, allow: bool) -> Self {
        self.allow_all_private = allow;
        self
    }

    pub fn can_access(&self, record: &MemoryRecord) -> bool {
        match &record.access_scope {
            AccessScope::Private => {
                self.allow_all_private || record.owner.as_deref() == Some(self.id.as_str())
            }
            AccessScope::Repository => {
                self.repositories.contains("*") || self.repositories.contains(&record.repository)
            }
            AccessScope::Workspace { workspace } => {
                self.workspaces.contains("*") || self.workspaces.contains(workspace)
            }
        }
    }
}

impl Default for MemoryPrincipal {
    fn default() -> Self {
        Self::operator()
    }
}
