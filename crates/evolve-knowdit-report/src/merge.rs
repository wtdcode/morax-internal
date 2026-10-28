use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use clap::Args;
use color_eyre::eyre::{Result, eyre};
use evolve_knowdit_agent::{RunBudget, RunEnd, RunRequest};
use evolve_knowdit_project::ProjectTools;
use evolve_knowdit_types::{Finding, FindingCluster, ScopeContext};
use llmy::agent::LLMYError;
use llmy::agent::tool::Tool;
use llmy::client::client::LLM;
use llmy::client::settings::LLMSettings;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::prompts::MergePrompt;

/// Report-stage knobs, flattened into the CLI.
#[derive(Args, Clone, Debug)]
pub struct MergeArgs {
    /// Agent step budget for clustering the findings.
    #[arg(long = "merge-max-steps", default_value_t = 40)]
    pub merge_max_steps: usize,
    #[arg(long = "merge-max-nudges", default_value_t = 3)]
    pub merge_max_nudges: usize,
    /// Fresh agent attempts when a run ends without an accepted submission.
    #[arg(long = "merge-agent-retries", default_value_t = 2)]
    pub merge_agent_retries: usize,
    #[arg(long = "merge-debug-prefix", default_value = "merge")]
    pub merge_debug_prefix: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ClustersPayload {
    /// Every finding id in exactly one cluster.
    pub clusters: Vec<FindingCluster>,
}

/// The merge agent's submit tool: checks that the clusters partition the findings, stores
/// them and raises `accepted`.
#[derive(Debug, Clone)]
pub struct SubmitClustersTool {
    pub finding_ids: Vec<String>,
    pub accepted: Arc<AtomicBool>,
    pub value: Arc<Mutex<Option<Vec<FindingCluster>>>>,
}

impl Tool for SubmitClustersTool {
    type ARGUMENTS = ClustersPayload;
    const NAME: &str = "submit_clusters";
    const DESCRIPTION: Option<&str> = Some(
        "Submit the clustering of the findings by root cause: every finding id in exactly one cluster, each cluster with its representative and a one-line root cause. The user message shows the exact payload shape. A rejected submission returns the problems to fix, an accepted one ends the run.",
    );

    async fn invoke(&self, args: ClustersPayload) -> Result<String, LLMYError> {
        let clusters = args.clusters;
        let errors = FindingCluster::structural_errors(&clusters, &self.finding_ids);
        if !errors.is_empty() {
            return Ok(format!(
                "rejected: {} problem(s). Fix all of them and call submit_clusters again with the whole payload.\n- {}",
                errors.len(),
                errors.join("\n- ")
            ));
        }
        let count = clusters.len();
        *self.value.lock().unwrap_or_else(|p| p.into_inner()) = Some(clusters);
        self.accepted.store(true, Ordering::SeqCst);
        Ok(format!("accepted: {count} cluster(s)"))
    }
}

/// Clusters the valid findings by root cause with an agent that can read the project.
pub struct FindingMerger {
    pub tools: ProjectTools,
    pub scope: ScopeContext,
    pub budget: RunBudget,
    pub retries: usize,
    pub debug_prefix: String,
    pub llm: LLM,
    pub settings: Option<LLMSettings>,
}

impl FindingMerger {
    pub async fn merge(&self, findings: &[Finding]) -> Result<Vec<FindingCluster>> {
        if findings.is_empty() {
            return Ok(Vec::new());
        }
        let finding_ids: Vec<String> = findings.iter().map(|f| f.id.clone()).collect();
        let system = MergePrompt::system(
            &self.scope,
            &self.tools.index.render_overview(),
            &self.tools.render_tool_guide(),
        );
        let user = MergePrompt::user(findings);
        for attempt in 0..=self.retries {
            let tool = SubmitClustersTool {
                finding_ids: finding_ids.clone(),
                accepted: Arc::new(AtomicBool::new(false)),
                value: Arc::new(Mutex::new(None)),
            };
            let mut tools = self.tools.tool_box();
            tools.add_tool(tool.clone());
            let request = RunRequest {
                label: format!("merge#{attempt}"),
                system_prompt: system.clone(),
                user_prompt: user.clone(),
                nudge: MergePrompt::nudge(),
                tools,
                debug_prefix: format!("{}-merge", self.debug_prefix),
                settings: self.settings.clone(),
                checkpoint: None,
            };
            let report = self.budget.run(&self.llm, request, &tool.accepted).await?;
            let submitted = tool.value.lock().unwrap_or_else(|p| p.into_inner()).take();
            if report.end == RunEnd::Submitted
                && let Some(clusters) = submitted
            {
                tracing::info!(
                    "merge: {} findings in {} clusters after {} steps",
                    findings.len(),
                    clusters.len(),
                    report.steps
                );
                return Ok(clusters);
            }
        }
        Err(eyre!(
            "merge: no accepted clustering after {} attempt(s)",
            self.retries + 1
        ))
    }
}
