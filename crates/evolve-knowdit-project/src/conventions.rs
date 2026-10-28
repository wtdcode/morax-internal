use std::path::Path;

use evolve_knowdit_types::FoundryLayout;

/// The facts a harness-writing agent needs to produce a test that compiles inside this
/// project: the foundry config, the remappings and how existing tests import project code.
#[derive(Debug, Clone, Default)]
pub struct ProjectConventions {
    pub foundry_toml_excerpt: String,
    pub remappings: Vec<String>,
    pub sample_test_files: Vec<String>,
    pub sample_test_header: String,
    pub via_ir: bool,
    pub solc: Option<String>,
}

impl ProjectConventions {
    pub async fn load(layout: &FoundryLayout, foundry_excerpt: String) -> Self {
        let root = &layout.root;
        let test_dir = root.join(&layout.test_dir);
        let mut tests = Vec::new();
        Self::collect_sol(&test_dir, 2, &mut tests);
        tests.sort();
        let sample_test_files = tests
            .iter()
            .take(10)
            .filter_map(|p| p.strip_prefix(root).ok())
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let sample_test_header = match tests.first() {
            Some(first) => tokio::fs::read_to_string(first)
                .await
                .map(|t| Self::header_of(&t))
                .unwrap_or_default(),
            None => String::new(),
        };
        Self {
            foundry_toml_excerpt: foundry_excerpt,
            remappings: layout.remappings.clone(),
            sample_test_files,
            sample_test_header,
            via_ir: layout.via_ir,
            solc: layout.solc.clone(),
        }
    }

    fn collect_sol(dir: &Path, depth: usize, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if depth > 0 {
                    Self::collect_sol(&path, depth - 1, out);
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("sol") {
                out.push(path);
            }
        }
    }

    /// Everything before the first contract / library / interface declaration: pragma and
    /// imports, which is what a new test must imitate.
    fn header_of(text: &str) -> String {
        let mut out = String::new();
        for line in text.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("contract ")
                || trimmed.starts_with("abstract contract ")
                || trimmed.starts_with("library ")
                || trimmed.starts_with("interface ")
            {
                break;
            }
            out.push_str(line);
            out.push('\n');
        }
        out.chars().take(2_000).collect()
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("foundry.toml (excerpt):\n```toml\n");
        out.push_str(if self.foundry_toml_excerpt.is_empty() {
            "(none)\n"
        } else {
            &self.foundry_toml_excerpt
        });
        out.push_str("\n```\nremappings:\n");
        if self.remappings.is_empty() {
            out.push_str("(none declared; forge auto-detects lib/ remappings)\n");
        } else {
            for r in &self.remappings {
                out.push_str("- ");
                out.push_str(r);
                out.push('\n');
            }
        }
        out.push_str("existing test files (imitate their imports and setUp):\n");
        if self.sample_test_files.is_empty() {
            out.push_str("(no tests found)\n");
        } else {
            for f in &self.sample_test_files {
                out.push_str("- ");
                out.push_str(f);
                out.push('\n');
            }
        }
        if !self.sample_test_header.is_empty() {
            out.push_str("header of one existing test (pragma + imports):\n```solidity\n");
            out.push_str(&self.sample_test_header);
            out.push_str("\n```\n");
        }
        if self.via_ir {
            out.push_str("the project compiles with via_ir = true; keep the harness small and expect slow compiles\n");
        }
        if let Some(solc) = &self.solc {
            out.push_str(&format!("solc pinned to {solc}; use a matching pragma\n"));
        }
        out
    }
}
