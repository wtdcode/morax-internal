use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, WrapErr};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Where each artifact of one run lives under the output root. Callers compose file paths from
/// these directories with `join` / `with_extension`.
#[derive(Debug, Clone)]
pub struct RunLayout {
    pub manifest_file: PathBuf,
    pub repo_dir: PathBuf,
    /// Compiler ASTs of the working copy (`forge build --ast` output), reused on resume.
    pub build_info_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub llm_debug_file: PathBuf,
    pub drafts_dir: PathBuf,
    pub properties_file: PathBuf,
    pub rows_dir: PathBuf,
    pub matrix_file: PathBuf,
    /// One task per obligation cell, `<subject>x<property>.json`.
    pub tasks_dir: PathBuf,
    /// The scenarios of every task, written by its unit agent, `<task>.json`.
    pub units_dir: PathBuf,
    /// The reproductions derived from the tasks, as verified.
    pub units_file: PathBuf,
    pub verify_dir: PathBuf,
    pub poc_dir: PathBuf,
    /// Aborted records a resumed run ran again, kept as `<unit>-<finished at>.json`.
    pub aborted_dir: PathBuf,
    pub report_dir: PathBuf,
}

impl RunLayout {
    pub fn new(root: &Path) -> Self {
        let matrix_dir = root.join("matrix");
        let verify_dir = root.join("verify");
        Self {
            manifest_file: root.join("manifest.json"),
            repo_dir: root.join("repo"),
            build_info_dir: root.join("build-info"),
            logs_dir: root.join("logs"),
            llm_debug_file: root.join("llm-debug.sqlite3"),
            drafts_dir: matrix_dir.join("drafts"),
            properties_file: matrix_dir.join("properties.json"),
            rows_dir: matrix_dir.join("rows"),
            matrix_file: matrix_dir.join("matrix.json"),
            tasks_dir: matrix_dir.join("tasks"),
            units_dir: matrix_dir.join("units"),
            units_file: matrix_dir.join("units.json"),
            poc_dir: verify_dir.join("poc"),
            aborted_dir: verify_dir.join("aborted"),
            verify_dir,
            report_dir: root.join("report"),
        }
    }
}

/// Atomic JSON persistence under one output root. Every write goes to a temporary sibling and
/// is renamed into place, so an interrupted run never leaves a half-written file behind.
#[derive(Debug, Clone)]
pub struct JsonStore {
    pub root: PathBuf,
    pub layout: RunLayout,
}

impl JsonStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            layout: RunLayout::new(&root),
            root,
        }
    }

    pub async fn write<T: Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(value).wrap_err("failed to serialize")?;
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        tokio::fs::write(&tmp, bytes)
            .await
            .wrap_err_with(|| format!("failed to write {}", tmp.display()))?;
        tokio::fs::rename(&tmp, path)
            .await
            .wrap_err_with(|| format!("failed to move {} into place", path.display()))?;
        Ok(())
    }

    /// Reads the file when it exists and parses it; a missing file is `None`, a file that does
    /// not parse is an error.
    pub async fn read_optional<T: DeserializeOwned>(&self, path: &Path) -> Result<Option<T>> {
        if !path.is_file() {
            return Ok(None);
        }
        let bytes = tokio::fs::read(path)
            .await
            .wrap_err_with(|| format!("failed to read {}", path.display()))?;
        let value = serde_json::from_slice(&bytes)
            .wrap_err_with(|| format!("failed to parse {}", path.display()))?;
        Ok(Some(value))
    }

    /// Lists `*.json` files directly under `dir`, sorted by name.
    pub async fn list_json(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        if !dir.is_dir() {
            return Ok(out);
        }
        let mut entries = tokio::fs::read_dir(dir)
            .await
            .wrap_err_with(|| format!("failed to list {}", dir.display()))?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                out.push(path);
            }
        }
        out.sort();
        Ok(out)
    }

    pub async fn write_text(&self, path: &Path, text: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
        }
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        tokio::fs::write(&tmp, text)
            .await
            .wrap_err_with(|| format!("failed to write {}", tmp.display()))?;
        tokio::fs::rename(&tmp, path)
            .await
            .wrap_err_with(|| format!("failed to move {} into place", path.display()))?;
        Ok(())
    }
}
