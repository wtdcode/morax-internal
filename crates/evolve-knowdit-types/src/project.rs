use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The Foundry layout facts every later stage needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoundryLayout {
    /// Directory containing `foundry.toml`, absolute.
    pub root: PathBuf,
    /// `src` directory relative to `root`.
    pub src_dir: String,
    /// `test` directory relative to `root`.
    pub test_dir: String,
    pub via_ir: bool,
    pub solc: Option<String>,
    pub remappings: Vec<String>,
    pub lib_dirs: Vec<String>,
}

/// Engagement scope as the competition or client states it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScopeContext {
    /// In-scope source files relative to the Foundry root. Empty means every project source.
    pub in_scope: BTreeSet<String>,
    /// Explicitly out-of-scope files relative to the Foundry root.
    pub out_of_scope: BTreeSet<String>,
    /// Excerpt of the project README (architecture, roles, trust assumptions).
    pub readme_excerpt: String,
    /// Known issues or excluded classes the engagement declares.
    pub known_issues: String,
    /// The adversary and impact the audit targets, when the engagement gives its own: replaces
    /// the built-in threat model in every agent's prompt.
    #[serde(default)]
    pub threat_model: Option<String>,
}

impl ScopeContext {
    pub fn is_in_scope(&self, relative_file: &str) -> bool {
        if self.out_of_scope.contains(relative_file) {
            return false;
        }
        self.in_scope.is_empty() || self.in_scope.contains(relative_file)
    }
}

/// Persisted once per run; every stage reads the project through it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectManifest {
    /// The directory the user pointed at (dataset root).
    pub dataset_root: PathBuf,
    /// The Foundry root as originally found under `dataset_root`.
    pub source_root: PathBuf,
    /// The Foundry root the pipeline works in (a private copy when copying is enabled).
    pub layout: FoundryLayout,
    pub scope: ScopeContext,
    pub created_at: chrono::DateTime<chrono::Utc>,
}
