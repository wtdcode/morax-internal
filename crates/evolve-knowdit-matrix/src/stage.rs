use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use clap::Args;
use color_eyre::eyre::{Result, eyre};
use evolve_knowdit_agent::{ProgressCheckpoint, RunBudget, RunEnd, RunRequest, Stage, StageLlms};
use evolve_knowdit_project::ProjectTools;
use evolve_knowdit_types::{
    CoverageMatrix, Dimension, JsonStore, MatrixDraft, Obligation, Property, PropertySet,
    ScenarioSet, ScopeContext, Subject, SubjectRow, TaskMode, TaskSet, VerificationTask,
};
use llmy::agent::tool::ToolBox;
use llmy::client::settings::LLMSettings;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::prompts::MatrixPromptContext;
use crate::tools::{
    DoubtLedger, SubmitDraftTool, SubmitPropertiesTool, SubmitRowTool, SubmitTaskTool,
    SubmitUnitsTool,
};
use crate::validate::{DraftValidator, RowValidator, TaskValidator, UnitsValidator};

/// Matrix-stage knobs, flattened into the CLI.
#[derive(Args, Clone, Debug)]
pub struct MatrixArgs {
    /// Maximum properties (columns) in the matrix.
    #[arg(long = "matrix-max-properties", default_value_t = 14)]
    pub max_properties: usize,
    /// Maximum candidate properties one level draft may propose.
    #[arg(long = "matrix-max-draft-properties", default_value_t = 8)]
    pub max_draft_properties: usize,
    /// How a task's open inputs become reproductions: `subtask` (the scenarios the task's
    /// unit agent writes) or `task` (one per task moving every open input).
    #[arg(long = "matrix-task-mode", default_value = "subtask")]
    pub task_mode: TaskMode,
    /// Most scenarios one task's unit agent writes in `subtask` mode.
    #[arg(long = "matrix-max-subtasks-per-task", default_value_t = 5)]
    pub max_subtasks_per_task: usize,
    /// Agent step budget for one level draft.
    #[arg(long = "matrix-draft-max-steps", default_value_t = 90)]
    pub draft_max_steps: usize,
    /// Agent step budget for merging the property columns.
    #[arg(long = "matrix-properties-max-steps", default_value_t = 40)]
    pub properties_max_steps: usize,
    /// Take every candidate property of the level drafts as a column, numbered in level
    /// order, instead of merging them with an agent.
    #[arg(long = "matrix-skip-property-merge")]
    pub skip_property_merge: bool,
    /// Agent step budget for deciding one subject row.
    #[arg(long = "matrix-row-max-steps", default_value_t = 40)]
    pub row_max_steps: usize,
    /// Agent step budget for writing the task of one obligation cell.
    #[arg(long = "matrix-task-max-steps", default_value_t = 40)]
    pub task_max_steps: usize,
    /// Steps a task agent may spend without calling `submit_task` before it is told to
    /// submit (repeated every that many steps until a submission is attempted).
    #[arg(long = "matrix-task-checkpoint-step", default_value_t = 20)]
    pub task_checkpoint_step: usize,
    /// Agent step budget for writing the scenarios of one task.
    #[arg(long = "matrix-unit-max-steps", default_value_t = 30)]
    pub unit_max_steps: usize,
    /// Times a model that stops without submitting is re-asked.
    #[arg(long = "matrix-max-nudges", default_value_t = 3)]
    pub matrix_max_nudges: usize,
    /// Fresh agent attempts when a run ends without an accepted submission.
    #[arg(long = "matrix-agent-retries", default_value_t = 2)]
    pub agent_retries: usize,
    #[arg(long = "matrix-dimension-concurrency", default_value_t = 3)]
    pub dimension_concurrency: usize,
    #[arg(long = "matrix-row-concurrency", default_value_t = 64)]
    pub row_concurrency: usize,
    /// Task agents running at once, one per obligation cell.
    #[arg(long = "matrix-task-concurrency", default_value_t = 64)]
    pub task_concurrency: usize,
    /// Unit agents running at once, one per task with open inputs.
    #[arg(long = "matrix-unit-concurrency", default_value_t = 64)]
    pub unit_concurrency: usize,
    #[arg(long = "matrix-debug-prefix", default_value = "matrix")]
    pub matrix_debug_prefix: String,
}

