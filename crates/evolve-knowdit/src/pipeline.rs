use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use color_eyre::eyre::{Result, eyre};
use evolve_knowdit_agent::{RunBudget, Stage, StageLlms};
use evolve_knowdit_matrix::{MatrixOutput, MatrixStage};
use evolve_knowdit_project::{
    AstBuild, CodeIndex, FoundryRepair, FoundryToml, PathFence, ProjectConventions,
    ProjectResolver, ProjectTools, ScopeLoader, Workspace,
};
use evolve_knowdit_report::{
    CoverageBuilder, FindingCollector, FindingMerger, MergeArgs, ReportRenderer,
};
use evolve_knowdit_types::{
    CoverageMatrix, Dimension, FindingCluster, JsonStore, ProjectManifest, SourceAnchor, TaskSet,
    VerificationRecord, VerificationTask,
};
use evolve_knowdit_verify::{HarnessWorkspace, Verifier};
use knowdit_forge::{ForgeBackend, ForgeBackendKind};
use llmy::clap::{LLMYConfig, OpenAISetup};
use llmy::client::client::LLM;
use llmy::codegraph::SolcBuildInfo;

use crate::cli::{InspectArgs, PreflightArgs, ProjectArgs, ReportArgs, RunArgs, Until};

/// Resolves the project once per output directory and persists the manifest.
pub struct ProjectSetup {
    pub manifest: ProjectManifest,
    pub foundry_excerpt: String,
}

impl ProjectSetup {
    pub async fn prepare(args: &ProjectArgs, store: Option<&JsonStore>) -> Result<Self> {
        let forge_std_source = args
            .forge_std_source
            .as_ref()
            .map(std::path::absolute)
            .transpose()?;
        if let Some(store) = store
            && let Some(mut manifest) = store
                .read_optional::<ProjectManifest>(&store.layout.manifest_file)
                .await?
        {
            manifest.layout.root = manifest.layout.root.canonicalize()?;
            tracing::info!("reusing manifest for {}", manifest.layout.root.display());
            if let Some(path) = &args.threat_model_file {
                manifest.scope.threat_model = Some(std::fs::read_to_string(path)?);
                store.write(&store.layout.manifest_file, &manifest).await?;
            }
            let owned_copy = store
                .layout
                .repo_dir
                .canonicalize()
                .map(|repo| manifest.layout.root.starts_with(repo))
                .unwrap_or(false);
            if owned_copy {
                // The repairs are idempotent; a copy an earlier stage made may predate a repair
                // (a forge-std provisioned for the dynamic stage), so apply them again.
                let repair = FoundryRepair {
                    root: manifest.layout.root.clone(),
                    forge_std_source: forge_std_source.clone(),
                };
                let changes = repair.apply()?;
                for change in &changes {
                    tracing::info!("foundry repair: {change}");
                }
                if !changes.is_empty() {
                    let toml = FoundryToml::load(&manifest.layout.root)?;
                    manifest.layout = toml.to_layout(manifest.layout.root.clone());
                    store.write(&store.layout.manifest_file, &manifest).await?;
                }
            }
            let toml = FoundryToml::load(&manifest.layout.root)?;
            return Ok(Self {
                manifest,
                foundry_excerpt: toml.raw_excerpt,
            });
        }
        let (source_root, prefix) = ProjectResolver {
            dataset_root: args.project.clone(),
            foundry_root_override: args.foundry_root.clone(),
        }
        .resolve()?;
        let work_root = match (store, args.copy) {
            (Some(store), true) => {
                let root = Workspace {
                    source_root: source_root.clone(),
                    destination: store.layout.repo_dir.clone(),
                }
                .materialize()
                .await?;
                let repair = FoundryRepair {
                    root: root.clone(),
                    forge_std_source: forge_std_source.clone(),
                };
                for change in repair.apply()? {
                    tracing::info!("foundry repair: {change}");
                }
                root
            }
            _ => source_root.clone(),
        };
        let toml = FoundryToml::load(&work_root)?;
        let layout = toml.to_layout(work_root.clone());
        let dataset_root = args.project.canonicalize()?;
        let scope = ScopeLoader {
            dataset_root: dataset_root.clone(),
            foundry_root: work_root.clone(),
            relative_prefix: prefix,
            scope_file: args.scope_file.clone(),
            out_of_scope_file: args.out_of_scope_file.clone(),
            threat_model_file: args.threat_model_file.clone(),
            readme_chars: args.readme_chars,
        }
        .load()?;
        tracing::info!(
            "project {} (src `{}`, test `{}`, via_ir {}): {} in-scope files, {} out-of-scope files",
            work_root.display(),
            layout.src_dir,
            layout.test_dir,
            layout.via_ir,
            scope.in_scope.len(),
            scope.out_of_scope.len()
        );
        let manifest = ProjectManifest {
            dataset_root,
            source_root,
            layout,
            scope,
            created_at: chrono::Utc::now(),
        };
        if let Some(store) = store {
            store.write(&store.layout.manifest_file, &manifest).await?;
        }
        Ok(Self {
            manifest,
            foundry_excerpt: toml.raw_excerpt,
        })
    }
}

