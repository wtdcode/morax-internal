use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use clap::Args;
use color_eyre::eyre::{Result, WrapErr};
use evolve_knowdit_agent::{RunBudget, Stage, StageLlms};
use evolve_knowdit_project::{ProjectConventions, ProjectTools};
use evolve_knowdit_types::{
    CellRef, CoverageMatrix, FoundryLayout, JsonStore, ScopeContext, TaskOutcome,
    VerificationRecord, VerificationTask,
};
use llmy::client::settings::LLMSettings;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;

use std::time::Duration;

use knowdit_forge::{ForgeBackend, ForgeRunner, ForgeTimeouts};

use crate::forge::ForgeArgs;
use crate::harness::HarnessWorkspace;
use crate::poc::{PocOutcome, PocRunner};
use crate::prompts::TaskContext;
use crate::verdict::VerdictRunner;

/// Dynamic-stage knobs, flattened into the CLI.
#[derive(Args, Clone, Debug)]
pub struct VerifyArgs {
    /// Units verified concurrently (each holds a forge process while compiling); the PoC and
    /// verdict pools default to this when unset.
    #[arg(long = "verify-concurrency", default_value_t = 3)]
    pub concurrency: usize,
    /// PoC agents running at once (0 = `--verify-concurrency`).
    #[arg(long = "verify-poc-concurrency", default_value_t = 0)]
    pub poc_concurrency: usize,
    /// Verdict agents running at once (0 = `--verify-concurrency`).
    #[arg(long = "verify-verdict-concurrency", default_value_t = 0)]
    pub verdict_concurrency: usize,
    /// Agent step budget per PoC attempt.
    #[arg(long = "verify-poc-max-steps", default_value_t = 60)]
    pub poc_max_steps: usize,
    /// Fresh PoC agent attempts per task.
    #[arg(long = "verify-poc-attempts", default_value_t = 2)]
    pub poc_attempts: usize,
    /// Step by which a PoC attempt must have run forge once, else it is told to stop reading
    /// and compile a first harness.
    #[arg(long = "verify-poc-first-run-step", default_value_t = 20)]
    pub poc_first_run_step: usize,
    /// Agent step budget per verdict.
    #[arg(long = "verify-verdict-max-steps", default_value_t = 40)]
    pub verdict_max_steps: usize,
    #[arg(long = "verify-verdict-retries", default_value_t = 1)]
    pub verdict_retries: usize,
    #[arg(long = "verify-max-nudges", default_value_t = 3)]
    pub verify_max_nudges: usize,
    /// Bytes of forge output returned to the agent per run.
    #[arg(long = "verify-output-tail-bytes", default_value_t = 6000)]
    pub output_tail_bytes: usize,
    #[arg(long = "verify-list-test-files-max-depth", default_value_t = 5)]
    pub list_test_files_max_depth: usize,
    /// Only verify tasks whose key contains this substring.
    #[arg(long = "verify-task-filter")]
    pub task_filter: Option<String>,
    /// Record demonstrated violations without running the verdict agent.
    #[arg(long = "verify-skip-verdict", default_value_t = false)]
    pub skip_verdict: bool,
    /// Refuse a PoC agent's `conclude` until it has run forge once in that attempt.
    #[arg(long = "force-poc", default_value_t = false)]
    pub force_poc: bool,
    #[arg(long = "verify-debug-prefix", default_value = "verify")]
    pub verify_debug_prefix: String,
}

#[derive(Debug, Clone)]
pub struct VerifyConfig {
    pub concurrency: usize,
    pub poc_concurrency: usize,
    pub verdict_concurrency: usize,
    pub poc_budget: RunBudget,
    pub poc_attempts: usize,
    pub poc_first_run_step: usize,
    pub verdict_budget: RunBudget,
    pub verdict_retries: usize,
    pub output_tail_bytes: usize,
    pub list_test_files_max_depth: usize,
    pub task_filter: Option<String>,
    pub skip_verdict: bool,
    pub force_poc: bool,
    pub debug_prefix: String,
}

impl VerifyArgs {
    /// The shared nudge limit with one agent's step budget.
    fn budget(&self, max_steps: usize) -> RunBudget {
        RunBudget {
            max_steps,
            max_nudges: self.verify_max_nudges,
        }
    }

