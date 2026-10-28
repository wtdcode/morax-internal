use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, WrapErr};
use evolve_knowdit_types::{
    Dimension, FoundryLayout, ScopeContext, SourceAnchor, Subject, SubjectExclusion,
};
use itertools::Itertools;
use llmy::codegraph::model::{
    AccessKind, CallEdge, Callable, CallableKind, CalleeRef, CodeGraph, InheritEdge, LineSpan,
    Module, ModuleKind, ParentRef, StateEdge, StateItem,
};
use llmy::codegraph::{CodeGraphBuilder, CodegraphContext, SolcBuildInfo};

/// One candidate subject the source offers for a dimension: a contract, an entry point or a
/// state variable, with the text the agent sees for it.
#[derive(Debug, Clone)]
pub struct InventoryEntry {
    pub anchor: SourceAnchor,
    pub label: String,
}

impl std::fmt::Display for InventoryEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} — {}", self.anchor, self.label)
    }
}

/// How a function belongs to an extent. The variants are ordered from the closest relation to
/// the loosest; a function the extent reaches in several ways is listed under the closest one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExtentRelation {
    /// Declared by an anchored contract.
    Declared,
    /// Declared by a base the anchored contract `by` inherits.
    Inherited { by: String },
    /// Writes the anchored state variable `state` (`Contract.variable`).
    Writes { state: String },
    /// Reads the anchored state variable `state` without writing it.
    Reads { state: String },
    /// Declared by a contract that inherits the anchored contract `base`.
    Derived { base: String },
    /// Called directly (or through an interface it implements) by the extent function `from`.
    Called { from: String },
    /// A state-changing entry point whose calls reach the extent function `target`.
    Reaches { target: String },
}

/// One function of an extent: the functions of the contracts a subject or an obligation
/// anchors, of their bases and of the contracts deriving from them, the functions that read or
/// write an anchored state variable, the project functions they call in other contracts, and
/// the entry points that reach them. A cell's task names one of them as the function its
/// violation goes through.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExtentFunction {
    /// The declaring contract.
    pub contract: String,
    pub function: String,
    pub signature: String,
    pub line: usize,
    /// Read-only (`view` / `pure`): listed for completeness, no review required.
    pub read_only: bool,
    pub relation: ExtentRelation,
}

impl std::fmt::Display for ExtentFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}() line {}: {}", self.contract, self.function, self.line, self.signature)?;
        match &self.relation {
            ExtentRelation::Declared => Ok(()),
            ExtentRelation::Inherited { by } => write!(f, " — inherited by {by}"),
            ExtentRelation::Derived { base } => write!(f, " — in a contract inheriting {base}"),
            ExtentRelation::Writes { state } => write!(f, " — writes {state}"),
            ExtentRelation::Reads { state } => write!(f, " — reads {state}"),
            ExtentRelation::Called { from } => write!(f, " — called from {from}"),
            ExtentRelation::Reaches { target } => write!(f, " — entry point reaching {target}"),
        }
    }
}

/// The code graph restricted to the project's own sources, with every module marked in or out
/// of the engagement scope. Test, script and mock files are dropped from that graph, which every
/// analysis reads, but kept in `readable`, which the source-reading tools serve.
#[derive(Debug, Clone)]
pub struct CodeIndex {
    pub root: PathBuf,
    pub context: CodegraphContext,
    /// Every indexed file except the reproduction harnesses: the project sources plus its
    /// tests, mocks and scripts, read by module, function or declaration name.
    pub readable: CodegraphContext,
    /// Relative file → whether the engagement scope includes it.
    files: BTreeMap<String, bool>,
    /// First declaration line of every state item, to recognise `constant` / `immutable`.
    state_declarations: BTreeMap<i64, String>,
    module_files: BTreeMap<i64, String>,
    /// Every callable that reaches a call outside the project, directly or through project
    /// code it calls, with the leaf call it reaches.
    effectful: BTreeMap<i64, String>,
    /// Interface member → the project callables implementing it.
    implementations: BTreeMap<i64, Vec<i64>>,
    src_dir: String,
}

impl CodeIndex {
    /// Builds the index; with the compiler ASTs of the copy (`solc`) the Solidity files are
    /// indexed from the AST, otherwise from tree-sitter. Files under `hidden` (the reproduction
    /// harnesses) stay out of `readable`.
    pub async fn build(
        layout: &FoundryLayout,
        scope: &ScopeContext,
        solc: Option<SolcBuildInfo>,
        hidden: &[PathBuf],
    ) -> Result<Self> {
        let root = layout.root.clone();
        let mut builder = CodeGraphBuilder::new(root.clone());
        if let Some(solc) = solc {
            tracing::info!("code graph: {} compiled sources from the solc AST", solc.units.len());
            builder = builder.with_solc(solc);
        }
        let built = builder
            .build()
            .await
            .map_err(|e| color_eyre::eyre::eyre!("code graph build failed: {e}"))?;
        tracing::info!(
            "code graph: {} files parsed, {} parse errors, {}",
            built.files.len(),
            built.total_parse_errors(),
            built.graph.counts()
        );
        let hidden: Vec<PathBuf> = hidden
            .iter()
            .filter_map(|h| h.strip_prefix(&root).ok().map(Path::to_path_buf))
            .collect();
        // Audit snapshots and flattened copies duplicate the project's contracts under the same
        // names and would be served in place of the real sources.
        let readable_modules: BTreeSet<i64> = built
            .graph
            .modules
            .values()
            .filter(|m| !hidden.iter().any(|h| m.file.starts_with(h)))
            .filter(|m| {
                let lower = m.file.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
                !lower.starts_with("audits/")
                    && !lower.starts_with("audit/")
                    && !lower.starts_with("docs/")
                    && !lower.contains("flatten")
            })
            .map(|m| m.id)
            .collect();
        let readable = SourceFilter::retain(built.graph.clone(), &readable_modules);
        let filter = SourceFilter::new(layout);
        let filtered = filter.apply(built.graph);
        let mut files = BTreeMap::new();
        let mut module_files = BTreeMap::new();
        for module in filtered.modules.values() {
            let rel = module.file.to_string_lossy().to_string();
            files.insert(rel.clone(), scope.is_in_scope(&rel));
            module_files.insert(module.id, rel);
        }
        let mut sources: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
        let source_files = filtered
            .states
            .values()
            .map(|s| &s.file)
            .chain(filtered.callables.values().map(|c| &c.file));
        for file in source_files {
            if !sources.contains_key(file) {
                let text = tokio::fs::read_to_string(root.join(file))
                    .await
                    .wrap_err_with(|| format!("failed to read {}", file.display()))?;
                sources.insert(file.clone(), text.lines().map(String::from).collect());
            }
        }
        let mut state_declarations = BTreeMap::new();
        for state in filtered.states.values() {
            let line = sources
                .get(&state.file)
                .and_then(|lines| lines.get(state.span.start_line.saturating_sub(1)))
                .cloned()
                .unwrap_or_default();
            state_declarations.insert(state.id, line);
        }
        let implementations = Self::interface_implementations(&filtered);
        let effectful = Self::effectful_callables(&filtered, &sources, &implementations);
        let in_scope = files.values().filter(|v| **v).count();
        tracing::info!(
            "code index: {} project files ({} in scope), {}",
            files.len(),
            in_scope,
            filtered.counts()
        );
        Ok(Self {
            root: root.clone(),
            context: CodegraphContext::new(filtered, root.clone()),
            readable: CodegraphContext::new(readable, root),
            files,
            state_declarations,
            module_files,
            effectful,
            implementations,
            src_dir: layout.src_dir.clone(),
        })
    }