#[derive(Debug, Clone)]
pub struct MatrixConfig {
    pub max_properties: usize,
    pub max_draft_properties: usize,
    pub skip_property_merge: bool,
    pub task_mode: TaskMode,
    pub max_subtasks_per_task: usize,
    pub task_checkpoint_step: usize,
    pub draft_budget: RunBudget,
    pub properties_budget: RunBudget,
    pub row_budget: RunBudget,
    pub task_budget: RunBudget,
    pub unit_budget: RunBudget,
    pub agent_retries: usize,
    pub dimension_concurrency: usize,
    pub row_concurrency: usize,
    pub task_concurrency: usize,
    pub unit_concurrency: usize,
    pub debug_prefix: String,
}

impl MatrixArgs {
    /// The shared nudge limit with one stage's step budget.
    fn budget(&self, max_steps: usize) -> RunBudget {
        RunBudget {
            max_steps,
            max_nudges: self.matrix_max_nudges,
        }
    }

    pub fn to_config(&self) -> MatrixConfig {
        MatrixConfig {
            max_properties: self.max_properties,
            max_draft_properties: self.max_draft_properties,
            skip_property_merge: self.skip_property_merge,
            task_mode: self.task_mode,
            max_subtasks_per_task: self.max_subtasks_per_task.max(1),
            task_checkpoint_step: self.task_checkpoint_step,
            draft_budget: self.budget(self.draft_max_steps),
            properties_budget: self.budget(self.properties_max_steps),
            row_budget: self.budget(self.row_max_steps),
            task_budget: self.budget(self.task_max_steps),
            unit_budget: self.budget(self.unit_max_steps),
            agent_retries: self.agent_retries,
            dimension_concurrency: self.dimension_concurrency.max(1),
            row_concurrency: self.row_concurrency.max(1),
            task_concurrency: self.task_concurrency.max(1),
            unit_concurrency: self.unit_concurrency.max(1),
            debug_prefix: self.matrix_debug_prefix.clone(),
        }
    }
}

/// Runs stages 1–4 for a project. Every stage output (level drafts, the property set, every
/// subject row, the assembled matrix, every cell's task, every task's scenarios) is persisted
/// under the store as soon as it is accepted, and a rerun resumes from whatever exists.
pub struct MatrixStage {
    pub store: JsonStore,
    pub tools: ProjectTools,
    pub scope: ScopeContext,
    pub config: MatrixConfig,
    pub llms: StageLlms,
    pub settings: Option<LLMSettings>,
}

/// Everything the matrix stages produced.
#[derive(Debug, Clone)]
pub struct MatrixOutput {
    pub matrix: CoverageMatrix,
    pub tasks: TaskSet,
    /// Every task's scenarios (`subtask` mode), keyed by task id.
    pub scenarios: BTreeMap<String, ScenarioSet>,
}

impl MatrixStage {
    fn prompt_context(&self) -> MatrixPromptContext {
        MatrixPromptContext {
            scope: self.scope.clone(),
            overview: self.tools.index.render_overview(),
            tool_guide: self.tools.render_tool_guide(),
        }
    }

    /// Runs fresh agents over the same request, on the LLM of `stage`, until the submit tool
    /// in `tools` raises `accepted`, up to the configured number of attempts. A run whose step
    /// errors out counts as a failed attempt.
    async fn run_until_accepted(
        &self,
        stage: Stage,
        label: String,
        budget: &RunBudget,
        tools: ToolBox,
        system_prompt: String,
        user_prompt: String,
        nudge: String,
        accepted: &AtomicBool,
        doubts: Option<&DoubtLedger>,
        checkpoint: Option<ProgressCheckpoint>,
    ) -> Result<()> {
        let mut attempt = 0;
        let mut waits = 0u32;
        while attempt <= self.config.agent_retries {
            accepted.store(false, Ordering::SeqCst);
            // A fresh agent has not seen the doubts raised to an earlier one.
            if let Some(doubts) = doubts {
                doubts.raised.lock().unwrap_or_else(|p| p.into_inner()).clear();
            }
            if let Some(checkpoint) = &checkpoint {
                checkpoint.flag.store(false, Ordering::SeqCst);
            }
            let request = RunRequest {
                label: format!("{label}#{attempt}"),
                system_prompt: system_prompt.clone(),
                user_prompt: user_prompt.clone(),
                nudge: nudge.clone(),
                tools: tools.clone(),
                debug_prefix: format!("{}-{label}", self.config.debug_prefix),
                settings: self.settings.clone(),
                checkpoint: checkpoint.clone(),
            };
            match budget.run(self.llms.of(stage), request, accepted).await {
                Ok(report) => {
                    tracing::info!(
                        "{label}#{attempt}: {:?} after {} steps ({} nudges)",
                        report.end,
                        report.steps,
                        report.nudges
                    );
                    if report.end == RunEnd::Submitted {
                        return Ok(());
                    }
                    attempt += 1;
                }
                Err(e) => {
                    // A provider outage is not the agent's failure: wait it out without
                    // spending an attempt (a short burst of 503s could otherwise use up every
                    // attempt within a second).
                    if RunBudget::is_transient(&format!("{e:#}")) && waits < 6 {
                        waits += 1;
                        let pause = std::time::Duration::from_secs(30 * u64::from(waits));
                        tracing::warn!(
                            "{label}#{attempt}: transient error from the LLM provider, retrying the same attempt in {}s ({waits}/6): {e:#}",
                            pause.as_secs()
                        );
                        tokio::time::sleep(pause).await;
                        continue;
                    }
                    tracing::warn!(
                        "{label}#{attempt}: run failed, retrying with a fresh agent: {e:#}"
                    );
                    attempt += 1;
                }
            }
        }
        Err(eyre!(
            "{label}: no accepted submission after {} attempt(s)",
            self.config.agent_retries + 1
        ))
    }