/// The compiler ASTs for a copy, when a local forge binary exists to produce them: the backend
/// resolution of `knowdit-forge` decides which binary (an explicit `--forge-bin`, else `forge`
/// on PATH); the docker backend has no host binary, so the index falls back to tree-sitter.
pub struct AstSetup {
    pub forge_bin: Option<PathBuf>,
    pub root: PathBuf,
    pub dir: PathBuf,
}

impl AstSetup {
    pub async fn load(&self) -> Result<Option<SolcBuildInfo>> {
        let backend = ForgeBackend::new(
            self.forge_bin.clone(),
            "lazymio/knowdit:foundry".to_string(),
            None,
        );
        let binary = match backend.map(|b| b.kind) {
            Ok(ForgeBackendKind::LocalDirect { binary })
            | Ok(ForgeBackendKind::LocalCgroup { binary, .. }) => binary,
            Ok(ForgeBackendKind::Docker { .. }) => {
                tracing::warn!(
                    "no local forge binary: the code index falls back to tree-sitter (pass --forge-bin for compiler ASTs)"
                );
                return Ok(None);
            }
            Err(e) => {
                tracing::warn!("no usable forge ({e}); the code index falls back to tree-sitter");
                return Ok(None);
            }
        };
        let build = AstBuild {
            root: self.root.clone(),
            dir: self.dir.clone(),
            forge_bin: binary,
        };
        Ok(Some(build.load_or_compile().await?))
    }
}

/// Builds the LLMs from the CLI flags and the optional llmy config (`--llmy-config` /
/// `LLMY_CONFIG`), defaulting the conversation dump into the output. Without a config every
/// stage runs on the env/flag endpoint. With one, a profile named after stages (`draft`,
/// `properties`, `row`, `plan`, `task`, `poc`, `verdict`, `merge`, or several joined by commas
/// such as `task,poc`) serves those stages as one client, so they share its spend cap and
/// concurrency; every other stage runs on the default chain: `--llm-profiles`, else the
/// config's `fallback`, else its one profile not named after a stage, else the env/flag
/// endpoint. Each of these LLMs is capped by the config's `billing-cap` on its own and by its
/// profile's `biling-cap`.
pub struct LlmSetup;

