use std::path::PathBuf;

use color_eyre::eyre::{Result, WrapErr, eyre};
use llmy::codegraph::{ForgeAstOptions, SolcBuildInfo};

/// The compiler ASTs of a working copy: reused from an earlier build under the run root, or
/// produced by `forge build --ast` with the copy's own configuration. The build lands in a
/// staging directory that is renamed into place only once forge succeeded, so an interrupted
/// build never leaves a half-written set behind.
#[derive(Debug, Clone)]
pub struct AstBuild {
    pub root: PathBuf,
    pub dir: PathBuf,
    pub forge_bin: PathBuf,
}

impl AstBuild {
    pub async fn load_or_compile(&self) -> Result<SolcBuildInfo> {
        let build_info = self.dir.join("build-info");
        if build_info.is_dir() {
            tracing::info!("reusing compiler ASTs under {}", build_info.display());
            return SolcBuildInfo::load(self.root.clone(), &build_info)
                .await
                .map_err(|e| eyre!("cannot load ASTs under {}: {e}", build_info.display()));
        }
        let staging = self.dir.with_extension("tmp");
        if staging.exists() {
            tokio::fs::remove_dir_all(&staging)
                .await
                .wrap_err_with(|| format!("cannot clear {}", staging.display()))?;
        }
        tracing::info!(
            "compiling {} with {} for its ASTs",
            self.root.display(),
            self.forge_bin.display()
        );
        let options = ForgeAstOptions {
            forge_bin: self.forge_bin.clone(),
            skip: ["test", "script", "**/test/**", "**/script/**"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            profile: None,
        };
        SolcBuildInfo::from_forge(self.root.clone(), &staging, &options)
            .await
            .map_err(|e| eyre!("forge build --ast failed for {}: {e}", self.root.display()))?;
        tokio::fs::rename(&staging, &self.dir)
            .await
            .wrap_err_with(|| format!("cannot move {} into place", staging.display()))?;
        SolcBuildInfo::load(self.root.clone(), &build_info)
            .await
            .map_err(|e| eyre!("cannot load ASTs under {}: {e}", build_info.display()))
    }
}
