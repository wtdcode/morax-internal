use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use color_eyre::eyre::{Result, eyre};

use evolve_knowdit_agent::{RunBudget, RunEnd, RunRequest};
use evolve_knowdit_project::ProjectTools;
use evolve_knowdit_types::{ScopeContext, SequenceCoverage, Verdict, VerdictClass};
use knowdit_forge::{ForgeRunner, RunKind};
use llmy::agent::LLMYError;
use llmy::agent::tool::Tool;
use llmy::client::client::LLM;
use llmy::client::settings::LLMSettings;

use crate::forge::LcovReport;
use crate::harness::{HarnessAttempt, HarnessWorkspace};
use crate::prompts::{TaskContext, VerdictPrompt};

/// The verdict agent's submit tool: checks the verdict, stores it and raises `accepted`.
#[derive(Debug, Clone)]
pub struct EmitVerdictTool {
    pub accepted: Arc<AtomicBool>,
    pub value: Arc<Mutex<Option<Verdict>>>,
}

impl Tool for EmitVerdictTool {
    type ARGUMENTS = Verdict;
    const NAME: &str = "emit_verdict";
    const DESCRIPTION: Option<&str> = Some(
        "Finalize the verdict on the demonstrated violation: the class, a rationale reporting each check in order with the project source and harness lines it rests on, for a rejection the ruled-out project fact (ruled_out_claim, and ruled_out_evidence when anchored), and for ValidFinding the client-facing finding (title, severity, location, root_cause, description, impact, recommendation). The user message shows the exact payload shape. A rejected submission returns the problems to fix, an accepted one ends the run.",
    );

    async fn invoke(&self, verdict: Verdict) -> Result<String, LLMYError> {
        let errors = verdict.structural_errors();
        if !errors.is_empty() {
            return Ok(format!(
                "rejected: {} problem(s). Fix all of them and call emit_verdict again with the whole payload.\n- {}",
                errors.len(),
                errors.join("\n- ")
            ));
        }
        let class = verdict.class;
        *self.value.lock().unwrap_or_else(|p| p.into_inner()) = Some(verdict);
        self.accepted.store(true, Ordering::SeqCst);
        Ok(format!("accepted: verdict {class:?}"))
    }
}

/// Judges one demonstrated violation.
pub struct VerdictRunner {
    pub tools: ProjectTools,
    pub scope: ScopeContext,
    /// Where the coverage re-run of the harness is written.
    pub workspace: HarnessWorkspace,
    pub runner: ForgeRunner,
    /// The project compiles through the IR pipeline, which `forge coverage` disables unless
    /// told otherwise.
    pub via_ir: bool,
    pub budget: RunBudget,
    pub retries: usize,
    pub debug_prefix: String,
    pub llm: LLM,
    pub settings: Option<LLMSettings>,
}

impl VerdictRunner {
    /// The violating harness re-run under `forge coverage` with the same test selection, and
    /// each step of the unit's sequence checked against the lines it executed (knowdit's
    /// coverage gate, given to the verdict as evidence). `None` when no report came out.
    async fn sequence_coverage(
        &self,
        ctx: &TaskContext,
        attempt: &HarnessAttempt,
    ) -> Option<SequenceCoverage> {
        let filename = attempt.harness_filename.as_ref()?;
        let violating = attempt.runs.iter().rev().find(|r| r.violated)?;
        let scratch = match self
            .workspace
            .scratch(&format!("{}_cov", ctx.task_key), &self.runner)
            .await
        {
            Ok(scratch) => scratch,
            Err(e) => {
                tracing::warn!("verdict {}: no coverage directory: {e:#}", ctx.task_key);
                return None;
            }
        };
        let report_file = scratch.dir.join("lcov.info");
        let _ = tokio::fs::remove_file(&report_file).await;
        if let Err(e) = tokio::fs::write(scratch.dir.join(filename), &attempt.harness_source).await
        {
            tracing::warn!("verdict {}: cannot write the harness for coverage: {e:#}", ctx.task_key);
            return None;
        }
        let mut argv = vec![
            "coverage".to_string(),
            "--report".to_string(),
            "lcov".to_string(),
            "--report-file".to_string(),
            report_file.to_string_lossy().to_string(),
            "--match-path".to_string(),
            scratch.match_path_glob.clone(),
        ];
        for pair in violating.forge_args.windows(2) {
            if pair[0] == "--match-contract" || pair[0] == "--match-test" {
                argv.extend(pair.iter().cloned());
            }
        }
        if self.via_ir && self.runner.features().coverage_force_via_ir {
            argv.push("--force-via-ir".to_string());
        }
        match scratch.runner.run(RunKind::Coverage, &argv).await {
            Ok(out) if out.timed_out => {
                tracing::warn!("verdict {}: forge coverage timed out", ctx.task_key);
                return None;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("verdict {}: forge coverage failed: {e:#}", ctx.task_key);
                return None;
            }
        }
        let text = match tokio::fs::read_to_string(&report_file).await {
            Ok(text) => text,
            Err(e) => {
                // A compile failure under coverage (optimizer off: "stack too deep") writes none.
                tracing::warn!("verdict {}: forge coverage wrote no report: {e}", ctx.task_key);
                return None;
            }
        };
        let lcov = LcovReport::parse(&text);
        if lcov.is_empty() {
            tracing::warn!("verdict {}: forge coverage wrote no executed line", ctx.task_key);
            return None;
        }
        let coverage = lcov.sequence_coverage(&ctx.task.sequence, &self.tools.index);
        tracing::info!("verdict {}: sequence coverage {}", ctx.task_key, coverage.render());
        Some(coverage)
    }