impl LlmSetup {
    pub async fn build(mut setup: OpenAISetup, store: &JsonStore) -> Result<StageLlms> {
        let debug = store.layout.llm_debug_file.display().to_string();
        if setup.llm_debug.is_none() {
            setup.llm_debug = Some(debug.clone());
        }
        let Some(path) = setup.llmy_config.clone() else {
            let default = setup
                .may_llm()
                .await
                .map_err(|e| eyre!("LLM setup failed: {e}"))?
                .ok_or_else(|| {
                    eyre!("no model configured: set OPENAI_API_MODEL / LLM_MODEL or --model")
                })?;
            return Ok(StageLlms {
                default,
                profiles: Vec::new(),
                stages: BTreeMap::new(),
            });
        };
        let mut config = LLMYConfig::load(&path).map_err(|e| eyre!("LLM setup failed: {e}"))?;
        if config.llm_debug.is_none() {
            config.llm_debug = Some(debug);
        }
        let injected = setup
            .default_profile()
            .map_err(|e| eyre!("LLM setup failed: {e}"))?;
        let mut staged: Vec<(String, Vec<Stage>)> = Vec::new();
        let mut unstaged: Vec<String> = Vec::new();
        for name in config.profiles.keys() {
            let parts: Vec<Option<Stage>> =
                name.split(',').map(|p| Stage::from_name(p.trim())).collect();
            if parts.iter().all(Option::is_some) {
                staged.push((name.clone(), parts.into_iter().flatten().collect()));
            } else if parts.iter().any(Option::is_some) {
                return Err(eyre!(
                    "{}: profile `{name}` mixes stage names with other names",
                    path.display()
                ));
            } else {
                unstaged.push(name.clone());
            }
        }
        let default_chain: Vec<String> = match (&setup.llm_profiles, &config.fallback) {
            (Some(names), _) if !names.is_empty() => names.clone(),
            (_, Some(order)) if !order.is_empty() => order.clone(),
            _ => {
                match (unstaged.len(), &injected) {
                    (1, _) => unstaged,
                    (0, Some(profile)) => vec![profile.name.clone()],
                    _ => {
                        return Err(eyre!(
                            "{}: the default LLM is undecided ({} profiles not named after a stage: [{}], and {}); order them with `fallback = [..]` or --llm-profiles",
                            path.display(),
                            unstaged.len(),
                            unstaged.join(", "),
                            if injected.is_some() {
                                "an env/flag endpoint"
                            } else {
                                "no env/flag endpoint"
                            }
                        ));
                    }
                }
            }
        };
        let default = config
            .to_llm(Some(&default_chain), injected)
            .await
            .map_err(|e| eyre!("LLM setup failed: {e}"))?;
        tracing::info!("default LLM: profiles [{}]", default_chain.join(", "));
        let mut profiles = Vec::new();
        let mut stages = BTreeMap::new();
        for (name, served) in staged {
            let llm = config
                .to_llm(Some(std::slice::from_ref(&name)), None)
                .await
                .map_err(|e| eyre!("LLM setup failed for profile `{name}`: {e}"))?;
            for stage in &served {
                if stages.insert(*stage, profiles.len()).is_some() {
                    return Err(eyre!(
                        "{}: stage `{}` is named by two profiles",
                        path.display(),
                        stage.name()
                    ));
                }
            }
            tracing::info!(
                "profile `{name}` serves stage(s) {}",
                served.iter().map(|s| s.name()).collect::<Vec<_>>().join(", ")
            );
            profiles.push(llm);
        }
        Ok(StageLlms {
            default,
            profiles,
            stages,
        })
    }
}

pub struct RunPipeline {
    pub args: RunArgs,
}

impl RunPipeline {
    fn dimensions(&self) -> Result<Vec<Dimension>> {
        let mut out = Vec::new();
        for raw in &self.args.dimensions {
            let d = Dimension::parse_slug(raw.trim()).ok_or_else(|| {
                eyre!("unknown dimension `{raw}`; use module, operation_flow, asset_lifecycle")
            })?;
            if !out.contains(&d) {
                out.push(d);
            }
        }
        Ok(out)
    }