    /// The callables that perform a call outside the project, directly (an external member
    /// call whose behaviour matters, or a low-level call in Solidity or assembly) or through
    /// project code they call. The value names the leaf call, so a listing can say what a call
    /// into project code reaches.
    /// For every interface member, the project callables that implement it: same-named
    /// functions in non-interface modules that inherit the interface, or, failing that, in the
    /// module the interface names (`IVault` → `Vault`).
    fn interface_implementations(graph: &CodeGraph) -> BTreeMap<i64, Vec<i64>> {
        let mut out = BTreeMap::new();
        for member in graph.callables.values() {
            let Some(interface) = graph.modules.get(&member.module_id) else {
                continue;
            };
            if interface.kind != ModuleKind::Interface {
                continue;
            }
            let named = interface.name.strip_prefix('I').unwrap_or(&interface.name);
            let implementations: Vec<i64> = graph
                .callables
                .values()
                .filter(|c| c.name == member.name && c.module_id != member.module_id)
                .filter(|c| {
                    graph.modules.get(&c.module_id).is_some_and(|m| {
                        m.kind != ModuleKind::Interface
                            && (Self::ancestors(graph, m.id).contains(&interface.id)
                                || m.name == named)
                    })
                })
                .map(|c| c.id)
                .collect();
            if !implementations.is_empty() {
                out.insert(member.id, implementations);
            }
        }
        out
    }

