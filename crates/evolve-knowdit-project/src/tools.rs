use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use globset::{Glob, GlobMatcher};
use llmy::agent::LLMYError;
use llmy::agent::tool::{Tool, ToolBox};
use llmy::agent::tools::files::{
    FileToolConfig, GrepDirectoryArgs, GrepDirectoryTool, sanitize_join_relative_path,
};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::Deserialize;

use llmy::codegraph::CodegraphToolOptions;
use llmy::codegraph::model::LineSpan;
use llmy::codegraph::tools::{LookupCallableTool, LookupStateTool};

use crate::index::CodeIndex;

/// One glob or a list of globs. Models write a single pattern as a bare string as often as
/// a one-element list; both parse, and the advertised schema admits both.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Globs {
    Many(Vec<String>),
    One(String),
}

impl Globs {
    pub fn into_vec(self) -> Vec<String> {
        match self {
            Globs::Many(v) => v,
            Globs::One(s) => vec![s],
        }
    }
}

impl JsonSchema for Globs {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Globs")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "anyOf": [
                {"type": "array", "items": {"type": "string"}},
                {"type": "string"}
            ]
        })
    }
}

/// Arguments of the project `grep` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GrepArgs {
    /// Directory to search recursively, relative to the project root (`.` for the whole
    /// project).
    pub directory: PathBuf,
    /// PCRE2 regular expression; only lines matching it are reported.
    pub pattern: String,
    /// Optional regular expression; lines that also match it are dropped.
    #[serde(default)]
    pub invert_pattern: Option<String>,
    /// Glob patterns (gitignore syntax) restricting which files are searched: one pattern or
    /// a list, each written as a JSON string in quotes, e.g. `"*.sol"` or `["src/**/*.sol"]`.
    #[serde(default)]
    pub include: Option<Globs>,
    /// Glob patterns excluding files from the search: one pattern or a list, each a quoted
    /// JSON string.
    #[serde(default)]
    pub exclude: Option<Globs>,
    /// Search case-insensitively.
    #[serde(default)]
    pub case_insensitive: bool,
    /// Maximum number of matching lines to return (default 50).
    #[serde(default)]
    pub max_matches: Option<usize>,
}

/// The parts of the project an agent may not read: the harness directories of the
/// reproductions. `own` is the one directory inside `hidden` the agent may still read — the
/// harness of its own reproduction attempt.
#[derive(Debug, Clone, Default)]
pub struct PathFence {
    pub hidden: Vec<PathBuf>,
    pub own: Option<PathBuf>,
}

impl PathFence {
    /// Whether `path` (absolute, canonical where it exists) lies behind the fence.
    pub fn refuses(&self, path: &Path) -> bool {
        let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        if self
            .own
            .as_ref()
            .is_some_and(|own| path.starts_with(canonical(own)))
        {
            return false;
        }
        self.hidden.iter().any(|h| path.starts_with(canonical(h)))
    }

    /// Exclusion globs for a directory search: every hidden directory at any depth.
    fn exclude_globs(&self) -> Vec<String> {
        self.hidden
            .iter()
            .filter_map(|h| h.file_name())
            .map(|name| format!("**/{}/**", name.to_string_lossy()))
            .collect()
    }
}

/// File access for agents: rooted at the project, refusing what the fence hides.
#[derive(Debug, Clone)]
pub struct ProjectFiles {
    pub root: PathBuf,
    pub fence: PathFence,
    /// Cap on the bytes one read returns.
    pub max_read_bytes: usize,
}

impl ProjectFiles {
    /// The absolute path of a project-relative `path`, or why the agent may not touch it.
    fn resolve(&self, path: &Path) -> Result<PathBuf, String> {
        let joined = sanitize_join_relative_path(&self.root, path)?;
        let resolved = joined.canonicalize().unwrap_or(joined);
        let root = self.root.canonicalize().unwrap_or_else(|_| self.root.clone());
        if !resolved.starts_with(&root) {
            return Err(format!("{} is outside the project", path.display()));
        }
        if self.fence.refuses(&resolved) {
            return Err(format!(
                "{} belongs to another reproduction's harness and cannot be read",
                path.display()
            ));
        }
        Ok(resolved)
    }

    fn relative(&self, path: &Path) -> String {
        let root = self.root.canonicalize().unwrap_or_else(|_| self.root.clone());
        path.strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string()
    }