    pub async fn run(self) -> Result<()> {
        let store = JsonStore::new(std::path::absolute(&self.args.out)?);
        tokio::fs::create_dir_all(&store.root).await?;
        let dimensions = self.dimensions()?;
        let setup = ProjectSetup::prepare(&self.args.project, Some(&store)).await?;
        let llms = LlmSetup::build(self.args.llm.clone(), &store).await?;
        let solc = AstSetup {
            forge_bin: self.args.forge.forge_bin.clone(),
            root: setup.manifest.layout.root.clone(),
            dir: store.layout.build_info_dir.clone(),
        }
        .load()
        .await?;
        let harnesses = HarnessWorkspace::new(&setup.manifest.layout).root;
        let index = Arc::new(
            CodeIndex::build(
                &setup.manifest.layout,
                &setup.manifest.scope,
                solc,
                std::slice::from_ref(&harnesses),
            )
            .await?,
        );
        // Every agent's file tools keep out of the reproduction harnesses; a PoC agent gets its
        // own attempt directory back.
        let tools = ProjectTools {
            index,
            fence: PathFence {
                hidden: vec![harnesses],
                own: None,
            },
            module_source: self.args.module_source,
            read_file: !self.args.no_read_file,
        };
        if self.args.until != Until::Matrix {
            Verifier::preflight(&setup.manifest.layout, &self.args.forge).await?;
        }

        let matrix_llms = llms.scope("matrix");
        let matrix = Arc::new(MatrixStage {
            store: store.clone(),
            tools: tools.clone(),
            scope: setup.manifest.scope.clone(),
            config: self.args.matrix.to_config(),
            llms: matrix_llms.clone(),
            settings: None,
        });
        let output = matrix.run(dimensions).await?;
        let units = output.tasks.units(self.args.matrix.task_mode, &output.scenarios);
        store.write(&store.layout.units_file, &units).await?;
        tracing::info!(
            "matrix stage done: {}; {}; {} unit(s); usage {}",
            output.matrix.describe(),
            output.tasks.describe(),
            units.len(),
            matrix_llms.usage()
        );
        if self.args.until == Until::Matrix {
            ReportStep::render_matrix_only(&store, &output, &units).await?;
            println!(
                "matrix: {}; {}; {} unit(s) → {}",
                output.matrix.describe(),
                output.tasks.describe(),
                units.len(),
                store.layout.report_dir.join("coverage.md").display()
            );
            Self::print_cost(&llms);
            return Ok(());
        }

        let verify_llms = llms.scope("verify");
        let conventions =
            ProjectConventions::load(&setup.manifest.layout, setup.foundry_excerpt.clone()).await;
        let verifier = Arc::new(
            Verifier::new(
                store.clone(),
                tools.clone(),
                &setup.manifest.layout,
                setup.manifest.scope.clone(),
                conventions,
                &self.args.forge,
                self.args.verify.to_config(),
                &verify_llms,
                None,
            )
            .await?,
        );
        let records = verifier.clone().run_all(&output.matrix, &units).await?;
        tracing::info!(
            "verify stage done: {} records; usage {}",
            records.len(),
            verify_llms.usage()
        );
        if self.args.until == Until::Verify {
            Self::print_cost(&llms);
            return Ok(());
        }
        let all_records = Verifier::load(&store).await?;
        ReportStep {
            store: store.clone(),
            merger: ReportStep::merger(
                tools,
                setup.manifest.scope.clone(),
                &self.args.merge,
                llms.of(Stage::Merge).scope(Some("report".to_string()), None),
            ),
            no_merge: self.args.no_merge,
            llm_cost_usd: llms.cost().round_dp(4).to_string(),
        }
        .render(&output.matrix, &output.tasks, &units, &all_records)
        .await?;
        Self::print_cost(&llms);
        Ok(())
    }

    fn print_cost(llms: &StageLlms) {
        println!(
            "LLM usage: {} — cost {} USD",
            llms.usage(),
            llms.cost().round_dp(4)
        );
    }
}

/// Renders every report artifact from persisted data.
pub struct ReportStep {
    pub store: JsonStore,
    pub merger: FindingMerger,
    pub no_merge: bool,
    pub llm_cost_usd: String,
}

impl ReportStep {
    pub fn merger(
        tools: ProjectTools,
        scope: evolve_knowdit_types::ScopeContext,
        args: &MergeArgs,
        llm: LLM,
    ) -> FindingMerger {
        FindingMerger {
            tools,
            scope,
            budget: RunBudget {
                max_steps: args.merge_max_steps,
                max_nudges: args.merge_max_nudges,
            },
            retries: args.merge_agent_retries,
            debug_prefix: args.merge_debug_prefix.clone(),
            llm,
            settings: None,
        }
    }