    pub async fn judge(&self, ctx: &TaskContext, attempt: &HarnessAttempt) -> Result<Verdict> {
        let digest = attempt
            .last_summary
            .clone()
            .unwrap_or_else(|| "(no digest)".to_string());
        let stdout_tail = attempt
            .runs
            .iter()
            .rev()
            .find(|r| r.violated)
            .map(|r| r.stdout_tail.clone())
            .unwrap_or_default();
        let system = VerdictPrompt::system(&self.scope);
        let coverage = self.sequence_coverage(ctx, attempt).await;
        let user = VerdictPrompt::user(
            ctx,
            &attempt.harness_source,
            &digest,
            &stdout_tail,
            attempt.updated_claim.as_ref(),
            coverage.as_ref(),
        );
        for retry in 0..=self.retries {
            let tool = EmitVerdictTool {
                accepted: Arc::new(AtomicBool::new(false)),
                value: Arc::new(Mutex::new(None)),
            };
            let mut tools = self.tools.tool_box();
            tools.add_tool(tool.clone());
            let request = RunRequest {
                label: format!("verdict-{}#{retry}", ctx.task_key),
                system_prompt: system.clone(),
                user_prompt: user.clone(),
                nudge: VerdictPrompt::nudge(),
                tools,
                debug_prefix: format!("{}-verdict-{}", self.debug_prefix, ctx.task_key),
                settings: self.settings.clone(),
                checkpoint: None,
            };
            let report = self.budget.run(&self.llm, request, &tool.accepted).await?;
            let submitted = tool.value.lock().unwrap_or_else(|p| p.into_inner()).take();
            if report.end == RunEnd::Submitted
                && let Some(mut verdict) = submitted
            {
                if verdict.class == VerdictClass::ValidFinding {
                    verdict.ruled_out_claim = None;
                    verdict.ruled_out_evidence = None;
                } else {
                    verdict.finding = None;
                }
                verdict.sequence_coverage = coverage.clone();
                tracing::info!(
                    "verdict {}: {:?} after {} steps",
                    ctx.task_key,
                    verdict.class,
                    report.steps
                );
                return Ok(verdict);
            }
        }
        Err(eyre!("verdict {}: no verdict emitted", ctx.task_key))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use llmy::agent::tool::ToolDyn;

    use super::EmitVerdictTool;

    #[test]
    fn emit_verdict_schema_types_the_class_and_the_finding() {
        let tool = EmitVerdictTool {
            accepted: Default::default(),
            value: Default::default(),
        };
        let schema = tool.schema().to_value();
        assert_eq!(schema["properties"]["rationale"]["type"], "string");
        assert!(schema["properties"].get("ruled_out_claim").is_some(), "{schema}");
        assert!(schema["properties"].get("sequence_coverage").is_none(), "{schema}");
        let text = schema.to_string();
        assert!(text.contains("HarnessArtifact"), "{text}");
        assert!(text.contains("root_cause"), "{text}");
    }

    #[test]
    fn the_engagement_threat_model_replaces_the_default_adversary_model() {
        use crate::prompts::VerdictPrompt;
        use evolve_knowdit_types::ScopeContext;

        let default = VerdictPrompt::system(&ScopeContext::default());
        assert!(default.contains(&VerdictPrompt::default_adversary_model()));
        let injected = VerdictPrompt::system(&ScopeContext {
            threat_model: Some("Owner operations that harm holders are in scope.".to_string()),
            ..ScopeContext::default()
        });
        assert!(injected.contains("Owner operations that harm holders are in scope."));
        assert!(!injected.contains(&VerdictPrompt::default_adversary_model()));
    }

    #[tokio::test]
    async fn emit_verdict_accepts_a_null_finding_for_non_valid_classes() {
        let tool = EmitVerdictTool {
            accepted: Default::default(),
            value: Default::default(),
        };
        let payload = serde_json::json!({
            "class": "HarnessArtifact",
            "rationale": "the harness etches a stub price feed that returns a zero price, which the real feed never returns",
            "ruled_out_claim": "the price feed reverts on a zero answer, so a zero price never reaches Vault.deposit",
            "finding": null
        });
        let result = tool.run(payload).await.expect("tool runs");
        assert!(result.starts_with("accepted"), "{result}");
        assert!(tool.accepted.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn emit_verdict_refuses_a_rejection_without_the_ruled_out_fact() {
        let tool = EmitVerdictTool {
            accepted: Default::default(),
            value: Default::default(),
        };
        let payload = serde_json::json!({
            "class": "ExpectedBehavior",
            "rationale": "the NatSpec of Vault.redeem documents that dust redemptions burn shares for zero assets",
            "finding": null
        });
        let result = tool.run(payload).await.expect("tool runs");
        assert!(result.contains("ruled_out_claim"), "{result}");
        assert!(!tool.accepted.load(Ordering::SeqCst));
    }
}
