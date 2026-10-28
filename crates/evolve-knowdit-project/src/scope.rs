use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use color_eyre::eyre::Result;
use evolve_knowdit_types::ScopeContext;

/// Reads the engagement scope files and context documents around a project.
#[derive(Debug, Clone)]
pub struct ScopeLoader {
    pub dataset_root: PathBuf,
    pub foundry_root: PathBuf,
    /// Path of the Foundry root relative to the dataset root, with a trailing slash, or empty.
    pub relative_prefix: String,
    pub scope_file: Option<PathBuf>,
    pub out_of_scope_file: Option<PathBuf>,
    /// A file whose text replaces the built-in threat model.
    pub threat_model_file: Option<PathBuf>,
    pub readme_chars: usize,
}

impl ScopeLoader {
    pub fn load(&self) -> Result<ScopeContext> {
        let in_scope = self.load_list(self.scope_file.clone(), "scope.txt")?;
        let out_of_scope = self.load_list(self.out_of_scope_file.clone(), "out_of_scope.txt")?;
        let readme_excerpt = self.read_first_existing(
            &["README.md", "README-sponsor.md", "readme.md", "README"],
            self.readme_chars,
        );
        let known_issues = self.read_first_existing(
            &[
                "known-issues.md",
                "KNOWN_ISSUES.md",
                "known_issues.md",
                "KNOWN-ISSUES.md",
            ],
            self.readme_chars,
        );
        let threat_model = self
            .threat_model_file
            .as_ref()
            .map(std::fs::read_to_string)
            .transpose()?;
        Ok(ScopeContext {
            in_scope,
            out_of_scope,
            readme_excerpt,
            known_issues,
            threat_model,
        })
    }

    fn load_list(&self, explicit: Option<PathBuf>, default_name: &str) -> Result<BTreeSet<String>> {
        let path = match explicit {
            Some(p) => p,
            None => {
                let at_dataset = self.dataset_root.join(default_name);
                let at_root = self.foundry_root.join(default_name);
                if at_dataset.is_file() {
                    at_dataset
                } else if at_root.is_file() {
                    at_root
                } else {
                    return Ok(BTreeSet::new());
                }
            }
        };
        let text = std::fs::read_to_string(&path)?;
        let mut out = BTreeSet::new();
        let mut dropped = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut value = line.trim_start_matches("./").to_string();
            if !self.relative_prefix.is_empty() {
                if let Some(stripped) = value.strip_prefix(&self.relative_prefix) {
                    value = stripped.to_string();
                } else if self.foundry_root != self.dataset_root
                    && !self.foundry_root.join(&value).exists()
                {
                    dropped.push(value);
                    continue;
                }
            }
            if !self.foundry_root.join(&value).exists() {
                dropped.push(value);
                continue;
            }
            out.insert(value);
        }
        if !dropped.is_empty() {
            tracing::warn!(
                "{}: {} entries do not exist under {} and were dropped: {}",
                path.display(),
                dropped.len(),
                self.foundry_root.display(),
                dropped
                    .iter()
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Ok(out)
    }

    fn read_first_existing(&self, names: &[&str], max_chars: usize) -> String {
        let roots: Vec<&Path> = if self.dataset_root == self.foundry_root {
            vec![&self.dataset_root]
        } else {
            vec![&self.dataset_root, &self.foundry_root]
        };
        for root in roots {
            for name in names {
                let path = root.join(name);
                if path.is_file()
                    && let Ok(text) = std::fs::read_to_string(&path)
                {
                    return text.chars().take(max_chars).collect();
                }
            }
        }
        String::new()
    }
}
