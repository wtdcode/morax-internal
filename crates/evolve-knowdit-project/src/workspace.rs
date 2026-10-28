use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, WrapErr};

/// A private copy of the Foundry project so harness files and build artifacts never touch the
/// original checkout.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub source_root: PathBuf,
    pub destination: PathBuf,
}

impl Workspace {
    /// Copies the project when the destination does not hold one yet; a destination that
    /// already has a `foundry.toml` is reused so a resumed run keeps its harness files.
    pub async fn materialize(&self) -> Result<PathBuf> {
        if self.destination.join("foundry.toml").is_file() {
            tracing::info!("reusing working copy at {}", self.destination.display());
            return Ok(self.destination.canonicalize()?);
        }
        let source = self.source_root.clone();
        let dest = self.destination.clone();
        tracing::info!("copying {} to {}", source.display(), dest.display());
        tokio::task::spawn_blocking(move || -> Result<()> {
            let staging = dest.with_extension(format!("partial-{}", std::process::id()));
            if staging.exists() {
                std::fs::remove_dir_all(&staging)?;
            }
            Self::copy_tree(&source, &staging, true)?;
            // The code-graph walker honours `.ignore`; vendored and build trees are never
            // project sources, and skipping them cuts indexing from tens of seconds to well
            // under one. Forge ignores this file.
            std::fs::write(
                staging.join(".ignore"),
                "lib/\nnode_modules/\nout/\ncache/\ncache-forge/\nartifacts/\nbroadcast/\n",
            )?;
            std::fs::rename(&staging, &dest)
                .wrap_err_with(|| format!("failed to move copy into {}", dest.display()))?;
            Ok(())
        })
        .await
        .wrap_err("copy task panicked")??;
        Ok(self.destination.canonicalize()?)
    }

    fn skip_top_level(name: &str) -> bool {
        matches!(
            name,
            "out"
                | "cache"
                | "cache-forge"
                | "broadcast"
                | "artifacts"
                | "typechain"
                | "typechain-types"
        )
    }

    fn copy_tree(from: &Path, to: &Path, top_level: bool) -> Result<()> {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str == ".git" || (top_level && Self::skip_top_level(&name_str)) {
                continue;
            }
            let src = entry.path();
            let dst = to.join(&name);
            let meta = std::fs::symlink_metadata(&src)?;
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&src)?;
                std::os::unix::fs::symlink(target, &dst)
                    .wrap_err_with(|| format!("failed to recreate symlink {}", dst.display()))?;
            } else if meta.is_dir() {
                Self::copy_tree(&src, &dst, false)?;
            } else {
                std::fs::copy(&src, &dst)
                    .wrap_err_with(|| format!("failed to copy {}", src.display()))?;
            }
        }
        Ok(())
    }
}