    pub async fn render_matrix_only(
        store: &JsonStore,
        output: &MatrixOutput,
        units: &[VerificationTask],
    ) -> Result<()> {
        let coverage = CoverageBuilder::build(&output.matrix, &output.tasks, units, &[], &[]);
        store
            .write(&store.layout.report_dir.join("coverage.json"), &coverage)
            .await?;
        store
            .write_text(
                &store.layout.report_dir.join("coverage.md"),
                &ReportRenderer::coverage_markdown(
                    &output.matrix,
                    &output.tasks,
                    units,
                    &coverage,
                    &[],
                ),
            )
            .await?;
        Ok(())
    }

    pub async fn render(
        &self,
        matrix: &CoverageMatrix,
        tasks: &TaskSet,
        units: &[VerificationTask],
        records: &[VerificationRecord],
    ) -> Result<()> {
        let findings = FindingCollector::collect(records);
        let clusters = if self.no_merge {
            findings
                .iter()
                .map(|f| FindingCluster {
                    representative: f.id.clone(),
                    members: vec![f.id.clone()],
                    root_cause: f.draft.root_cause.clone(),
                })
                .collect()
        } else {
            self.merger.merge(&findings).await?
        };
        let coverage = CoverageBuilder::build(matrix, tasks, units, records, &findings);
        let summary = ReportRenderer::summary(
            matrix,
            tasks,
            units,
            records,
            &findings,
            &clusters,
            self.llm_cost_usd.clone(),
        );
        let report = &self.store.layout.report_dir;
        self.store
            .write(&report.join("findings.json"), &findings)
            .await?;
        self.store
            .write(&report.join("clusters.json"), &clusters)
            .await?;
        self.store
            .write(&report.join("coverage.json"), &coverage)
            .await?;
        self.store
            .write(&report.join("summary.json"), &summary)
            .await?;
        self.store
            .write_text(
                &report.join("coverage.md"),
                &ReportRenderer::coverage_markdown(matrix, tasks, units, &coverage, records),
            )
            .await?;
        self.store
            .write_text(
                &report.join("findings.md"),
                &ReportRenderer::findings_markdown(&findings, &clusters, records),
            )
            .await?;
        println!(
            "report: {} subjects, {} properties, {} cells ({} excluded), {} tasks, {} units, {} demonstrated, {} valid findings in {} clusters → {}",
            summary.subjects,
            summary.properties,
            summary.cells,
            summary.excluded_cells,
            summary.tasks,
            summary.units,
            summary.demonstrated,
            summary.valid_findings,
            summary.clusters,
            report.display()
        );
        Ok(())
    }
}

pub struct InspectCommand;

