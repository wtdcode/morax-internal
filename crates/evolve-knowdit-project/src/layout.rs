use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, WrapErr, eyre};
use evolve_knowdit_types::FoundryLayout;

/// Locates the Foundry project under the directory the user pointed at.
#[derive(Debug, Clone)]
pub struct ProjectResolver {
    pub dataset_root: PathBuf,
    pub foundry_root_override: Option<PathBuf>,
}

impl ProjectResolver {
    /// Returns the absolute Foundry root and its path relative to the dataset root (`""` when
    /// they coincide). The relative prefix is what scope entries written against the dataset
    /// root have to lose to become project-relative.
    pub fn resolve(&self) -> Result<(PathBuf, String)> {
        let dataset_root = self
            .dataset_root
            .canonicalize()
            .wrap_err_with(|| format!("cannot access {}", self.dataset_root.display()))?;
        if let Some(over) = &self.foundry_root_override {
            let root = if over.is_absolute() {
                over.clone()
            } else {
                dataset_root.join(over)
            };
            let root = root
                .canonicalize()
                .wrap_err_with(|| format!("cannot access foundry root {}", root.display()))?;
            if !root.join("foundry.toml").is_file() {
                return Err(eyre!("{} has no foundry.toml", root.display()));
            }
            return Ok((root.clone(), Self::relative_prefix(&dataset_root, &root)));
        }
        if dataset_root.join("foundry.toml").is_file() {
            return Ok((dataset_root, String::new()));
        }
        let candidates = Self::find_foundry_roots(&dataset_root, 2);
        match candidates.len() {
            0 => Err(eyre!(
                "no foundry.toml under {} (depth 2); pass --foundry-root",
                dataset_root.display()
            )),
            1 => {
                let root = candidates[0].clone();
                let prefix = Self::relative_prefix(&dataset_root, &root);
                Ok((root, prefix))
            }
            _ => Err(eyre!(
                "several Foundry projects under {}: {}; pass --foundry-root to pick one",
                dataset_root.display(),
                candidates
                    .iter()
                    .map(|c| c.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    fn relative_prefix(dataset_root: &Path, root: &Path) -> String {
        root.strip_prefix(dataset_root)
            .map(|p| {
                let s = p.to_string_lossy().to_string();
                if s.is_empty() { s } else { format!("{s}/") }
            })
            .unwrap_or_default()
    }

    fn find_foundry_roots(dir: &Path, depth: usize) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        let mut children: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .filter(|p| {
                !matches!(
                    p.file_name().and_then(|n| n.to_str()),
                    Some("lib") | Some("node_modules") | Some(".git") | Some("out") | Some("cache")
                )
            })
            .collect();
        children.sort();
        for child in children {
            if child.join("foundry.toml").is_file() {
                out.push(child);
            } else if depth > 1 {
                out.extend(Self::find_foundry_roots(&child, depth - 1));
            }
        }
        out
    }
}

/// The subset of `foundry.toml` the pipeline needs, read from the default profile.
#[derive(Debug, Clone)]
pub struct FoundryToml {
    pub src: String,
    pub test: String,
    pub libs: Vec<String>,
    pub remappings: Vec<String>,
    pub via_ir: bool,
    pub solc: Option<String>,
    pub raw_excerpt: String,
}

impl FoundryToml {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("foundry.toml");
        let text = std::fs::read_to_string(&path)
            .wrap_err_with(|| format!("failed to read {}", path.display()))?;
        let value: toml::Value = toml::from_str(&text)
            .wrap_err_with(|| format!("failed to parse {}", path.display()))?;
        let profile = value
            .get("profile")
            .and_then(|p| p.get("default"))
            .or_else(|| value.get("default"))
            .cloned()
            .unwrap_or(toml::Value::Table(Default::default()));
        let string = |key: &str| profile.get(key).and_then(|v| v.as_str()).map(String::from);
        let strings = |key: &str| -> Vec<String> {
            profile
                .get(key)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        let via_ir = profile
            .get("via_ir")
            .or_else(|| profile.get("via-ir"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut remappings = strings("remappings");
        let remappings_txt = root.join("remappings.txt");
        if remappings_txt.is_file() {
            let extra = std::fs::read_to_string(&remappings_txt).unwrap_or_default();
            remappings.extend(
                extra
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(String::from),
            );
        }
        let libs = {
            let l = strings("libs");
            if l.is_empty() {
                vec!["lib".to_string()]
            } else {
                l
            }
        };
        Ok(Self {
            src: string("src").unwrap_or_else(|| {
                if root.join("src").is_dir() {
                    "src".into()
                } else {
                    "contracts".into()
                }
            }),
            test: string("test").unwrap_or_else(|| "test".into()),
            libs,
            remappings,
            via_ir,
            solc: string("solc").or_else(|| string("solc_version")),
            raw_excerpt: text.chars().take(4_000).collect(),
        })
    }

    pub fn to_layout(&self, root: PathBuf) -> FoundryLayout {
        FoundryLayout {
            root,
            src_dir: self.src.trim_end_matches('/').to_string(),
            test_dir: self.test.trim_end_matches('/').to_string(),
            via_ir: self.via_ir,
            solc: self.solc.clone(),
            remappings: self.remappings.clone(),
            lib_dirs: self.libs.clone(),
        }
    }
}