    fn effectful_callables(
        graph: &CodeGraph,
        sources: &BTreeMap<PathBuf, Vec<String>>,
        implementations: &BTreeMap<i64, Vec<i64>>,
    ) -> BTreeMap<i64, String> {
        let qualified = |id: i64| -> String {
            let Some(callable) = graph.callables.get(&id) else {
                return id.to_string();
            };
            let module = graph
                .modules
                .get(&callable.module_id)
                .map(|m| m.name.as_str())
                .unwrap_or_default();
            format!("{module}.{}", callable.name)
        };
        let mut reasons: BTreeMap<i64, String> = BTreeMap::new();
        for callable in graph.callables.values() {
            // An interface member the project does not implement is a call to external
            // code; an implemented one inherits its implementation's effect below.
            if graph
                .modules
                .get(&callable.module_id)
                .is_some_and(|m| m.kind == ModuleKind::Interface)
            {
                if implementations.contains_key(&callable.id) {
                    continue;
                }
                reasons.insert(
                    callable.id,
                    format!(
                        "an external call through the interface `{}`",
                        qualified(callable.id)
                    ),
                );
                continue;
            }
            if let Some(lines) = sources.get(&callable.file)
                && let Some(op) = Self::low_level_call(lines, &callable.span)
            {
                reasons.insert(
                    callable.id,
                    format!("a low-level `{op}` in {}", qualified(callable.id)),
                );
            }
        }
        for edge in &graph.call_edges {
            if reasons.contains_key(&edge.caller_id) {
                continue;
            }
            if let CalleeRef::External(_) = &edge.callee {
                let callee = Self::callee_chain(&edge.callee_text);
                if Self::is_behavioural_call(&callee) {
                    reasons.insert(
                        edge.caller_id,
                        format!("`{callee}` in {}", qualified(edge.caller_id)),
                    );
                }
            }
        }
        // A call resolved to a base declaration runs whichever override the deployed contract
        // has, so a declaration inherits the effect of its overrides in descendant modules.
        let mut overrides: Vec<(i64, i64)> = implementations
            .iter()
            .flat_map(|(member, impls)| impls.iter().map(move |i| (*member, *i)))
            .collect();
        overrides.extend(graph
            .callables
            .values()
            .flat_map(|base| {
                graph
                    .callables
                    .values()
                    .filter(|other| {
                        other.id != base.id
                            && other.name == base.name
                            && Self::ancestors(graph, other.module_id).contains(&base.module_id)
                    })
                    .map(|other| (base.id, other.id))
                    .collect::<Vec<_>>()
            }));
        loop {
            let mut changed = false;
            for edge in &graph.call_edges {
                if reasons.contains_key(&edge.caller_id) {
                    continue;
                }
                let targets: Vec<i64> = match &edge.callee {
                    CalleeRef::Resolved(id) => vec![*id],
                    CalleeRef::Ambiguous(ids) => ids.clone(),
                    CalleeRef::External(_) => {
                        let name = Self::callee_chain(&edge.callee_text);
                        if name.contains('.') || name.is_empty() {
                            continue;
                        }
                        let Some(caller) = graph.callables.get(&edge.caller_id) else {
                            continue;
                        };
                        Self::callables_by_bare_name(graph, caller.module_id, &name)
                    }
                };
                if let Some(reason) = targets.iter().find_map(|id| reasons.get(id)).cloned() {
                    reasons.insert(edge.caller_id, reason);
                    changed = true;
                }
            }
            for (base, derived) in &overrides {
                if !reasons.contains_key(base)
                    && let Some(reason) = reasons.get(derived).cloned()
                {
                    reasons.insert(*base, reason);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        reasons
    }


    /// The first low-level call inside a callable's lines: `.call`, `.delegatecall`,
    /// `.staticcall` or `.send` in Solidity, `call(` / `delegatecall(` / `staticcall(` in
    /// assembly. Comment text is ignored.
    fn low_level_call(lines: &[String], span: &LineSpan) -> Option<String> {
        let start = span.start_line.saturating_sub(1);
        let end = span.end_line.min(lines.len());
        for line in lines.iter().take(end).skip(start) {
            let code = line.split("//").next().unwrap_or_default();
            for op in ["delegatecall", "staticcall", "call", "send"] {
                let needle = format!("{op}(");
                let mut from = 0;
                while let Some(pos) = code[from..].find(&needle) {
                    let at = from + pos;
                    let before = code[..at].chars().next_back();
                    let member = before == Some('.');
                    let bare = before.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
                    if member || (bare && op != "send") {
                        return Some(op.to_string());
                    }
                    from = at + needle.len();
                }
                if code.contains(&format!(".{op}{{")) {
                    return Some(op.to_string());
                }
            }
        }
        None
    }

    /// Transitive ancestors of a module through its inheritance edges, resolving a parent the
    /// graph only knows by name (a base declared in another file) to the unique module of that
    /// name.
    fn ancestors(graph: &CodeGraph, module_id: i64) -> BTreeSet<i64> {
        let mut out = BTreeSet::new();
        let mut frontier = vec![module_id];
        while let Some(current) = frontier.pop() {
            for edge in graph.inherit_edges.iter().filter(|e| e.module_id == current) {
                let parent = match &edge.parent {
                    ParentRef::Resolved(id) => Some(*id),
                    ParentRef::External(name) => {
                        let mut named = graph.modules.values().filter(|m| &m.name == name);
                        match (named.next(), named.next()) {
                            (Some(only), None) => Some(only.id),
                            _ => None,
                        }
                    }
                };
                if let Some(parent) = parent
                    && out.insert(parent)
                {
                    frontier.push(parent);
                }
            }
        }
        out
    }

    /// Project callables a bare (unqualified) call site `name` from `caller_module` can run:
    /// same-named callables in that module's ancestors or descendants. A base declaration
    /// without a body leaves such a call unresolved in the graph, so this finds the overrides.
    fn callables_by_bare_name(graph: &CodeGraph, caller_module: i64, name: &str) -> Vec<i64> {
        let ancestors = Self::ancestors(graph, caller_module);
        graph
            .callables
            .values()
            .filter(|c| c.name == name)
            .filter(|c| {
                c.module_id == caller_module
                    || ancestors.contains(&c.module_id)
                    || Self::ancestors(graph, c.module_id).contains(&caller_module)
            })
            .map(|c| c.id)
            .collect()
    }



    fn callee_chain(text: &str) -> String {
        // Call options (`f{value: x, gas: y}`) sit between the member chain and the arguments.
        let text = match (text.trim_end().ends_with('}'), text.rfind('{')) {
            (true, Some(open)) => &text[..open],
            _ => text,
        };
        let joined = text.split('.').map(str::trim).collect::<Vec<_>>().join(".");
        let bytes = joined.as_bytes();
        let mut i = bytes.len();
        let mut depth = 0usize;
        while i > 0 {
            let c = bytes[i - 1] as char;
            if c == ')' {
                depth += 1;
            } else if c == '(' {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            } else if depth == 0 && !(c.is_ascii_alphanumeric() || c == '_' || c == '.') {
                break;
            }
            i -= 1;
        }
        joined[i..].trim_matches('.').to_string()
    }

    pub fn in_scope_files(&self) -> Vec<String> {
        self.files
            .iter()
            .filter(|(_, v)| **v)
            .map(|(k, _)| k.clone())
            .collect()
    }

    fn module_in_scope(&self, module: &Module) -> bool {
        self.module_files
            .get(&module.id)
            .map(|f| self.files.get(f).copied().unwrap_or(false))
            .unwrap_or(false)
    }

    fn state_is_constant(&self, state: &StateItem) -> bool {
        self.state_declarations
            .get(&state.id)
            .map(|line| {
                let tokens: BTreeSet<&str> = line.split(|c: char| !c.is_alphanumeric()).collect();
                tokens.contains("constant") || tokens.contains("immutable")
            })
            .unwrap_or(false)
    }

    fn callable_is_entry(callable: &Callable) -> bool {
        let sig = &callable.signature;
        let tokens: BTreeSet<&str> = sig.split(|c: char| !c.is_alphanumeric()).collect();
        let reachable = tokens.contains("external")
            || tokens.contains("public")
            || matches!(
                callable.kind,
                CallableKind::Fallback | CallableKind::Receive
            );
        let mutating = !(tokens.contains("view") || tokens.contains("pure"));
        matches!(
            callable.kind,
            CallableKind::Function | CallableKind::Fallback | CallableKind::Receive
        ) && reachable
            && mutating
    }

    /// The project map an agent sees in its system prompt.
    pub fn render_overview(&self) -> String {
        let graph = self.context.graph();
        let mut lines = vec![format!(
            "Indexed project sources under `{}`: {}. Files marked [out of scope] belong to the project but the engagement excludes them; subjects must anchor in-scope files.",
            self.src_dir,
            graph.counts()
        )];
        for module in graph.modules.values() {
            let scope = if self.module_in_scope(module) {
                "in scope"
            } else {
                "out of scope"
            };
            let callables = graph.callables_of_module(module.id);
            let entries = callables
                .iter()
                .filter(|c| Self::callable_is_entry(c))
                .count();
            let states = graph.states_of_module(module.id);
            lines.push(format!(
                "- {} {} ({}) [{}]: {} callables ({} state-changing entry points), {} state variables",
                module.kind.render(),
                module.name,
                module.file.display(),
                scope,
                callables.len(),
                entries,
                states.len()
            ));
        }
        lines.join("\n")
    }

    /// Candidate subjects the source offers for a dimension: contracts for the module
    /// dimension, state-changing entry points for operation flow, storage variables that are
    /// neither constant nor immutable for asset lifecycle.
    pub fn inventory(&self, dimension: Dimension) -> Vec<InventoryEntry> {
        let graph = self.context.graph();
        let mut out = Vec::new();
        for module in graph.modules.values() {
            if !self.module_in_scope(module) || module.kind == ModuleKind::Interface {
                continue;
            }
            let file = module.file.to_string_lossy().to_string();
            match dimension {
                Dimension::Module => out.push(InventoryEntry {
                    anchor: SourceAnchor {
                        file,
                        contract: Some(module.name.clone()),
                        function: None,
                        state: None,
                    },
                    label: format!(
                        "{} with {} callables and {} state variables",
                        module.kind.render(),
                        graph.callables_of_module(module.id).len(),
                        graph.states_of_module(module.id).len()
                    ),
                }),
                Dimension::OperationFlow => {
                    if module.kind == ModuleKind::Library {
                        continue;
                    }
                    for callable in graph.callables_of_module(module.id) {
                        if !Self::callable_is_entry(callable) {
                            continue;
                        }
                        out.push(InventoryEntry {
                            anchor: SourceAnchor {
                                file: file.clone(),
                                contract: Some(module.name.clone()),
                                function: Some(callable.name.clone()),
                                state: None,
                            },
                            label: callable.signature.chars().take(160).collect(),
                        });
                    }
                }
                Dimension::AssetLifecycle => {
                    for state in graph.states_of_module(module.id) {
                        if self.state_is_constant(state) {
                            continue;
                        }
                        out.push(InventoryEntry {
                            anchor: SourceAnchor {
                                file: file.clone(),
                                contract: Some(module.name.clone()),
                                function: None,
                                state: Some(state.name.clone()),
                            },
                            label: state.type_text.clone(),
                        });
                    }
                }
            }
        }
        out
    }

    pub fn render_inventory(&self, dimension: Dimension) -> String {
        let entries = self.inventory(dimension);
        if entries.is_empty() {
            return "(the inventory is empty)".to_string();
        }
        let mut lines = Vec::new();
        for ((file, contract), group) in &entries
            .iter()
            .chunk_by(|e| (e.anchor.file.clone(), e.anchor.contract.clone()))
        {
            lines.push(format!(
                "{} ({}):",
                contract.unwrap_or_else(|| "<file>".into()),
                file
            ));
            for entry in group {
                let leaf = entry
                    .anchor
                    .function
                    .as_ref()
                    .map(|f| format!("{f}()"))
                    .or_else(|| entry.anchor.state.clone())
                    .unwrap_or_default();
                if leaf.is_empty() {
                    lines.push(format!("  {}", entry.label));
                } else {
                    lines.push(format!("  - {leaf}: {}", entry.label));
                }
            }
        }
        lines.join("\n")
    }

    /// Whether an unresolved call site is a call into other code whose behaviour matters, as
    /// opposed to a type conversion, an ABI helper or a pure math or string library.
    fn is_behavioural_call(callee: &str) -> bool {
        let Some((head, last)) = callee.rsplit_once('.') else {
            return false;
        };
        let first = head.split(['.', '(']).next().unwrap_or("");
        const PURE: [&str; 12] = [
            "abi", "type", "Math", "SafeMath", "FixedPointMathLib", "FullMath", "SafeCast",
            "SafeCastLib", "Strings", "LibString", "SignedMath", "MathUpgradeable",
        ];
        !first.is_empty()
            && !PURE.contains(&first)
            && last.chars().next().is_some_and(|c| c.is_ascii_lowercase())
    }

    fn modules_in_file(&self, file: &str) -> Vec<&Module> {
        self.context
            .graph()
            .modules
            .values()
            .filter(|m| m.file.to_string_lossy() == file)
            .collect()
    }



    /// The modules an extent names (every contract of an anchored file when the anchor gives
    /// no contract).
    fn extent_modules(&self, extent: &[SourceAnchor]) -> Vec<&Module> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for anchor in extent {
            let file = anchor.file.trim_start_matches("./");
            for module in self.modules_in_file(file) {
                if let Some(contract) = &anchor.contract
                    && &module.name != contract
                {
                    continue;
                }
                if seen.insert(module.id) {
                    out.push(module);
                }
            }
        }
        out
    }

    /// The functions an extent reaches, one entry per function under its closest relation:
    /// every function of the anchored contracts (all visibilities), of the in-scope bases they
    /// inherit and of the in-scope contracts inheriting them; the in-scope functions that write
    /// or read an anchored state variable; one hop of calls from those functions into other
    /// in-scope contracts, an interface call standing for its implementations; and the in-scope
    /// state-changing entry points whose calls reach any of them but those callees. Listed by
    /// declaring contract, name and line.
    pub fn extent_functions(&self, extent: &[SourceAnchor]) -> Vec<ExtentFunction> {
        let graph = self.context.graph();
        let in_scope = |callable: &Callable| {
            graph
                .modules
                .get(&callable.module_id)
                .is_some_and(|m| self.module_in_scope(m))
        };
        let add = |out: &mut BTreeMap<i64, ExtentFunction>, callable: &Callable, relation| {
            let listed = matches!(
                callable.kind,
                CallableKind::Function | CallableKind::Fallback | CallableKind::Receive
            );
            let Some(module) = graph.modules.get(&callable.module_id) else {
                return;
            };
            if !listed || module.kind == ModuleKind::Interface {
                return;
            }
            if out
                .get(&callable.id)
                .is_some_and(|existing| existing.relation <= relation)
            {
                return;
            }
            let tokens: BTreeSet<&str> = callable
                .signature
                .split(|c: char| !c.is_alphanumeric())
                .collect();
            out.insert(
                callable.id,
                ExtentFunction {
                    contract: module.name.clone(),
                    function: callable.name.clone(),
                    signature: callable.signature.chars().take(160).collect(),
                    line: callable.span.start_line,
                    read_only: tokens.contains("view") || tokens.contains("pure"),
                    relation,
                },
            );
        };
        let mut out: BTreeMap<i64, ExtentFunction> = BTreeMap::new();
        let modules: Vec<&Module> = self
            .extent_modules(extent)
            .into_iter()
            .filter(|m| m.kind != ModuleKind::Interface)
            .collect();
        for module in &modules {
            for callable in graph.callables_of_module(module.id) {
                add(&mut out, callable, ExtentRelation::Declared);
            }
            for parent in Self::ancestors(graph, module.id) {
                let Some(base) = graph.modules.get(&parent) else {
                    continue;
                };
                if !self.module_in_scope(base) {
                    continue;
                }
                for callable in graph.callables_of_module(base.id) {
                    add(
                        &mut out,
                        callable,
                        ExtentRelation::Inherited {
                            by: module.name.clone(),
                        },
                    );
                }
            }
        }
        for child in graph.modules.values() {
            if !self.module_in_scope(child) {
                continue;
            }
            let ancestors = Self::ancestors(graph, child.id);
            for module in modules.iter().filter(|m| ancestors.contains(&m.id)) {
                for callable in graph.callables_of_module(child.id) {
                    add(
                        &mut out,
                        callable,
                        ExtentRelation::Derived {
                            base: module.name.clone(),
                        },
                    );
                }
            }
        }
        for anchor in extent {
            let Some(name) = &anchor.state else {
                continue;
            };
            for module in self.extent_modules(std::slice::from_ref(anchor)) {
                for state in graph.states_of_module(module.id) {
                    if &state.name != name {
                        continue;
                    }
                    let label = format!("{}.{}", module.name, state.name);
                    for edge in graph.accessors_of_state(state.id) {
                        let Some(callable) = graph.callables.get(&edge.callable_id) else {
                            continue;
                        };
                        if !in_scope(callable) {
                            continue;
                        }
                        let relation = match edge.access {
                            AccessKind::Write => ExtentRelation::Writes {
                                state: label.clone(),
                            },
                            AccessKind::Read => ExtentRelation::Reads {
                                state: label.clone(),
                            },
                        };
                        add(&mut out, callable, relation);
                    }
                }
            }
        }
        // One hop of calls from the extent's own code (not from what it merely inherits).
        let core: BTreeSet<i64> = out
            .iter()
            .filter(|(_, f)| !matches!(f.relation, ExtentRelation::Inherited { .. }))
            .map(|(id, _)| *id)
            .collect();
        for edge in &graph.call_edges {
            if !core.contains(&edge.caller_id) {
                continue;
            }
            let from = self.qualified(edge.caller_id);
            for id in self.edge_targets(edge) {
                let Some(callee) = graph.callables.get(&id) else {
                    continue;
                };
                if in_scope(callee) {
                    add(&mut out, callee, ExtentRelation::Called { from: from.clone() });
                }
            }
        }
        // The state-changing entry points whose calls reach the extent's code (everything listed
        // so far but its callees) through any chain of project calls.
        let mut callers: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
        for edge in &graph.call_edges {
            for id in self.edge_targets(edge) {
                callers.entry(id).or_default().push(edge.caller_id);
            }
        }
        let mut frontier: Vec<(i64, String)> = out
            .iter()
            .filter(|(_, f)| !matches!(f.relation, ExtentRelation::Called { .. }))
            .map(|(id, f)| (*id, format!("{}.{}", f.contract, f.function)))
            .collect();
        let mut reached: BTreeMap<i64, String> = BTreeMap::new();
        while let Some((id, target)) = frontier.pop() {
            for caller in callers.get(&id).into_iter().flatten() {
                if out.contains_key(caller) || reached.contains_key(caller) {
                    continue;
                }
                reached.insert(*caller, target.clone());
                frontier.push((*caller, target.clone()));
            }
        }
        for (id, target) in reached {
            let Some(callable) = graph.callables.get(&id) else {
                continue;
            };
            if Self::callable_is_entry(callable) && in_scope(callable) {
                add(&mut out, callable, ExtentRelation::Reaches { target });
            }
        }
        let mut listed: Vec<ExtentFunction> = out.into_values().collect();
        listed.sort();
        listed
    }

    /// The project callables a call edge can run: its resolved callee or candidates, with an
    /// interface member standing for the project callables that implement it.
    fn edge_targets(&self, edge: &CallEdge) -> Vec<i64> {
        let ids = match &edge.callee {
            CalleeRef::Resolved(id) => vec![*id],
            CalleeRef::Ambiguous(ids) => ids.clone(),
            CalleeRef::External(_) => Vec::new(),
        };
        ids.into_iter()
            .flat_map(|id| {
                self.implementations
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| vec![id])
            })
            .collect()
    }

    /// The functions one obligation anchor names: for a function anchor, that function of the
    /// anchored contract (declared there or inherited, matched by name); for a contract or file
    /// anchor, every function declared or inherited there; for a state anchor, those and the
    /// functions anywhere in scope that write or read it. Callees and entry points are not
    /// included: an anchor names its own code, and whether that code belongs to a subject is
    /// decided against the subject's extent.
    pub fn anchored_functions(&self, anchors: &[SourceAnchor]) -> Vec<ExtentFunction> {
        let mut out: BTreeSet<ExtentFunction> = BTreeSet::new();
        for anchor in anchors {
            for f in self.extent_functions(std::slice::from_ref(anchor)) {
                let named = anchor.function.as_ref().is_none_or(|name| name == &f.function);
                let bound = match &f.relation {
                    ExtentRelation::Writes { .. } | ExtentRelation::Reads { .. } => true,
                    ExtentRelation::Declared => named,
                    ExtentRelation::Inherited { by } => {
                        named && anchor.contract.as_deref().is_none_or(|c| c == by)
                    }
                    _ => false,
                };
                if bound {
                    out.insert(f);
                }
            }
        }
        out.into_iter().collect()
    }

    /// The function anchors of one operation subject partitioned by settlement path: the
    /// overloads of one function are one entry, and two entries share a group when one calls
    /// the other directly (a thin wrapper), an interface call standing for its implementations.
    /// Each group lists its entries as `Contract.function`.
    pub fn settlement_groups(&self, anchors: &[SourceAnchor]) -> Vec<Vec<String>> {
        let graph = self.context.graph();
        let mut entries: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
        for anchor in anchors {
            let Some(name) = &anchor.function else {
                continue;
            };
            for module in self.extent_modules(std::slice::from_ref(anchor)) {
                let ids = entries
                    .entry(format!("{}.{name}", module.name))
                    .or_default();
                for parent in std::iter::once(module.id).chain(Self::ancestors(graph, module.id)) {
                    ids.extend(
                        graph
                            .callables_of_module(parent)
                            .iter()
                            .filter(|c| &c.name == name)
                            .map(|c| c.id),
                    );
                }
            }
        }
        let labels: Vec<&String> = entries.keys().collect();
        // Union of entries linked by a direct call in either direction.
        let mut group: Vec<usize> = (0..labels.len()).collect();
        for edge in &graph.call_edges {
            let targets = self.edge_targets(edge);
            let Some(from) = labels.iter().position(|l| entries[*l].contains(&edge.caller_id)) else {
                continue;
            };
            for (to, label) in labels.iter().enumerate() {
                if to == from || !targets.iter().any(|t| entries[*label].contains(t)) {
                    continue;
                }
                let (keep, merge) = (group[from].min(group[to]), group[from].max(group[to]));
                for g in group.iter_mut().filter(|g| **g == merge) {
                    *g = keep;
                }
            }
        }
        let mut out: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for (i, label) in labels.iter().enumerate() {
            out.entry(group[i]).or_default().push((*label).clone());
        }
        out.into_values().collect()
    }

    /// Every `(contract, function)` the project exposes: the functions each project contract
    /// declares or inherits from project bases.
    pub fn exposed_functions(&self) -> BTreeSet<(String, String)> {
        let graph = self.context.graph();
        let mut out = BTreeSet::new();
        for module in graph.modules.values() {
            for owner in std::iter::once(module.id).chain(Self::ancestors(graph, module.id)) {
                for callable in graph.callables_of_module(owner) {
                    out.insert((module.name.clone(), callable.name.clone()));
                }
            }
        }
        out
    }

    /// The anchor of `function` as `contract` exposes it: the file and contract that declare
    /// it (`contract` itself, else a project base declaring it). `None` when the project has no
    /// such function.
    pub fn function_anchor(&self, contract: &str, function: &str) -> Option<SourceAnchor> {
        let graph = self.context.graph();
        for module in graph.modules_by_name(contract) {
            for owner in std::iter::once(module.id).chain(Self::ancestors(graph, module.id)) {
                let Some(declaring) = graph.modules.get(&owner) else {
                    continue;
                };
                if graph
                    .callables_of_module(owner)
                    .iter()
                    .any(|c| c.name == function)
                {
                    return Some(SourceAnchor {
                        file: declaring.file.to_string_lossy().to_string(),
                        contract: Some(declaring.name.clone()),
                        function: Some(function.to_string()),
                        state: None,
                    });
                }
            }
        }
        None
    }

    /// The file and line span of every overload of `function` as `contract` exposes it (declared
    /// by `contract` or a project base). When `contract` exposes none, every project function of
    /// that name, so a step naming an interface or a misnamed contract still finds the code.
    pub fn function_spans(&self, contract: &str, function: &str) -> Vec<(String, LineSpan)> {
        let graph = self.context.graph();
        let spans = |callables: Vec<&Callable>| -> Vec<(String, LineSpan)> {
            callables
                .into_iter()
                .filter(|c| c.name == function)
                .map(|c| (c.file.to_string_lossy().to_string(), c.span))
                .collect()
        };
        for module in graph.modules_by_name(contract) {
            for owner in std::iter::once(module.id).chain(Self::ancestors(graph, module.id)) {
                let found = spans(graph.callables_of_module(owner));
                if !found.is_empty() {
                    return found;
                }
            }
        }
        spans(graph.callables.values().collect())
    }

    /// Every call edge of `Contract.function` with how the graph resolved it, one line each;
    /// for inspecting why a call is or is not on a review list.
    pub fn calls_of(&self, contract: &str, function: &str) -> Vec<String> {
        let graph = self.context.graph();
        let mut out = Vec::new();
        for module in graph.modules.values().filter(|m| m.name == contract) {
            let ancestors: Vec<String> = graph
                .ancestors_of(module.id)
                .iter()
                .filter_map(|id| graph.modules.get(id).map(|m| m.name.clone()))
                .collect();
            let descendants: Vec<String> = graph
                .modules
                .values()
                .filter(|m| Self::ancestors(graph, m.id).contains(&module.id))
                .map(|m| m.name.clone())
                .collect();
            let raw_parents: Vec<String> = graph
                .inherit_edges
                .iter()
                .filter(|e| e.module_id == module.id)
                .map(|e| format!("{:?}", e.parent))
                .collect();
            out.push(format!(
                "{contract}: parents {:?}, ancestors [{}], descendants [{}]",
                raw_parents,
                ancestors.join(", "),
                descendants.join(", ")
            ));
            for callable in graph.callables_of_module(module.id) {
                if callable.name != function {
                    continue;
                }
                let same_name: Vec<String> = graph
                    .callables
                    .values()
                    .filter(|c| c.id != callable.id && c.name == callable.name)
                    .map(|c| {
                        format!(
                            "{}{}",
                            self.qualified(c.id),
                            self.effectful
                                .get(&c.id)
                                .map(|e| format!(" (reaches {e})"))
                                .unwrap_or_default()
                        )
                    })
                    .collect();
                out.push(format!(
                    "{contract}.{function}: same-named callables elsewhere [{}]{}",
                    same_name.join(", "),
                    self.effectful
                        .get(&callable.id)
                        .map(|e| format!("; itself reaches {e}"))
                        .unwrap_or_default()
                ));
                for edge in &graph.call_edges {
                    if edge.caller_id != callable.id {
                        continue;
                    }
                    let resolution = match &edge.callee {
                        CalleeRef::External(text) => format!("external `{text}`"),
                        CalleeRef::Resolved(id) => format!("resolved {}", self.qualified(*id)),
                        CalleeRef::Ambiguous(ids) => format!(
                            "ambiguous {}",
                            ids.iter().map(|id| self.qualified(*id)).join(" | ")
                        ),
                    };
                    let effect = match &edge.callee {
                        CalleeRef::Resolved(id) => self.effectful.get(id).cloned(),
                        CalleeRef::Ambiguous(ids) => ids.iter().find_map(|id| self.effectful.get(id)).cloned(),
                        CalleeRef::External(_) => None,
                    };
                    out.push(format!(
                        "line {}: `{}` → {}{}",
                        edge.line,
                        Self::callee_chain(&edge.callee_text),
                        resolution,
                        effect.map(|e| format!(" (reaches {e})")).unwrap_or_default()
                    ));
                }
            }
        }
        out
    }

    /// `Contract.function` of a callable id.
    fn qualified(&self, id: i64) -> String {
        let graph = self.context.graph();
        let Some(callable) = graph.callables.get(&id) else {
            return id.to_string();
        };
        let module = graph
            .modules
            .get(&callable.module_id)
            .map(|m| m.name.as_str())
            .unwrap_or_default();
        format!("{module}.{}", callable.name)
    }

    /// Why an anchor does not point at indexed project code, if it does not. With
    /// `require_in_scope` the file must also be inside the engagement scope.
    pub fn anchor_error(&self, anchor: &SourceAnchor, require_in_scope: bool) -> Option<String> {
        let file = anchor.file.trim_start_matches("./");
        let Some(in_scope) = self.files.get(file) else {
            let hint = self
                .files
                .keys()
                .filter(|f| Path::new(f).file_name() == Path::new(file).file_name())
                .take(3)
                .cloned()
                .collect::<Vec<_>>();
            return Some(format!(
                "`{}` is not an indexed project source file{}",
                anchor.file,
                if hint.is_empty() {
                    String::new()
                } else {
                    format!(" (did you mean {})", hint.join(" or "))
                }
            ));
        };
        if require_in_scope && !in_scope {
            return Some(format!(
                "`{}` is out of the engagement scope; subjects must anchor in-scope files",
                anchor.file
            ));
        }
        let Some(contract) = &anchor.contract else {
            return None;
        };
        let modules = self.modules_in_file(file);
        let Some(module) = modules.iter().find(|m| &m.name == contract) else {
            return Some(format!(
                "`{}` declares no contract named `{contract}` (it declares {})",
                anchor.file,
                modules.iter().map(|m| m.name.as_str()).join(", ")
            ));
        };
        let graph = self.context.graph();
        if let Some(function) = &anchor.function
            && !graph
                .callables_of_module(module.id)
                .iter()
                .any(|c| &c.name == function)
        {
            return Some(format!(
                "`{contract}` has no function `{function}` in `{}`",
                anchor.file
            ));
        }
        if let Some(state) = &anchor.state
            && !graph
                .states_of_module(module.id)
                .iter()
                .any(|s| &s.name == state)
        {
            return Some(format!(
                "`{contract}` has no state variable `{state}` in `{}`",
                anchor.file
            ));
        }
        None
    }

    fn anchor_covers(anchor: &SourceAnchor, entry: &SourceAnchor, dimension: Dimension) -> bool {
        if anchor.file.trim_start_matches("./") != entry.file || anchor.contract != entry.contract {
            return false;
        }
        match dimension {
            Dimension::Module => true,
            Dimension::OperationFlow => anchor.function == entry.function,
            Dimension::AssetLifecycle => anchor.state == entry.state,
        }
    }

    /// Inventory entries no subject anchors and no subject exclusion names: the completeness
    /// gaps a draft must close before it is accepted.
    pub fn completeness_gaps(
        &self,
        dimension: Dimension,
        subjects: &[Subject],
        exclusions: &[SubjectExclusion],
    ) -> Vec<InventoryEntry> {
        self.inventory(dimension)
            .into_iter()
            .filter(|entry| {
                let anchored = subjects
                    .iter()
                    .flat_map(|s| s.anchors.iter())
                    .any(|a| Self::anchor_covers(a, &entry.anchor, dimension));
                let excluded = exclusions
                    .iter()
                    .any(|e| Self::anchor_covers(&e.anchor, &entry.anchor, dimension));
                !anchored && !excluded
            })
            .collect()
    }
}

/// Keeps only modules that belong to the project's own sources.
struct SourceFilter {
    src_prefix: String,
    test_prefix: String,
    lib_prefixes: Vec<String>,
}

impl SourceFilter {
    fn new(layout: &FoundryLayout) -> Self {
        let norm = |s: &str| format!("{}/", s.trim_matches('/'));
        Self {
            src_prefix: norm(&layout.src_dir),
            test_prefix: norm(&layout.test_dir),
            lib_prefixes: layout.lib_dirs.iter().map(|l| norm(l)).collect(),
        }
    }

