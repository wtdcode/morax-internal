use std::sync::Arc;

use color_eyre::eyre::Result;
use evolve_knowdit_agent::{ProgressCheckpoint, RunBudget, RunEnd, RunReport, RunRequest};
use evolve_knowdit_project::{PathFence, ProjectConventions, ProjectTools};
use llmy::client::client::LLM;
use llmy::client::settings::LLMSettings;

use crate::harness::{
    AttemptHandle, ConcludeTool, HarnessAttempt, HarnessWorkspace, ListTestFilesTool, RunForgeTool,
    UpdateClaimTool, WriteHarnessFileTool,
};
use crate::prompts::{PocPrompt, TaskContext};
use knowdit_forge::ForgeRunner;

/// Result of the PoC stage for one task. Persisted on its own so a verdict retry never
/// repeats the harness work.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PocOutcome {
    pub attempt: HarnessAttempt,
    pub reports: Vec<RunReport>,
    pub harness_dir: String,
}

/// Drives the reproduction-test agent for one task, with fresh-agent retries that carry the
/// previous harness forward.
pub struct PocRunner {
    pub tools: ProjectTools,
    pub conventions: Arc<ProjectConventions>,
    pub workspace: HarnessWorkspace,
    pub runner: ForgeRunner,
    /// `-v` repeated, as `forge test` expects it.
    pub verbosity_flag: String,
    /// Revert-reason prefix that marks a demonstrated violation.
    pub violation_marker: String,
    pub budget: RunBudget,
    pub attempts: usize,
    /// Step by which an attempt must have run forge once.
    pub first_run_step: usize,
    /// `conclude` is refused until the attempt has run forge once.
    pub force_poc: bool,
    pub output_tail_bytes: usize,
    pub list_test_files_max_depth: usize,
    pub debug_prefix: String,
    pub llm: LLM,
    pub settings: Option<LLMSettings>,
}

impl PocRunner {
    /// An agent that finds a test file left by an earlier attempt may run it without writing
    /// it again; the record still needs the source, so read it back from the scratch directory.
    async fn backfill_harness(dir: &std::path::Path, attempt: &mut HarnessAttempt) {
        if attempt.harness_filename.is_some() {
            return;
        }
        let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".t.sol")
                && let Ok(source) = tokio::fs::read_to_string(&path).await
            {
                attempt.harness_filename = Some(name);
                attempt.harness_source = source;
                return;
            }
        }
    }

    pub async fn run(&self, ctx: &TaskContext) -> Result<PocOutcome> {
        let mut reports = Vec::new();
        let mut previous: Option<HarnessAttempt> = None;
        let mut last_dir = String::new();
        for attempt_no in 0..self.attempts.max(1) {
            let scratch = self
                .workspace
                .scratch(&format!("{}_a{attempt_no}", ctx.task_key), &self.runner)
                .await?;
            let harness_dir = scratch.dir.display().to_string();
            last_dir = harness_dir.clone();
            let attempt = AttemptHandle::default();
            // A claim an earlier attempt updated stays updated: the retry pursues it.
            let carried = previous.as_ref().and_then(|p| p.updated_claim.clone());
            attempt
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .updated_claim = carried.clone();
            // The attempt reads its own harness directory; every other reproduction's stays
            // behind the fence.
            let mut tools = ProjectTools {
                index: self.tools.index.clone(),
                fence: PathFence {
                    hidden: self.tools.fence.hidden.clone(),
                    own: Some(scratch.dir.clone()),
                },
                module_source: self.tools.module_source,
                read_file: self.tools.read_file,
            }
            .tool_box();
            tools.add_tool(WriteHarnessFileTool {
                attempt: attempt.clone(),
                dir: scratch.dir.clone(),
                filename: ctx.filename.clone(),
            });
            tools.add_tool(RunForgeTool {
                attempt: attempt.clone(),
                runner: scratch.runner.clone(),
                match_path_glob: scratch.match_path_glob.clone(),
                output_tail_bytes: self.output_tail_bytes,
                verbosity_flag: self.verbosity_flag.clone(),
                violation_marker: self.violation_marker.clone(),
            });
            tools.add_tool(ConcludeTool {
                attempt: attempt.clone(),
                force_poc: self.force_poc,
            });
            tools.add_tool(UpdateClaimTool {
                attempt: attempt.clone(),
                force_poc: self.force_poc,
            });
            tools.add_tool(ListTestFilesTool {
                repo_root: self.tools.index.root.clone(),
                max_depth: self.list_test_files_max_depth,
            });
            let mut user_prompt = match &previous {
                Some(prev) if !prev.harness_source.is_empty() => PocPrompt::retry_user(
                    ctx,
                    &prev.harness_source,
                    prev.last_summary.as_deref().unwrap_or("(no forge run)"),
                    &self.violation_marker,
                ),
                _ => PocPrompt::user(ctx, &self.violation_marker),
            };
            if let Some(claim) = &carried {
                user_prompt.push_str(&PocPrompt::carried_claim(claim));
            }
            let request = RunRequest {
                label: format!("poc-{}#{attempt_no}", ctx.task_key),
                system_prompt: PocPrompt::system(
                    ctx,
                    &self.conventions,
                    &harness_dir,
                    &self.violation_marker,
                ),
                user_prompt,
                nudge: PocPrompt::nudge(&self.violation_marker),
                tools,
                debug_prefix: format!("{}-poc-{}", self.debug_prefix, ctx.task_key),
                settings: self.settings.clone(),
                checkpoint: Some(ProgressCheckpoint {
                    flag: attempt.ran_forge.clone(),
                    by_step: self.first_run_step,
                    message: PocPrompt::checkpoint(
                        self.first_run_step,
                        &self.violation_marker,
                        &PocPrompt::skeleton(ctx, &self.conventions, &self.violation_marker),
                    ),
                }),
            };
            let report = self.budget.run(&self.llm, request, &attempt.done).await?;
            let mut snapshot = attempt
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            tracing::info!(
                "poc {} attempt {}: {:?} after {} steps, {} forge runs, violated={}",
                ctx.task_key,
                attempt_no,
                report.end,
                report.steps,
                snapshot.runs.len(),
                snapshot.violation_observed
            );
            reports.push(report.clone());
            Self::backfill_harness(&scratch.dir, &mut snapshot).await;
            if snapshot.violation_observed
                || snapshot.concluded.is_some()
                || report.end == RunEnd::Submitted
            {
                return Ok(PocOutcome {
                    attempt: snapshot,
                    reports,
                    harness_dir,
                });
            }
            previous = Some(snapshot);
        }
        Ok(PocOutcome {
            attempt: previous.unwrap_or_default(),
            reports,
            harness_dir: last_dir,
        })
    }
}