    /// Stage 1 for one level; persisted at `matrix/drafts/<level>.json`.
    pub async fn draft_dimension(&self, dimension: Dimension) -> Result<MatrixDraft> {
        let path = self
            .store
            .layout
            .drafts_dir
            .join(dimension.slug())
            .with_extension("json");
        if let Some(existing) = self.store.read_optional::<MatrixDraft>(&path).await? {
            tracing::info!("{dimension}: draft already exists, reusing");
            return Ok(existing);
        }
        let prompt = self.prompt_context();
        let inventory = self.tools.index.render_inventory(dimension);
        let tool = SubmitDraftTool {
            validator: DraftValidator {
                index: self.tools.index.clone(),
                dimension,
                max_properties: self.config.max_draft_properties,
            },
            doubts: DoubtLedger::default(),
            accepted: Arc::new(AtomicBool::new(false)),
            value: Arc::new(Mutex::new(None)),
        };
        let mut tools = self.tools.tool_box();
        tools.add_tool(tool.clone());
        self.run_until_accepted(
            Stage::Draft,
            format!("draft-{dimension}"),
            &self.config.draft_budget,
            tools,
            prompt.draft_system(dimension, self.config.max_draft_properties),
            prompt.draft_user(dimension, &inventory),
            MatrixPromptContext::nudge("submit_draft"),
            &tool.accepted,
            Some(&tool.doubts),
            None,
        )
        .await?;
        let draft = tool
            .value
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .ok_or_else(|| eyre!("{dimension}: accepted draft missing"))?;
        self.store.write(&path, &draft).await?;
        tracing::info!("{dimension}: draft accepted with {}", draft.describe());
        Ok(draft)
    }

    /// Stage 2a: the merged columns; persisted at `matrix/properties.json`.
    pub async fn merge_properties(&self, drafts: &[MatrixDraft]) -> Result<PropertySet> {
        let path = self.store.layout.properties_file.clone();
        if let Some(existing) = self.store.read_optional::<PropertySet>(&path).await? {
            tracing::info!("property set already exists, reusing");
            return Ok(existing);
        }
        if self.config.skip_property_merge {
            let set = PropertySet {
                scope_snapshot: drafts
                    .iter()
                    .map(|d| d.scope_snapshot.trim())
                    .collect::<Vec<_>>()
                    .join("\n\n"),
                properties: drafts
                    .iter()
                    .flat_map(|d| d.properties.iter().map(move |p| (d.dimension, p)))
                    .enumerate()
                    .map(|(idx, (origin, p))| Property {
                        id: format!("P{:02}", idx + 1),
                        origin,
                        ..p.clone()
                    })
                    .collect(),
            };
            self.store.write(&path, &set).await?;
            tracing::info!(
                "property merge skipped: {} draft properties taken as columns",
                set.properties.len()
            );
            return Ok(set);
        }
        let prompt = self.prompt_context();
        let tool = SubmitPropertiesTool {
            max_properties: self.config.max_properties,
            accepted: Arc::new(AtomicBool::new(false)),
            value: Arc::new(Mutex::new(None)),
        };
        let mut tools = self.tools.tool_box();
        tools.add_tool(tool.clone());
        self.run_until_accepted(
            Stage::Properties,
            "properties".to_string(),
            &self.config.properties_budget,
            tools,
            prompt.properties_system(self.config.max_properties),
            prompt.properties_user(drafts),
            MatrixPromptContext::nudge("submit_properties"),
            &tool.accepted,
            None,
            None,
        )
        .await?;
        let set = tool
            .value
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .ok_or_else(|| eyre!("accepted property set missing"))?;
        self.store.write(&path, &set).await?;
        tracing::info!(
            "property set accepted with {} properties",
            set.properties.len()
        );
        Ok(set)
    }