    pub fn to_config(&self) -> VerifyConfig {
        VerifyConfig {
            concurrency: self.concurrency.max(1),
            poc_concurrency: if self.poc_concurrency == 0 { self.concurrency.max(1) } else { self.poc_concurrency },
            verdict_concurrency: if self.verdict_concurrency == 0 { self.concurrency.max(1) } else { self.verdict_concurrency },
            poc_budget: self.budget(self.poc_max_steps),
            poc_attempts: self.poc_attempts,
            poc_first_run_step: self.poc_first_run_step,
            verdict_budget: self.budget(self.verdict_max_steps),
            verdict_retries: self.verdict_retries,
            output_tail_bytes: self.output_tail_bytes,
            list_test_files_max_depth: self.list_test_files_max_depth,
            task_filter: self.task_filter.clone(),
            skip_verdict: self.skip_verdict,
            force_poc: self.force_poc,
            debug_prefix: self.verify_debug_prefix.clone(),
        }
    }
}

/// Runs the dynamic stage over every task and persists one record per task.
pub struct Verifier {
    store: JsonStore,
    poc: PocRunner,
    verdict: VerdictRunner,
    config: VerifyConfig,
    /// Separate pools: a slow verdict never holds a PoC slot and the reverse.
    poc_gate: Arc<Semaphore>,
    verdict_gate: Arc<Semaphore>,
}

impl Verifier {
    /// Errors that describe the provider or the network rather than the task: connection
    /// failures, 5xx answers, rate limits and timeouts. Anything else is the task's own failure.
    fn is_transient(text: &str) -> bool {
        RunBudget::is_transient(text)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        store: JsonStore,
        tools: ProjectTools,
        layout: &FoundryLayout,
        scope: ScopeContext,
        conventions: ProjectConventions,
        forge: &ForgeArgs,
        config: VerifyConfig,
        llms: &StageLlms,
        settings: Option<LLMSettings>,
    ) -> Result<Self> {
        let (workspace, runner) = Self::preflight(layout, forge).await?;
        let poc = PocRunner {
            tools: tools.clone(),
            conventions: Arc::new(conventions),
            workspace: workspace.clone(),
            runner: runner.clone(),
            verbosity_flag: format!("-{}", "v".repeat(forge.verbosity.clamp(1, 5))),
            violation_marker: forge.violation_marker.clone(),
            budget: config.poc_budget.clone(),
            attempts: config.poc_attempts,
            first_run_step: config.poc_first_run_step,
            force_poc: config.force_poc,
            output_tail_bytes: config.output_tail_bytes,
            list_test_files_max_depth: config.list_test_files_max_depth,
            debug_prefix: config.debug_prefix.clone(),
            llm: llms.of(Stage::Poc).clone(),
            settings: settings.clone(),
        };
        let verdict = VerdictRunner {
            tools,
            scope,
            workspace,
            runner,
            via_ir: layout.via_ir,
            budget: config.verdict_budget.clone(),
            retries: config.verdict_retries,
            debug_prefix: config.debug_prefix.clone(),
            llm: llms.of(Stage::Verdict).clone(),
            settings,
        };
        Ok(Self {
            poc_gate: Arc::new(Semaphore::new(config.poc_concurrency)),
            verdict_gate: Arc::new(Semaphore::new(config.verdict_concurrency)),
            store,
            poc,
            verdict,
            config,
        })
    }

    /// Creates the harness workspace and smoke-tests the forge environment. Called at the start
    /// of a run so a project that cannot compile fails before any LLM spend.
    pub async fn preflight(
        layout: &FoundryLayout,
        forge: &ForgeArgs,
    ) -> Result<(HarnessWorkspace, ForgeRunner)> {
        let mut workspace = HarnessWorkspace::new(layout);
        workspace.ensure().await?;
        let mem_cap_bytes = (forge.mem_cap_gb > 0).then(|| forge.mem_cap_gb << 30);
        let backend = ForgeBackend::new(
            forge.forge_bin.clone(),
            forge.docker_image.clone(),
            mem_cap_bytes,
        )?;
        let timeouts = ForgeTimeouts {
            test: Duration::from_secs(forge.timeout_secs),
            coverage: Duration::from_secs(forge.timeout_secs),
        };
        let runner = ForgeRunner::new(backend, layout.root.clone(), timeouts);
        runner.preflight(&workspace.root).await?;
        Ok((workspace, runner))
    }