impl InspectCommand {
    pub async fn run(args: InspectArgs) -> Result<()> {
        let mut project = args.project.clone();
        project.copy = false;
        let setup = ProjectSetup::prepare(&project, None).await?;
        let dir = match &args.build_info_dir {
            Some(dir) => dir.clone(),
            None => {
                let name: String = setup
                    .manifest
                    .layout
                    .root
                    .to_string_lossy()
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                    .collect();
                std::env::temp_dir().join("evolve-knowdit-build-info").join(name)
            }
        };
        let solc = AstSetup {
            forge_bin: args.forge_bin.clone(),
            root: setup.manifest.layout.root.clone(),
            dir,
        }
        .load()
        .await?;
        let index = CodeIndex::build(
            &setup.manifest.layout,
            &setup.manifest.scope,
            solc,
            &[HarnessWorkspace::new(&setup.manifest.layout).root],
        )
        .await?;
        println!("{}", index.render_overview());
        if args.inventory {
            for dimension in Dimension::all() {
                println!(
                    "\n== inventory: {} ==\n{}",
                    dimension.title(),
                    index.render_inventory(dimension)
                );
            }
        }
        if let Some(target) = &args.calls_of {
            let (contract, function) = target
                .split_once('.')
                .ok_or_else(|| eyre!("--calls-of takes `Contract.function`"))?;
            println!("\n== calls of {contract}.{function} ==");
            for line in index.calls_of(contract, function) {
                println!("{line}");
            }
        }
        if let Some(target) = &args.functions_of {
            let (file, name) = match target.split_once(':') {
                Some((f, rest)) => (Some(f), rest),
                None => (None, target.as_str()),
            };
            let (contract, member) = match name.split_once('.') {
                Some((c, m)) => (c, Some(m.to_string())),
                None => (name, None),
            };
            let graph = index.context.graph();
            let module = graph
                .modules
                .values()
                .find(|m| {
                    m.name == contract
                        && file.is_none_or(|f| m.file.to_string_lossy() == f.trim_start_matches("./"))
                })
                .ok_or_else(|| eyre!("no contract named {contract} in the index"))?;
            let is_state = member.as_ref().is_some_and(|name| {
                graph
                    .states_of_module(module.id)
                    .iter()
                    .any(|s| &s.name == name)
            });
            let anchor = SourceAnchor {
                file: module.file.to_string_lossy().to_string(),
                contract: Some(contract.to_string()),
                function: if is_state { None } else { member.clone() },
                state: if is_state { member } else { None },
            };
            println!("\n== extent functions of {target} ==");
            for f in index.extent_functions(&[anchor]) {
                println!("{}{}", if f.read_only { "[view] " } else { "" }, f);
            }
        }
        Ok(())
    }
}

pub struct ReportCommand;

impl ReportCommand {
    pub async fn run(args: ReportArgs) -> Result<()> {
        let store = JsonStore::new(std::path::absolute(&args.out)?);
        let output = MatrixStage::load(&store).await?.ok_or_else(|| {
            eyre!(
                "no matrix under {}; run `run --until matrix` first",
                store.root.display()
            )
        })?;
        let records = Verifier::load(&store).await?;
        let units: Vec<VerificationTask> = store
            .read_optional(&store.layout.units_file)
            .await?
            .unwrap_or_default();
        let manifest: ProjectManifest = store
            .read_optional(&store.layout.manifest_file)
            .await?
            .ok_or_else(|| eyre!("no manifest under {}", store.root.display()))?;
        let llms = LlmSetup::build(args.llm.clone(), &store).await?;
        let build_info = store.layout.build_info_dir.join("build-info");
        let solc = if build_info.is_dir() {
            Some(
                SolcBuildInfo::load(manifest.layout.root.clone(), &build_info)
                    .await
                    .map_err(|e| eyre!("cannot load ASTs under {}: {e}", build_info.display()))?,
            )
        } else {
            None
        };
        let harnesses = HarnessWorkspace::new(&manifest.layout).root;
        let index = Arc::new(
            CodeIndex::build(
                &manifest.layout,
                &manifest.scope,
                solc,
                std::slice::from_ref(&harnesses),
            )
            .await?,
        );
        ReportStep {
            store: store.clone(),
            merger: ReportStep::merger(
                ProjectTools {
                    index,
                    fence: PathFence {
                        hidden: vec![harnesses],
                        own: None,
                    },
                    module_source: false,
                    read_file: true,
                },
                manifest.scope.clone(),
                &args.merge,
                llms.of(Stage::Merge).scope(Some("report".to_string()), None),
            ),
            no_merge: args.no_merge,
            llm_cost_usd: llms.cost().round_dp(4).to_string(),
        }
        .render(&output.matrix, &output.tasks, &units, &records)
        .await
    }
}

pub struct PreflightCommand;

impl PreflightCommand {
    pub async fn run(args: PreflightArgs) -> Result<()> {
        let store = JsonStore::new(std::path::absolute(&args.out)?);
        tokio::fs::create_dir_all(&store.root).await?;
        let setup = ProjectSetup::prepare(&args.project, Some(&store)).await?;
        Verifier::preflight(&setup.manifest.layout, &args.forge).await?;
        println!("preflight ok: {}", setup.manifest.layout.root.display());
        Ok(())
    }
}