    fn keep(&self, file: &Path, strict: bool) -> bool {
        let rel = file.to_string_lossy().replace('\\', "/");
        if rel.ends_with(".t.sol") || rel.ends_with(".s.sol") {
            return false;
        }
        let lower = rel.to_ascii_lowercase();
        if rel.starts_with(&self.test_prefix)
            || self.lib_prefixes.iter().any(|p| rel.starts_with(p))
            || rel.starts_with("lib/")
            || rel.starts_with("node_modules/")
            || rel.starts_with("out/")
            || rel.starts_with("cache/")
            || rel.starts_with("script/")
            || rel.starts_with("scripts/")
            || rel.starts_with("test/")
            || rel.starts_with("tests/")
            || lower.contains("/mocks/")
            || lower.contains("/mock/")
            || lower.contains("/test/")
            || lower.contains("/tests/")
            || lower.contains("/script/")
        {
            return false;
        }
        if self.src_prefix != "/" && !rel.starts_with(&self.src_prefix) {
            // Only the lenient pass, taken when the configured src dir holds nothing, admits
            // sources outside it; audits, docs, flattened copies and vendored trees stay out.
            return !strict
                && !rel.contains("node_modules/")
                && !rel.contains("/lib/")
                && !lower.starts_with("audits/")
                && !lower.starts_with("audit/")
                && !lower.starts_with("docs/")
                && !lower.contains("flatten");
        }
        true
    }

