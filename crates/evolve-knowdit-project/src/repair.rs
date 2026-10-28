use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, WrapErr, eyre};
use toml_edit::{DocumentMut, Item};

use crate::layout::FoundryToml;

/// Repairs a working copy's Foundry configuration so `forge test` can run in this
/// environment: a `solc` pinned to a local binary that is not there, an empty `eth_rpc_url`
/// that makes forge try to fork, and a missing `forge-std` dependency. Only applied to copies
/// the pipeline owns, never to the original checkout.
#[derive(Debug, Clone)]
pub struct FoundryRepair {
    pub root: PathBuf,
    pub forge_std_source: Option<PathBuf>,
}

impl FoundryRepair {
    /// Applies every repair whose precondition holds and reports what changed.
    pub fn apply(&self) -> Result<Vec<String>> {
        let toml_path = self.root.join("foundry.toml");
        let text = std::fs::read_to_string(&toml_path)
            .wrap_err_with(|| format!("failed to read {}", toml_path.display()))?;
        let mut doc: DocumentMut = text
            .parse()
            .wrap_err_with(|| format!("failed to parse {}", toml_path.display()))?;
        let changes = Self::repair_profile(&mut doc, &self.root);
        if !changes.is_empty() {
            Self::write_atomically(&toml_path, &doc.to_string())?;
        }
        let mut all = changes;
        all.extend(self.provision_forge_std()?);
        Ok(all)
    }

    /// Drops, from the default profile, a `solc` that points at a missing local binary and an
    /// empty `eth_rpc_url`; returns one line per change.
    fn repair_profile(doc: &mut DocumentMut, root: &Path) -> Vec<String> {
        let mut changes = Vec::new();
        let Some(profile) = doc
            .get_mut("profile")
            .and_then(Item::as_table_like_mut)
            .and_then(|p| p.get_mut("default"))
            .and_then(Item::as_table_like_mut)
        else {
            return changes;
        };
        let solc = profile.get("solc").and_then(Item::as_str).map(String::from);
        if let Some(value) = solc
            && (value.contains('/') || value.contains('\\'))
            && !root.join(&value).exists()
        {
            profile.remove("solc");
            changes.push(format!(
                "dropped local solc pin `{value}` (binary missing; forge auto-detects a version)"
            ));
        }
        let rpc = profile
            .get("eth_rpc_url")
            .and_then(Item::as_str)
            .map(str::trim)
            .map(String::from);
        if let Some(value) = rpc
            && value.is_empty()
        {
            profile.remove("eth_rpc_url");
            changes
                .push("dropped empty eth_rpc_url (an empty fork url makes forge fork)".to_string());
        }
        changes
    }

    fn provision_forge_std(&self) -> Result<Vec<String>> {
        let toml = FoundryToml::load(&self.root)?;
        let lib_dirs: Vec<PathBuf> = toml.libs.iter().map(|l| self.root.join(l)).collect();
        let present = lib_dirs
            .iter()
            .any(|dir| dir.join("forge-std").join("src").join("Test.sol").is_file());
        if present {
            return Ok(Vec::new());
        }
        let Some(source) = &self.forge_std_source else {
            tracing::warn!(
                "no forge-std under {:?}; pass --forge-std-source to provision one",
                toml.libs
            );
            return Ok(Vec::new());
        };
        if !source.join("src").join("Test.sol").is_file() {
            return Err(eyre!(
                "--forge-std-source {} does not contain src/Test.sol",
                source.display()
            ));
        }
        let Some(target_dir) = lib_dirs.first() else {
            return Ok(Vec::new());
        };
        let target = target_dir.join("forge-std");
        Self::copy_tree(source, &target)?;
        let rel = target
            .strip_prefix(&self.root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| target.to_string_lossy().to_string());
        let remapping = format!("forge-std/={rel}/src/");
        let remappings_path = self.root.join("remappings.txt");
        let existing = if remappings_path.is_file() {
            std::fs::read_to_string(&remappings_path)
                .wrap_err_with(|| format!("failed to read {}", remappings_path.display()))?
        } else {
            String::new()
        };
        if !existing
            .lines()
            .any(|l| l.trim_start().starts_with("forge-std/="))
        {
            let mut text = existing;
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&remapping);
            text.push('\n');
            Self::write_atomically(&remappings_path, &text)?;
        }
        Ok(vec![format!(
            "provisioned forge-std into {rel} with remapping {remapping}"
        )])
    }

    /// Writes through a temporary sibling and a rename, so an interrupted repair never leaves
    /// a half-written configuration behind.
    fn write_atomically(path: &Path, text: &str) -> Result<()> {
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&tmp, text)
            .wrap_err_with(|| format!("failed to write {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .wrap_err_with(|| format!("failed to move {} into place", path.display()))?;
        Ok(())
    }

    fn copy_tree(from: &Path, to: &Path) -> Result<()> {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let src = entry.path();
            let dst = to.join(entry.file_name());
            if entry.file_name() == ".git" {
                continue;
            }
            if src.is_dir() {
                Self::copy_tree(&src, &dst)?;
            } else {
                std::fs::copy(&src, &dst)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_solc_pin_and_empty_rpc_url_are_removed_keeping_the_rest() {
        let text = "[profile.default]\nsrc = 'src'\nsolc = './bin/solc' # local binary\neth_rpc_url = \"\"\nvia_ir = true\n\n[fmt]\nline_length = 100\n";
        let mut doc: DocumentMut = text.parse().expect("parses");
        let changes = FoundryRepair::repair_profile(&mut doc, Path::new("/nonexistent-root"));
        assert_eq!(changes.len(), 2, "{changes:?}");
        let out = doc.to_string();
        assert!(!out.contains("solc ="), "{out}");
        assert!(!out.contains("eth_rpc_url"), "{out}");
        assert!(out.contains("via_ir = true"), "{out}");
        assert!(out.contains("[fmt]"), "{out}");
    }

    #[test]
    fn a_version_pin_is_kept() {
        let text = "[profile.default]\nsolc = '0.8.13'\n";
        let mut doc: DocumentMut = text.parse().expect("parses");
        let changes = FoundryRepair::repair_profile(&mut doc, Path::new("/nonexistent-root"));
        assert!(changes.is_empty(), "{changes:?}");
    }
}