    /// Stage 2b for one subject; persisted at `matrix/rows/<subject>.json`.
    pub async fn decide_row(
        &self,
        subject: &Subject,
        properties: &[Property],
        scope_snapshot: &str,
    ) -> Result<SubjectRow> {
        let path = self
            .store
            .layout
            .rows_dir
            .join(&subject.id)
            .with_extension("json");
        if let Some(existing) = self.store.read_optional::<SubjectRow>(&path).await? {
            return Ok(existing);
        }
        let prompt = self.prompt_context();
        let subject_functions = self.tools.index.extent_functions(&subject.anchors);
        let tool = SubmitRowTool {
            validator: RowValidator {
                index: self.tools.index.clone(),
                subject: subject.clone(),
                properties: properties.to_vec(),
            },
            doubts: DoubtLedger::default(),
            accepted: Arc::new(AtomicBool::new(false)),
            value: Arc::new(Mutex::new(None)),
        };
        let mut tools = self.tools.tool_box();
        tools.add_tool(tool.clone());
        self.run_until_accepted(
            Stage::Row,
            format!("row-{}", subject.id),
            &self.config.row_budget,
            tools,
            prompt.row_system(),
            prompt.row_user(subject, properties, scope_snapshot, &subject_functions),
            MatrixPromptContext::nudge("submit_row"),
            &tool.accepted,
            Some(&tool.doubts),
            None,
        )
        .await?;
        let row = tool
            .value
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .ok_or_else(|| eyre!("{}: accepted row missing", subject.id))?;
        self.store.write(&path, &row).await?;
        Ok(row)
    }

    /// The matrix assembled from the parts; persisted at `matrix/matrix.json`.
    async fn assemble(
        &self,
        drafts: &[MatrixDraft],
        properties: &PropertySet,
        rows: &[SubjectRow],
    ) -> Result<CoverageMatrix> {
        let matrix = CoverageMatrix::assemble(drafts, properties, rows);
        let errors = matrix.structural_errors();
        if !errors.is_empty() {
            return Err(eyre!("assembled matrix is inconsistent: {}", errors.join("; ")));
        }
        self.store
            .write(&self.store.layout.matrix_file, &matrix)
            .await?;
        tracing::info!("matrix assembled: {}", matrix.describe());
        Ok(matrix)
    }

    /// Stage 3 for one obligation cell; persisted at `matrix/tasks/<cell>.json`.
    pub async fn task_for_cell(
        &self,
        matrix: &CoverageMatrix,
        obligation: &Obligation,
    ) -> Result<VerificationTask> {
        let cell = &obligation.cell;
        let path = self
            .store
            .layout
            .tasks_dir
            .join(cell.key())
            .with_extension("json");
        if let Some(existing) = self.store.read_optional::<VerificationTask>(&path).await? {
            return Ok(existing);
        }
        let prompt = self.prompt_context();
        // The functions listed to the agent: what the obligation's anchors and the subject's
        // reach.
        let mut universe = obligation.anchors.clone();
        if let Some(subject) = matrix.subjects.get(&cell.subject_id) {
            universe.extend(subject.anchors.iter().cloned());
        }
        let functions = self.tools.index.extent_functions(&universe);
        let attempted = Arc::new(AtomicBool::new(false));
        let tool = SubmitTaskTool {
            validator: TaskValidator {
                cell: cell.clone(),
                functions: functions.clone(),
                exposed: self.tools.index.exposed_functions(),
            },
            doubts: DoubtLedger::default(),
            accepted: Arc::new(AtomicBool::new(false)),
            value: Arc::new(Mutex::new(None)),
            attempted: attempted.clone(),
        };
        let mut tools = self.tools.tool_box();
        tools.add_tool(tool.clone());
        self.run_until_accepted(
            Stage::Task,
            format!("task-{}", cell.key()),
            &self.config.task_budget,
            tools,
            prompt.tasks_system(),
            prompt.tasks_user(
                matrix,
                cell,
                &obligation.statement,
                &obligation.anchors,
                &functions,
            ),
            MatrixPromptContext::nudge("submit_task"),
            &tool.accepted,
            Some(&tool.doubts),
            Some(ProgressCheckpoint {
                flag: attempted,
                by_step: self.config.task_checkpoint_step,
                message: MatrixPromptContext::task_checkpoint(self.config.task_checkpoint_step),
            }),
        )
        .await?;
        let task = tool
            .value
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .ok_or_else(|| eyre!("{cell}: accepted task missing"))?;
        self.store.write(&path, &task).await?;
        Ok(task)
    }