    fn apply(&self, graph: CodeGraph) -> CodeGraph {
        let strict: BTreeSet<i64> = graph
            .modules
            .values()
            .filter(|m| self.keep(&m.file, true))
            .map(|m| m.id)
            .collect();
        let kept_modules: BTreeSet<i64> = if strict.is_empty() {
            tracing::warn!(
                "no Solidity sources under `{}`; indexing every non-vendored source instead",
                self.src_prefix
            );
            graph
                .modules
                .values()
                .filter(|m| self.keep(&m.file, false))
                .map(|m| m.id)
                .collect()
        } else {
            strict
        };
        Self::retain(graph, &kept_modules)
    }

    /// `graph` restricted to `kept_modules`: their callables, state items and declarations
    /// (a file-level one when a kept module shares its file) and the edges among kept items;
    /// an edge to a dropped item turns external.
    fn retain(graph: CodeGraph, kept_modules: &BTreeSet<i64>) -> CodeGraph {
        let mut out = CodeGraph::default();
        // Names of dropped modules: an inheritance edge that pointed at a dropped copy of a
        // contract (a vendored or test duplicate) keeps the name, so the kept copy resolves.
        let mut dropped_names: BTreeMap<i64, String> = BTreeMap::new();
        for (id, module) in graph.modules {
            if kept_modules.contains(&id) {
                out.modules.insert(id, module);
            } else {
                dropped_names.insert(id, module.name);
            }
        }
        let mut kept_callables = BTreeSet::new();
        for (id, callable) in graph.callables {
            if kept_modules.contains(&callable.module_id) {
                kept_callables.insert(id);
                out.callables.insert(id, callable);
            }
        }
        let mut kept_states = BTreeSet::new();
        for (id, state) in graph.states {
            if kept_modules.contains(&state.module_id) {
                kept_states.insert(id);
                out.states.insert(id, state);
            }
        }
        for edge in graph.call_edges {
            if !kept_callables.contains(&edge.caller_id) {
                continue;
            }
            let callee = match edge.callee {
                CalleeRef::Resolved(id) if kept_callables.contains(&id) => CalleeRef::Resolved(id),
                CalleeRef::Resolved(_) => CalleeRef::External(edge.callee_text.clone()),
                CalleeRef::Ambiguous(ids) => {
                    let ids: Vec<i64> = ids
                        .into_iter()
                        .filter(|id| kept_callables.contains(id))
                        .collect();
                    match ids.len() {
                        0 => CalleeRef::External(edge.callee_text.clone()),
                        1 => CalleeRef::Resolved(ids[0]),
                        _ => CalleeRef::Ambiguous(ids),
                    }
                }
                CalleeRef::External(text) => CalleeRef::External(text),
            };
            out.call_edges.push(CallEdge {
                caller_id: edge.caller_id,
                callee,
                callee_text: edge.callee_text,
                line: edge.line,
            });
        }
        for edge in graph.state_edges {
            if kept_callables.contains(&edge.callable_id) && kept_states.contains(&edge.state_id) {
                out.state_edges.push(StateEdge {
                    callable_id: edge.callable_id,
                    state_id: edge.state_id,
                    access: edge.access,
                    line: edge.line,
                });
            }
        }
        for edge in graph.inherit_edges {
            if !kept_modules.contains(&edge.module_id) {
                continue;
            }
            let parent = match edge.parent {
                ParentRef::Resolved(id) if kept_modules.contains(&id) => ParentRef::Resolved(id),
                ParentRef::Resolved(id) => ParentRef::External(
                    dropped_names
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(|| format!("<dropped module {id}>")),
                ),
                ParentRef::External(name) => ParentRef::External(name),
            };
            out.inherit_edges.push(InheritEdge {
                module_id: edge.module_id,
                parent,
            });
        }
        let kept_files: BTreeSet<PathBuf> = out.modules.values().map(|m| m.file.clone()).collect();
        for (id, declaration) in graph.declarations {
            let kept = match declaration.module_id {
                Some(module_id) => kept_modules.contains(&module_id),
                None => kept_files.contains(&declaration.file),
            };
            if kept {
                out.declarations.insert(id, declaration);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A storage base whose state only derived contracts write, a library-style helper, an
    /// interface implemented by the deployed contract, and a router calling it through the
    /// interface with a thin wrapper.
    const SOURCES: [(&str, &str); 4] = [
        (
            "Storage.sol",
            "pragma solidity ^0.8.0;\ncontract Storage {\n    mapping(address => uint256) internal _balances;\n    uint256 internal total;\n}\n",
        ),
        (
            "Accounting.sol",
            "pragma solidity ^0.8.0;\nimport {Storage} from \"./Storage.sol\";\nabstract contract Accounting is Storage {\n    function _credit(address to, uint256 amount) internal {\n        _balances[to] += amount;\n        total += amount;\n    }\n}\n",
        ),
        (
            "Pool.sol",
            "pragma solidity ^0.8.0;\nimport {Accounting} from \"./Accounting.sol\";\ninterface IPool {\n    function deposit(address to, uint256 amount) external;\n}\ncontract Pool is Accounting, IPool {\n    function deposit(address to, uint256 amount) external {\n        _credit(to, amount);\n    }\n    function balanceOf(address a) external view returns (uint256) {\n        return _balances[a];\n    }\n}\n",
        ),
        (
            "Router.sol",
            "pragma solidity ^0.8.0;\nimport {IPool} from \"./Pool.sol\";\ncontract Router {\n    IPool public pool;\n    function route(address to, uint256 amount) public {\n        pool.deposit(to, amount);\n    }\n    function wrap(address to) external {\n        route(to, 1);\n    }\n    function other() external {\n        pool = IPool(address(0));\n    }\n}\n",
        ),
    ];

    async fn index(name: &str) -> (CodeIndex, PathBuf) {
        let root = std::env::temp_dir()
            .join("evolve-knowdit-index-tests")
            .join(format!("{name}-{}", std::process::id()));
        let src = root.join("src");
        tokio::fs::create_dir_all(&src).await.expect("fixture dir");
        for (file, text) in SOURCES {
            tokio::fs::write(src.join(file), text).await.expect("fixture file");
        }
        let layout = FoundryLayout {
            root: root.clone(),
            src_dir: "src".to_string(),
            test_dir: "test".to_string(),
            via_ir: false,
            solc: None,
            remappings: Vec::new(),
            lib_dirs: Vec::new(),
        };
        let index = CodeIndex::build(&layout, &ScopeContext::default(), None, &[])
            .await
            .expect("index");
        (index, root)
    }

    fn anchor(file: &str, contract: &str, function: Option<&str>, state: Option<&str>) -> SourceAnchor {
        SourceAnchor {
            file: format!("src/{file}"),
            contract: Some(contract.to_string()),
            function: function.map(String::from),
            state: state.map(String::from),
        }
    }

    fn relation_of<'a>(listed: &'a [ExtentFunction], contract: &str, function: &str) -> Option<&'a ExtentRelation> {
        listed
            .iter()
            .find(|f| f.contract == contract && f.function == function)
            .map(|f| &f.relation)
    }

    #[tokio::test]
    async fn function_spans_follow_inheritance_and_fall_back_to_the_name() {
        let (index, _root) = index("spans").await;
        let deposit = index.function_spans("Pool", "deposit");
        assert_eq!(deposit.len(), 1, "{deposit:?}");
        assert_eq!(deposit[0].0, "src/Pool.sol");
        assert!(deposit[0].1.start_line <= 8 && 8 <= deposit[0].1.end_line, "{deposit:?}");
        let credit = index.function_spans("Pool", "_credit");
        assert_eq!(credit.len(), 1, "{credit:?}");
        assert_eq!(credit[0].0, "src/Accounting.sol");
        assert_eq!(index.function_spans("Vault", "deposit").len(), 2);
        assert!(index.function_spans("Pool", "missing").is_empty());
    }

    #[tokio::test]
    async fn the_readable_graph_keeps_tests_and_mocks_but_not_audit_or_flattened_copies() {
        let root = std::env::temp_dir()
            .join("evolve-knowdit-index-tests")
            .join(format!("readable-{}", std::process::id()));
        let pool = SOURCES.iter().find(|(f, _)| *f == "Pool.sol").expect("Pool fixture").1;
        for (dir, file, text) in [
            ("src", "Pool.sol", pool),
            ("audits/snapshot", "Pool-flatten.sol", pool),
            ("src", "Accounting.sol", SOURCES[1].1),
            ("src", "Storage.sol", SOURCES[0].1),
            ("test/mocks", "MockPool.sol", "pragma solidity ^0.8.0;\ncontract MockPool {\n    function deposit(address, uint256) external {}\n}\n"),
        ] {
            tokio::fs::create_dir_all(root.join(dir)).await.expect("fixture dir");
            tokio::fs::write(root.join(dir).join(file), text).await.expect("fixture file");
        }
        let layout = FoundryLayout {
            root: root.clone(),
            src_dir: "src".to_string(),
            test_dir: "test".to_string(),
            via_ir: false,
            solc: None,
            remappings: Vec::new(),
            lib_dirs: Vec::new(),
        };
        let index = CodeIndex::build(&layout, &ScopeContext::default(), None, &[])
            .await
            .expect("index");
        let files_of = |name: &str| -> Vec<String> {
            index
                .readable
                .graph()
                .modules
                .values()
                .filter(|m| m.name == name)
                .map(|m| m.file.to_string_lossy().replace('\\', "/"))
                .collect()
        };
        assert_eq!(files_of("Pool"), vec!["src/Pool.sol".to_string()]);
        assert_eq!(files_of("MockPool"), vec!["test/mocks/MockPool.sol".to_string()]);
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn a_state_anchor_in_a_storage_base_lists_its_writers_and_the_entry_points_reaching_them() {
        let (index, root) = index("state").await;
        let listed = index.extent_functions(&[anchor("Storage.sol", "Storage", None, Some("_balances"))]);
        let shown: Vec<String> = listed.iter().map(|f| f.to_string()).collect();
        assert!(
            matches!(relation_of(&listed, "Accounting", "_credit"), Some(ExtentRelation::Writes { .. })),
            "{shown:#?}"
        );
        assert!(relation_of(&listed, "Pool", "deposit").is_some(), "{shown:#?}");
        assert!(
            matches!(relation_of(&listed, "Router", "route"), Some(ExtentRelation::Reaches { .. })),
            "{shown:#?}"
        );
        assert!(
            matches!(relation_of(&listed, "Router", "wrap"), Some(ExtentRelation::Reaches { .. })),
            "{shown:#?}"
        );
        assert!(relation_of(&listed, "Router", "other").is_none(), "{shown:#?}");
        // The anchor itself names the state's accessors, so a row may anchor them.
        let named = index.anchored_functions(&[anchor("Storage.sol", "Storage", None, Some("_balances"))]);
        assert!(named.iter().any(|f| f.function == "_credit"), "{named:#?}");
        assert!(named.iter().any(|f| f.function == "balanceOf"), "{named:#?}");
        tokio::fs::remove_dir_all(root).await.expect("cleanup");
    }

    #[tokio::test]
    async fn settlement_groups_join_thin_wrappers_and_split_unrelated_entry_points() {
        let (index, root) = index("groups").await;
        let wrapped = index.settlement_groups(&[
            anchor("Router.sol", "Router", Some("route"), None),
            anchor("Router.sol", "Router", Some("wrap"), None),
        ]);
        assert_eq!(wrapped.len(), 1, "{wrapped:?}");
        let apart = index.settlement_groups(&[
            anchor("Router.sol", "Router", Some("route"), None),
            anchor("Router.sol", "Router", Some("other"), None),
        ]);
        assert_eq!(apart.len(), 2, "{apart:?}");
        tokio::fs::remove_dir_all(root).await.expect("cleanup");
    }

    #[tokio::test]
    async fn exposed_functions_and_anchors_follow_inheritance() {
        let (index, root) = index("exposed").await;
        let exposed = index.exposed_functions();
        assert!(exposed.contains(&("Pool".to_string(), "_credit".to_string())));
        assert!(exposed.contains(&("Router".to_string(), "wrap".to_string())));
        let anchor = index.function_anchor("Pool", "_credit").expect("inherited function");
        assert_eq!(anchor.contract.as_deref(), Some("Accounting"));
        assert!(index.function_anchor("Pool", "nosuch").is_none());
        tokio::fs::remove_dir_all(root).await.expect("cleanup");
    }
}