    /// Every file under `dir` whose name matches `matcher`, skipping fenced directories.
    fn find(&self, dir: &Path, matcher: &GlobMatcher, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if self.fence.refuses(&path) {
                continue;
            }
            if path.is_dir() {
                self.find(&path, matcher, out);
            } else if matcher.is_match(entry.file_name()) {
                out.push(path);
            }
        }
    }
}

/// Arguments of `read_file`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// Path to the file, relative to the project root.
    pub file_path: PathBuf,
    /// Optional 1-based line number to start reading from. Defaults to 1.
    #[serde(default)]
    pub start_line: Option<usize>,
    /// Optional number of lines to read from `start_line`.
    #[serde(default)]
    pub line_count: Option<usize>,
}

/// Reads a project file the code graph does not hold, whole or a line range.
#[derive(Debug, Clone)]
pub struct ReadFileTool {
    pub files: ProjectFiles,
    pub index: Arc<CodeIndex>,
    /// Whether the agent has `read_module_source`, so the refusal names only tools it has.
    pub module_source: bool,
}

impl ReadFileTool {
    /// Why `file` (project-relative) is read by name instead: the readable graph holds it.
    /// `line` names the function, declaration or contract around a requested line.
    fn read_by_name(&self, file: &str, line: Option<usize>) -> Option<String> {
        let graph = self.index.readable.graph();
        let path = Path::new(file.trim_start_matches("./"));
        let modules: Vec<_> = graph.modules.values().filter(|m| m.file == path).collect();
        let file_level = graph.file_declarations(path);
        if modules.is_empty() && file_level.is_empty() {
            return None;
        }
        let covers = |span: &LineSpan, line: usize| span.start_line <= line && line <= span.end_line;
        let mut hint = format!(
            "`{file}` is in the code graph, so `read_file` does not read it. Read it by name: `read_contract_metadata(module)` for a contract's parents, declarations, state and functions, `read_callable_source(name, module)` for a function, `read_type_definition(name, module?, file?)` for a struct, enum, event, error, user-defined type or file-level constant{}.",
            if self.module_source {
                ", `read_module_source(module, start_line?, line_count?)` for contract source by line range"
            } else {
                ""
            }
        );
        if !modules.is_empty() {
            hint.push_str(&format!(
                " Contracts in this file: {}.",
                modules
                    .iter()
                    .map(|m| format!("{} (lines {}..{})", m.name, m.span.start_line, m.span.end_line))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !file_level.is_empty() {
            hint.push_str(&format!(
                " File-level declarations: {}.",
                file_level
                    .iter()
                    .map(|d| format!("{} {} (line {})", d.kind.render(), d.name, d.span.start_line))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let Some(line) = line {
            let module_name = |id: i64| graph.modules.get(&id).map(|m| m.name.clone()).unwrap_or_default();
            if let Some(c) = graph.callables.values().find(|c| c.file == path && covers(&c.span, line)) {
                hint.push_str(&format!(" Line {line} is in `{}.{}`.", module_name(c.module_id), c.name));
            } else if let Some(d) = graph
                .declarations
                .values()
                .find(|d| d.file == path && covers(&d.span, line))
            {
                hint.push_str(&format!(" Line {line} is in {} `{}`.", d.kind.render(), d.name));
            } else if let Some(m) = modules.iter().find(|m| covers(&m.span, line)) {
                hint.push_str(&format!(" Line {line} is in contract `{}`, outside its functions.", m.name));
            }
        }
        Some(hint)
    }
}

impl Tool for ReadFileTool {
    type ARGUMENTS = ReadFileArgs;
    const NAME: &str = "read_file";
    const DESCRIPTION: Option<&str> = Some(
        "Read a file under the project root that the code graph does not hold (configuration, remappings, documentation, vendored libraries outside the graph), whole or from `start_line` for `line_count` lines. Contracts, tests and mocks in the code graph are read by name with the code-graph tools. Other reproductions' harness files cannot be read.",
    );

    async fn invoke(&self, args: ReadFileArgs) -> Result<String, LLMYError> {
        let path = match self.files.resolve(&args.file_path) {
            Ok(path) => path,
            Err(refused) => return Ok(refused),
        };
        let start = args.start_line.unwrap_or(1);
        if start == 0 || args.line_count == Some(0) {
            return Ok("start_line and line_count must be at least 1".to_string());
        }
        if path.is_dir() {
            return Ok(format!("{} is a directory", args.file_path.display()));
        }
        if let Some(hint) = self.read_by_name(&self.files.relative(&path), args.start_line) {
            return Ok(hint);
        }
        let text = match tokio::fs::read(&path).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => return Ok(format!("cannot read {}: {e}", args.file_path.display())),
        };
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        if start > 1 && start > lines.len() {
            return Ok(format!(
                "start_line {start} is beyond the end of {}, which has {} lines",
                args.file_path.display(),
                lines.len()
            ));
        }
        let from = (start - 1).min(lines.len());
        let to = args
            .line_count
            .map(|n| from.saturating_add(n))
            .unwrap_or(lines.len())
            .min(lines.len());
        let mut out = lines[from..to].concat();
        if out.len() > self.files.max_read_bytes {
            let mut cut = self.files.max_read_bytes;
            while cut > 0 && !out.is_char_boundary(cut) {
                cut -= 1;
            }
            out.truncate(cut);
        }
        Ok(out)
    }
}

/// Arguments of `list_dir`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ListDirArgs {
    /// Directory to list, relative to the project root (`.` for the root).
    pub relative_path: PathBuf,
}

/// Lists the immediate entries of a project directory.
#[derive(Debug, Clone)]
pub struct ListDirTool {
    pub files: ProjectFiles,
}

impl Tool for ListDirTool {
    type ARGUMENTS = ListDirArgs;
    const NAME: &str = "list_dir";
    const DESCRIPTION: Option<&str> = Some(
        "List the entries of a directory under the project root as `name\\ttype\\tsize` rows. Other reproductions' harness directories are not listed.",
    );

    async fn invoke(&self, args: ListDirArgs) -> Result<String, LLMYError> {
        let dir = match self.files.resolve(&args.relative_path) {
            Ok(dir) => dir,
            Err(refused) => return Ok(refused),
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(format!("{} is not a directory", args.relative_path.display()));
        };
        let mut rows: Vec<String> = entries
            .flatten()
            .filter(|e| !self.files.fence.refuses(&e.path()))
            .map(|e| {
                let meta = e.metadata().ok();
                let kind = if meta.as_ref().is_some_and(|m| m.is_dir()) {
                    "dir"
                } else {
                    "file"
                };
                let size = meta.map(|m| m.len()).unwrap_or(0);
                format!("{}\t{kind}\t{size}", e.file_name().to_string_lossy())
            })
            .collect();
        rows.sort();
        Ok(format!(
            "The contents of folder {} is:\nname\ttype\tsize\n{}",
            args.relative_path.display(),
            rows.join("\n")
        ))
    }
}

/// Arguments of `find_file`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FindFileArgs {
    /// Directory to search recursively, relative to the project root.
    pub directory: PathBuf,
    /// Glob pattern matched against each file name, e.g. `*.sol`.
    pub file_name_pattern: String,
}

/// Finds project files by name pattern.
#[derive(Debug, Clone)]
pub struct FindFileTool {
    pub files: ProjectFiles,
}

impl Tool for FindFileTool {
    type ARGUMENTS = FindFileArgs;
    const NAME: &str = "find_file";
    const DESCRIPTION: Option<&str> = Some(
        "Recursively find files under a project directory whose names match a glob pattern. Other reproductions' harness directories are not searched.",
    );

    async fn invoke(&self, args: FindFileArgs) -> Result<String, LLMYError> {
        let dir = match self.files.resolve(&args.directory) {
            Ok(dir) => dir,
            Err(refused) => return Ok(refused),
        };
        let matcher = match Glob::new(&args.file_name_pattern) {
            Ok(glob) => glob.compile_matcher(),
            Err(e) => return Ok(format!("invalid glob pattern: {e}")),
        };
        let mut found = Vec::new();
        self.files.find(&dir, &matcher, &mut found);
        found.sort();
        Ok(format!(
            "The files found under directory {} with given pattern {} are:\n{}",
            args.directory.display(),
            args.file_name_pattern,
            found
                .iter()
                .map(|p| self.files.relative(p))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}

/// The project-rooted content search, with the lenient argument shape above; the search
/// itself is llmy's, with the fenced directories excluded.
#[derive(Debug, Clone)]
pub struct GrepTool {
    pub inner: GrepDirectoryTool,
    pub files: ProjectFiles,
}

impl Tool for GrepTool {
    type ARGUMENTS = GrepArgs;
    const NAME: &str = "grep";
    const DESCRIPTION: Option<&str> = Some(
        "Recursively searches a directory under the project root for lines matching a regular expression. Returns `file:line: text` matches.",
    );

    async fn invoke(&self, args: GrepArgs) -> Result<String, LLMYError> {
        if let Err(refused) = self.files.resolve(&args.directory) {
            return Ok(refused);
        }
        let mut exclude = args.exclude.map(Globs::into_vec).unwrap_or_default();
        exclude.extend(self.files.fence.exclude_globs());
        self.inner
            .grep_directory(GrepDirectoryArgs {
                directory: args.directory,
                pattern: args.pattern,
                invert_pattern: args.invert_pattern,
                include: args.include.map(Globs::into_vec).unwrap_or_default(),
                exclude,
                case_insensitive: args.case_insensitive,
                max_matches: args.max_matches,
            })
            .await
    }
}

/// The read-only project inspection tools every agent gets: code-graph queries plus file
/// access rooted at the project, behind `fence`.
#[derive(Debug, Clone)]
pub struct ProjectTools {
    pub index: Arc<CodeIndex>,
    pub fence: PathFence,
    /// Offer `read_module_source`.
    pub module_source: bool,
    /// Offer `read_file`.
    pub read_file: bool,
}

impl ProjectTools {
    pub fn tool_box(&self) -> ToolBox {
        let root = self.index.root.clone();
        let files = ProjectFiles {
            root: root.clone(),
            fence: self.fence.clone(),
            max_read_bytes: FileToolConfig::default().max_read_bytes,
        };
        // Sources are read by name from the readable graph, tests and mocks included.
        let mut tools = self.index.readable.tool_box_with(CodegraphToolOptions {
            module_source: self.module_source,
            ..CodegraphToolOptions::default()
        });
        // The overview already sits in every system prompt, and its argument-less schema is
        // rejected by some OpenAI-compatible gateways.
        tools.remove_tool("codegraph_overview");
        // Call edges and state accesses come from the project graph, so tests and mocks never
        // show up among a function's callers or a variable's writers.
        tools.remove_tool("lookup_callable");
        tools.remove_tool("lookup_state");
        tools.add_tool(LookupCallableTool::new(self.index.context.clone()));
        tools.add_tool(LookupStateTool::new(self.index.context.clone()));
        if self.read_file {
            tools.add_tool(ReadFileTool {
                files: files.clone(),
                index: self.index.clone(),
                module_source: self.module_source,
            });
        }
        tools.add_tool(ListDirTool {
            files: files.clone(),
        });
        tools.add_tool(FindFileTool {
            files: files.clone(),
        });
        tools.add_tool(GrepTool {
            inner: GrepDirectoryTool::new(root),
            files,
        });
        tools
    }

    /// The tool roster as the system prompt describes it.
    pub fn render_tool_guide(&self) -> String {
        format!(
            "Project inspection tools (the code graph holds the project's contracts, tests, mocks and scripts):\n\
             - `read_contract_metadata(module)`: a contract's parents and further ancestors, `using` directives, declared structs/enums/events/errors/user-defined types, state variables as declared, function signatures with their lines, and its file's file-level declarations.\n\
             - `read_callable_source(name, module?)`: a function's source, or several functions' given a list of names; `read_type_definition(name, module?, file?)`: a struct, enum, event, error, user-defined type or file-level constant.\n{}\
             - `lookup_callable(name, module?)`: callers, callees and state accesses of a project function; `lookup_state(name, module?)`: readers and writers of a project state variable.\n\
             - {}`list_dir(relative_path)`, `find_file(directory, file_name_pattern)`, `grep(directory, pattern, ...)`: any file under the project root, except other reproductions' harness files.",
            if self.module_source {
                "             - `read_module_source(module, start_line?, line_count?)`: a contract's source in capped line ranges.\n"
            } else {
                ""
            },
            if self.read_file {
                "`read_file(file_path, start_line?, line_count?)`: files the code graph does not hold (configuration, remappings, docs, vendored libraries); "
            } else {
                ""
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn include_accepts_a_string_or_a_list() {
        let one: GrepArgs =
            serde_json::from_str(r#"{"directory": ".", "pattern": "x", "include": "*.sol"}"#)
                .expect("string form parses");
        assert_eq!(
            one.include.map(Globs::into_vec),
            Some(vec!["*.sol".to_string()])
        );
        let many: GrepArgs = serde_json::from_str(
            r#"{"directory": ".", "pattern": "x", "include": ["a.sol", "b.sol"], "exclude": []}"#,
        )
        .expect("list form parses");
        assert_eq!(many.include.map(Globs::into_vec).map(|v| v.len()), Some(2));
        let schema = serde_json::to_value(schemars::schema_for!(GrepArgs)).expect("schema");
        let include = &schema["properties"]["include"];
        assert!(
            include.to_string().contains("anyOf"),
            "include schema must admit both shapes: {include}"
        );
    }

    #[tokio::test]
    async fn the_fence_keeps_other_harnesses_out_of_every_file_tool() {
        let root = std::env::temp_dir().join(format!("evolve-fence-{}", std::process::id()));
        let harness = root.join("test").join("evolve_harness");
        for (dir, file) in [
            (root.join("src"), "Vault.sol"),
            (root.join("test"), "Base.t.sol"),
            (harness.join("A_a0"), "Test_A.t.sol"),
            (harness.join("B_a0"), "Test_B.t.sol"),
        ] {
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join(file), "contract X {\n    function f() external {}\n}\n")
                .expect("write");
        }
        std::fs::write(root.join("foundry.toml"), "[profile.default]\n").expect("write");
        let layout = evolve_knowdit_types::FoundryLayout {
            root: root.clone(),
            src_dir: "src".to_string(),
            test_dir: "test".to_string(),
            via_ir: false,
            solc: None,
            remappings: Vec::new(),
            lib_dirs: Vec::new(),
        };
        let index = Arc::new(
            CodeIndex::build(
                &layout,
                &evolve_knowdit_types::ScopeContext::default(),
                None,
                std::slice::from_ref(&harness),
            )
            .await
            .expect("index"),
        );
        let files = ProjectFiles {
            root: root.clone(),
            fence: PathFence {
                hidden: vec![harness.clone()],
                own: Some(harness.join("A_a0")),
            },
            max_read_bytes: 8192,
        };
        let read = ReadFileTool {
            files: files.clone(),
            index,
            module_source: false,
        };
        let read_at = |p: &str| ReadFileArgs {
            file_path: PathBuf::from(p),
            start_line: None,
            line_count: None,
        };
        // The own harness is outside the readable graph, so read_file serves it.
        let own = read.invoke(read_at("test/evolve_harness/A_a0/Test_A.t.sol")).await.expect("read");
        assert!(own.starts_with("contract X"), "{own}");
        let other = read.invoke(read_at("test/evolve_harness/B_a0/Test_B.t.sol")).await.expect("read");
        assert!(other.contains("cannot be read"), "{other}");
        // Project sources and tests are in the readable graph: read by name instead.
        for file in ["src/Vault.sol", "test/Base.t.sol"] {
            let refused = read.invoke(read_at(file)).await.expect("read");
            assert!(refused.contains("is in the code graph"), "{refused}");
            assert!(refused.contains("Contracts in this file: X (lines 1..3)"), "{refused}");
            assert!(!refused.contains("read_module_source"), "{refused}");
        }
        let at_line = read
            .invoke(ReadFileArgs {
                file_path: PathBuf::from("src/Vault.sol"),
                start_line: Some(2),
                line_count: Some(1),
            })
            .await
            .expect("read");
        assert!(at_line.contains("Line 2 is in `X.f`"), "{at_line}");
        let config = read.invoke(read_at("foundry.toml")).await.expect("read");
        assert!(config.starts_with("[profile.default]"), "{config}");
        let list = ListDirTool { files: files.clone() }
            .invoke(ListDirArgs { relative_path: PathBuf::from("test/evolve_harness") })
            .await
            .expect("list");
        assert!(list.contains("cannot be read"), "{list}");
        let found = FindFileTool { files }
            .invoke(FindFileArgs {
                directory: PathBuf::from("."),
                file_name_pattern: "*.sol".to_string(),
            })
            .await
            .expect("find");
        assert!(found.contains("src/Vault.sol") && !found.contains("Test_B"), "{found}");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }
}