    /// Stage 4 for one task: the scenarios its unit agent writes; persisted at
    /// `matrix/units/<task>.json`.
    pub async fn units_for_task(
        &self,
        matrix: &CoverageMatrix,
        task: &VerificationTask,
    ) -> Result<ScenarioSet> {
        let path = self
            .store
            .layout
            .units_dir
            .join(&task.id)
            .with_extension("json");
        if let Some(existing) = self.store.read_optional::<ScenarioSet>(&path).await? {
            return Ok(existing);
        }
        let prompt = self.prompt_context();
        // The listing the task agent chose from (what the obligations' anchors and the
        // subjects' reach), plus the task's own function, which the agent may have kept from
        // outside it.
        let mut universe = Vec::new();
        for c in &task.cells {
            if let Some(o) = matrix.obligations.iter().find(|o| o.cell == c.cell) {
                universe.extend(o.anchors.iter().cloned());
            }
            if let Some(subject) = matrix.subjects.get(&c.cell.subject_id) {
                universe.extend(subject.anchors.iter().cloned());
            }
        }
        if let Some(f) = &task.function
            && let Some(anchor) = self.tools.index.function_anchor(&f.contract, &f.function)
        {
            universe.push(anchor);
        }
        let functions = self.tools.index.extent_functions(&universe);
        let attempted = Arc::new(AtomicBool::new(false));
        let tool = SubmitUnitsTool {
            validator: UnitsValidator {
                task: task.clone(),
                functions: functions.clone(),
                exposed: self.tools.index.exposed_functions(),
                max_scenarios: self.config.max_subtasks_per_task,
            },
            doubts: DoubtLedger::default(),
            accepted: Arc::new(AtomicBool::new(false)),
            value: Arc::new(Mutex::new(None)),
            attempted: attempted.clone(),
        };
        let mut tools = self.tools.tool_box();
        tools.add_tool(tool.clone());
        self.run_until_accepted(
            Stage::Unit,
            format!("unit-{}", task.id),
            &self.config.unit_budget,
            tools,
            prompt.units_system(self.config.max_subtasks_per_task),
            prompt.units_user(matrix, task, &functions, self.config.max_subtasks_per_task),
            MatrixPromptContext::nudge("submit_units"),
            &tool.accepted,
            Some(&tool.doubts),
            Some(ProgressCheckpoint {
                flag: attempted,
                by_step: self.config.task_checkpoint_step,
                message: MatrixPromptContext::units_checkpoint(self.config.task_checkpoint_step),
            }),
        )
        .await?;
        let set = tool
            .value
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .ok_or_else(|| eyre!("{}: accepted scenarios missing", task.id))?;
        self.store.write(&path, &set).await?;
        Ok(set)
    }