    /// One task end to end; an existing record is reused. `poc_slot` is the PoC slot the
    /// dispatcher gave this unit; without one the unit waits for a slot of its own.
    pub async fn verify_task(
        &self,
        ctx: TaskContext,
        poc_slot: Option<OwnedSemaphorePermit>,
    ) -> Result<VerificationRecord> {
        let path = self
            .store
            .layout
            .verify_dir
            .join(&ctx.task_key)
            .with_extension("json");
        if let Some(existing) = self
            .store
            .read_optional::<VerificationRecord>(&path)
            .await?
        {
            // A demonstration whose verdict failed (a provider error, not a judgement) is
            // re-judged from the saved PoC outcome instead of being carried without a verdict.
            if existing.outcome == TaskOutcome::Demonstrated && existing.verdict.is_none() {
                tracing::info!(
                    "{}: record exists without a verdict, re-running the verdict",
                    ctx.task_key
                );
            } else if existing.outcome == TaskOutcome::Aborted {
                // An aborted stage (a provider refusal, a crash) is not a result: the record is
                // archived and the unit runs again.
                let archived = self.store.layout.aborted_dir.join(format!(
                    "{}-{}.json",
                    ctx.task_key,
                    existing.finished_at.format("%Y%m%dT%H%M%SZ")
                ));
                self.store.write(&archived, &existing).await?;
                tracing::info!(
                    "{}: record aborted ({}), archived to {}, running the unit again",
                    ctx.task_key,
                    existing.outcome_reason.chars().take(120).collect::<String>(),
                    archived.display()
                );
            } else {
                tracing::info!(
                    "{}: record exists ({:?}), reusing",
                    ctx.task_key,
                    existing.outcome
                );
                return Ok(existing);
            }
        }
        let started_at = chrono::Utc::now();
        let poc_path = self
            .store
            .layout
            .poc_dir
            .join(&ctx.task_key)
            .with_extension("json");
        let outcome = match self.store.read_optional::<PocOutcome>(&poc_path).await? {
            Some(saved) => {
                tracing::info!(
                    "{}: PoC outcome exists, running the verdict only",
                    ctx.task_key
                );
                Ok(saved)
            }
            None => {
                let _permit = match poc_slot {
                    Some(slot) => slot,
                    None => self.poc_gate.clone().acquire_owned().await?,
                };
                // A transport or provider error is not a verdict on the task: wait and retry
                // before recording anything, so a short outage costs minutes, not tasks.
                let mut result = self.poc.run(&ctx).await;
                let mut waits = 0u32;
                while let Err(e) = &result
                    && Self::is_transient(&format!("{e:#}"))
                    && waits < 6
                {
                    waits += 1;
                    let pause = Duration::from_secs(30 * u64::from(waits));
                    tracing::warn!(
                        "{}: transient error from the LLM provider, retrying in {}s (attempt {waits}/6): {e:#}",
                        ctx.task_key,
                        pause.as_secs()
                    );
                    tokio::time::sleep(pause).await;
                    result = self.poc.run(&ctx).await;
                }
                if let Ok(poc) = &result {
                    self.store.write(&poc_path, poc).await?;
                }
                result
            }
        };
        let (outcome_kind, reason, attempt, steps, restarts, harness_dir) = match outcome {
            Ok(poc) => {
                let steps: usize = poc.reports.iter().map(|r| r.steps).sum();
                let restarts = poc.reports.len().saturating_sub(1);
                let attempt = poc.attempt.clone();
                let kind = if attempt.violation_observed {
                    TaskOutcome::Demonstrated
                } else {
                    TaskOutcome::NotDemonstrated
                };
                let mut reason = if attempt.violation_observed {
                    "the test reverted with the violation marker".to_string()
                } else if let Some(concluded) = &attempt.concluded {
                    format!("agent concluded: {concluded}")
                } else {
                    format!(
                        "budget exhausted after {} steps and {} forge runs",
                        steps,
                        attempt.runs.len()
                    )
                };
                if attempt.updated_claim.is_some() {
                    reason.push_str(" (on the claim the reproduction updated)");
                }
                (kind, reason, attempt, steps, restarts, poc.harness_dir)
            }
            Err(e) => {
                tracing::error!("{}: poc stage failed: {e:#}", ctx.task_key);
                (
                    TaskOutcome::Aborted,
                    format!("poc stage failed: {e:#}"),
                    Default::default(),
                    0,
                    0,
                    String::new(),
                )
            }
        };
        let verdict = if outcome_kind == TaskOutcome::Demonstrated && !self.config.skip_verdict {
            let _permit = self.verdict_gate.clone().acquire_owned().await;
            match self.verdict.judge(&ctx, &attempt).await {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::error!("{}: verdict failed: {e:#}", ctx.task_key);
                    None
                }
            }
        } else {
            None
        };
        let harness_path = attempt.harness_filename.as_ref().map(|f| {
            let full = std::path::Path::new(&harness_dir).join(f);
            full.strip_prefix(&self.poc.tools.index.root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| full.to_string_lossy().to_string())
        });
        let record = VerificationRecord {
            task_key: ctx.task_key.clone(),
            task: ctx.task.clone(),
            outcome: outcome_kind,
            outcome_reason: reason,
            agent_steps: steps,
            restarts,
            harness_path,
            harness_source: (!attempt.harness_source.is_empty())
                .then(|| attempt.harness_source.clone()),
            runs: attempt.runs.clone(),
            verdict,
            updated_claim: attempt.updated_claim.clone(),
            started_at,
            finished_at: chrono::Utc::now(),
        };
        self.store.write(&path, &record).await?;
        tracing::info!(
            "{}: {:?} ({}); verdict {:?}",
            ctx.task_key,
            record.outcome,
            record.outcome_reason.chars().take(120).collect::<String>(),
            record.verdict.as_ref().map(|v| v.class)
        );
        Ok(record)
    }

    /// Verifies every unit with bounded concurrency (the PoC and verdict pools). A unit with
    /// a record or a saved PoC outcome starts at once. Every free PoC slot goes to the
    /// (cell, task) with the fewest units started so far, earlier runs' records included,
    /// which starts its next unit; ties go to the unit earlier in the order given.
    pub async fn run_all(
        self: Arc<Self>,
        matrix: &CoverageMatrix,
        units: &[VerificationTask],
    ) -> Result<Vec<VerificationRecord>> {
        let contexts: Vec<TaskContext> = units
            .iter()
            .map(|unit| TaskContext::new(matrix, unit))
            .filter(|ctx| {
                self.config
                    .task_filter
                    .as_ref()
                    .is_none_or(|filter| ctx.task_key.contains(filter.as_str()))
            })
            .collect();
        let total = contexts.len();
        let mut set = JoinSet::new();
        // Per (cell, task): units started so far and the units still waiting, in order.
        let mut started: BTreeMap<Vec<CellRef>, usize> = BTreeMap::new();
        let mut waiting: BTreeMap<Vec<CellRef>, VecDeque<(usize, TaskContext)>> = BTreeMap::new();
        for (order, ctx) in contexts.into_iter().enumerate() {
            let key: Vec<CellRef> = ctx.task.cells.iter().map(|c| c.cell.clone()).collect();
            let layout = &self.store.layout;
            let has_record =
                tokio::fs::try_exists(layout.verify_dir.join(&ctx.task_key).with_extension("json"))
                    .await?;
            let has_poc =
                tokio::fs::try_exists(layout.poc_dir.join(&ctx.task_key).with_extension("json"))
                    .await?;
            let count = started.entry(key.clone()).or_default();
            if has_record || has_poc {
                *count += 1;
                let verifier = self.clone();
                set.spawn(async move { verifier.verify_task(ctx, None).await });
            } else {
                waiting.entry(key).or_default().push_back((order, ctx));
            }
        }
        tracing::info!(
            "verifying {total} unit(s), {} reusing a record or PoC outcome, the rest by (cell, task) turn over {} (cell, task) pair(s): {} PoC and {} verdict agents at once",
            set.len(),
            started.len(),
            self.config.poc_concurrency,
            self.config.verdict_concurrency
        );
        let mut records = Vec::new();
        loop {
            let pending = waiting.values().any(|q| !q.is_empty());
            tokio::select! {
                slot = self.poc_gate.clone().acquire_owned(), if pending => {
                    let slot = slot?;
                    let key = waiting
                        .iter()
                        .filter_map(|(key, q)| q.front().map(|(order, _)| (started[key], *order, key)))
                        .min()
                        .map(|(_, _, key)| key.clone())
                        .expect("a waiting unit exists");
                    let (_, ctx) = waiting
                        .get_mut(&key)
                        .and_then(|q| q.pop_front())
                        .expect("the chosen pair has a waiting unit");
                    *started.get_mut(&key).expect("every pair is counted") += 1;
                    let verifier = self.clone();
                    set.spawn(async move { verifier.verify_task(ctx, Some(slot)).await });
                }
                Some(joined) = set.join_next() => {
                    match joined? {
                        Ok(record) => {
                            records.push(record);
                            if records.len() % 25 == 0 {
                                tracing::info!("{} of {total} unit(s) verified", records.len());
                            }
                        }
                        Err(e) => tracing::error!("verification task failed: {e:#}"),
                    }
                }
                else => break,
            }
        }
        records.sort_by(|a, b| a.task_key.cmp(&b.task_key));
        Ok(records)
    }


    /// Loads every record an earlier run persisted. A record that does not parse is an error:
    /// the run must not continue over a corrupt artifact.
    pub async fn load(store: &JsonStore) -> Result<Vec<VerificationRecord>> {
        let mut out = Vec::new();
        for path in store.list_json(&store.layout.verify_dir).await? {
            let bytes = tokio::fs::read(&path)
                .await
                .wrap_err_with(|| format!("failed to read {}", path.display()))?;
            let record: VerificationRecord = serde_json::from_slice(&bytes)
                .wrap_err_with(|| format!("corrupt verification record {}", path.display()))?;
            out.push(record);
        }
        out.sort_by(|a, b| a.task_key.cmp(&b.task_key));
        Ok(out)
    }
}