    /// Stages 1–4: level drafts in parallel, one property merge, subject rows in parallel, the
    /// assembled matrix, one task per obligation cell in parallel, then (`subtask` mode) the
    /// scenarios of every task with open inputs in parallel. A failed draft, row, task or
    /// scenario set is logged and left for a rerun; a failed property merge ends the run.
    pub async fn run(self: Arc<Self>, dimensions: Vec<Dimension>) -> Result<MatrixOutput> {
        let mut drafts = Vec::new();
        let gate = Arc::new(Semaphore::new(self.config.dimension_concurrency));
        let mut set = JoinSet::new();
        for dimension in dimensions {
            let stage = self.clone();
            let gate = gate.clone();
            set.spawn(async move {
                let _permit = gate.acquire_owned().await;
                stage.draft_dimension(dimension).await
            });
        }
        while let Some(joined) = set.join_next().await {
            match joined? {
                Ok(draft) => drafts.push(draft),
                Err(e) => tracing::error!("level draft failed (rerun to resume): {e:#}"),
            }
        }
        if drafts.is_empty() {
            return Err(eyre!("no level draft was accepted"));
        }
        drafts.sort_by_key(|d| d.dimension);
        let properties = self.merge_properties(&drafts).await?;

        let mut rows = Vec::new();
        let gate = Arc::new(Semaphore::new(self.config.row_concurrency));
        let mut set = JoinSet::new();
        for subject in drafts.iter().flat_map(|d| d.subjects.iter()) {
            let stage = self.clone();
            let gate = gate.clone();
            let subject = subject.clone();
            let properties = properties.clone();
            set.spawn(async move {
                let _permit = gate.acquire_owned().await;
                stage
                    .decide_row(&subject, &properties.properties, &properties.scope_snapshot)
                    .await
            });
        }
        while let Some(joined) = set.join_next().await {
            match joined? {
                Ok(row) => rows.push(row),
                Err(e) => tracing::error!("subject row failed (rerun to resume): {e:#}"),
            }
        }
        let expected: usize = drafts.iter().map(|d| d.subjects.len()).sum();
        if rows.len() != expected {
            return Err(eyre!(
                "{} of {expected} subject rows decided; rerun to resume the rest",
                rows.len()
            ));
        }
        rows.sort_by(|a, b| a.subject_id.cmp(&b.subject_id));
        let matrix = Arc::new(self.assemble(&drafts, &properties, &rows).await?);

        let mut tasks = Vec::new();
        let gate = Arc::new(Semaphore::new(self.config.task_concurrency));
        let mut set = JoinSet::new();
        for obligation in matrix.obligations.clone() {
            let stage = self.clone();
            let gate = gate.clone();
            let matrix = matrix.clone();
            set.spawn(async move {
                let _permit = gate.acquire_owned().await;
                stage.task_for_cell(&matrix, &obligation).await
            });
        }
        while let Some(joined) = set.join_next().await {
            match joined? {
                Ok(task) => tasks.push(task),
                Err(e) => tracing::error!("cell task failed (rerun to resume): {e:#}"),
            }
        }
        tasks.sort_by(|a, b| a.id.cmp(&b.id));
        tracing::info!(
            "task stage: {} of {} obligations have a task",
            tasks.len(),
            matrix.obligations.len()
        );

        let mut scenarios = BTreeMap::new();
        if self.config.task_mode == TaskMode::Subtask {
            let gate = Arc::new(Semaphore::new(self.config.unit_concurrency));
            let mut set = JoinSet::new();
            let pending: Vec<VerificationTask> = tasks
                .iter()
                .filter(|t| t.function.is_some() && !t.open_inputs().is_empty())
                .cloned()
                .collect();
            let expected = pending.len();
            for task in pending {
                let stage = self.clone();
                let gate = gate.clone();
                let matrix = matrix.clone();
                set.spawn(async move {
                    let _permit = gate.acquire_owned().await;
                    let result = stage.units_for_task(&matrix, &task).await;
                    (task.id, result)
                });
            }
            while let Some(joined) = set.join_next().await {
                match joined? {
                    (id, Ok(found)) => {
                        scenarios.insert(id, found);
                    }
                    (id, Err(e)) => {
                        tracing::error!("{id}: scenarios failed (rerun to resume): {e:#}")
                    }
                }
            }
            tracing::info!(
                "unit stage: {} of {expected} tasks with open inputs have scenarios ({} scenarios)",
                scenarios.len(),
                scenarios.values().map(|s: &ScenarioSet| s.scenarios.len()).sum::<usize>()
            );
        }
        Ok(MatrixOutput {
            matrix: (*matrix).clone(),
            tasks: TaskSet { tasks },
            scenarios,
        })
    }

    /// Loads what an earlier run persisted, for stages that run after the matrix.
    pub async fn load(store: &JsonStore) -> Result<Option<MatrixOutput>> {
        let Some(matrix) = store
            .read_optional::<CoverageMatrix>(&store.layout.matrix_file)
            .await?
        else {
            return Ok(None);
        };
        let mut tasks = Vec::new();
        let mut scenarios = BTreeMap::new();
        for obligation in &matrix.obligations {
            if let Some(task) = store
                .read_optional::<VerificationTask>(
                    &store
                        .layout
                        .tasks_dir
                        .join(obligation.cell.key())
                        .with_extension("json"),
                )
                .await?
            {
                if let Some(set) = store
                    .read_optional::<ScenarioSet>(
                        &store.layout.units_dir.join(&task.id).with_extension("json"),
                    )
                    .await?
                {
                    scenarios.insert(task.id.clone(), set);
                }
                tasks.push(task);
            }
        }
        Ok(Some(MatrixOutput {
            matrix,
            tasks: TaskSet { tasks },
            scenarios,
        }))
    }
}
