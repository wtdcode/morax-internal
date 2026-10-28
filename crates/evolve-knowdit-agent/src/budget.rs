use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use color_eyre::eyre::{Result, WrapErr};
use llmy::agent::{StepResult, tool::ToolBox};
use llmy::client::client::LLM;
use llmy::client::settings::LLMSettings;
use llmy::harness::{Agent, AgentConfig};
use serde::{Deserialize, Serialize};

/// Step limits for one agent run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunBudget {
    /// Model round-trips allowed.
    pub max_steps: usize,
    /// Times the loop re-asks a model that ended its turn without submitting.
    pub max_nudges: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunEnd {
    /// The submit tool accepted a payload.
    Submitted,
    /// The step budget ran out before a submission.
    Exhausted,
    /// The model kept ending its turn without submitting.
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub end: RunEnd,
    pub steps: usize,
    pub nudges: usize,
    pub last_message: Option<String>,
}

/// A mid-run check: while `flag` is still down, `message` is sent as a user turn at step
/// `by_step` and again every `by_step` steps after that. A tool raises the flag when the run
/// has made the progress the message asks for (a PoC agent's first forge run, for instance).
#[derive(Debug, Clone)]
pub struct ProgressCheckpoint {
    pub flag: Arc<AtomicBool>,
    pub by_step: usize,
    pub message: String,
}

/// The prompts and tools of one agent run.
pub struct RunRequest {
    pub label: String,
    pub system_prompt: String,
    pub user_prompt: String,
    /// Text sent when the model ends its turn without having submitted.
    pub nudge: String,
    pub tools: ToolBox,
    pub debug_prefix: String,
    pub settings: Option<LLMSettings>,
    pub checkpoint: Option<ProgressCheckpoint>,
}

impl RunBudget {
    /// A transport or provider failure that says nothing about the request: the caller
    /// waits and retries instead of spending an attempt or recording an outcome.
    pub fn is_transient(text: &str) -> bool {
        let lower = text.to_ascii_lowercase();
        [
            "error sending request",
            "temporarily unavailable",
            "upstream rejected",
            "upstream_error",
            "bad gateway",
            "rate limit",
            "rate_limit",
            "too many pending requests",
            "queue_full",
            // A provider's per-account cap on requests in flight.
            "concurrency limit exceeded",
            "status: 429",
            "status: 503",
            "status: 502",
            "status: 504",
            "http error",
            "timed out",
            "timeout",
            "connection reset",
            "broken pipe",
            // A chat request answered by a Responses-API validator (a proxy that misroutes it).
            "input should be a valid string",
            "validation errors",
            "invalid_request_error",
        ]
        .iter()
        .any(|w| lower.contains(w))
    }

    /// Drives a fresh agent over `request` until `accepted` is raised by a submit tool or the
    /// budget ends. Tools run one at a time so a model that matches results by position never
    /// mixes them up.
    pub async fn run(
        &self,
        llm: &LLM,
        request: RunRequest,
        accepted: &AtomicBool,
    ) -> Result<RunReport> {
        let mut agent = Agent::new_with_config(
            request.system_prompt,
            request.tools,
            None,
            AgentConfig::default().sequential_toolcall(),
        );
        let prefix = Some(request.debug_prefix.as_str());
        let label = request.label;
        let mut steps = 0usize;
        let mut nudges = 0usize;
        let mut next_checkpoint = request.checkpoint.as_ref().map(|c| c.by_step);
        let mut step = agent
            .step_with_user(request.user_prompt, llm, prefix, request.settings.clone())
            .await
            .wrap_err_with(|| format!("{label}: initial step failed"))?;
        steps += 1;
        loop {
            if accepted.load(Ordering::SeqCst) {
                return Ok(RunReport {
                    end: RunEnd::Submitted,
                    steps,
                    nudges,
                    last_message: step.assistant_message().cloned(),
                });
            }
            if steps >= self.max_steps {
                tracing::warn!("{label}: step budget {} exhausted", self.max_steps);
                return Ok(RunReport {
                    end: RunEnd::Exhausted,
                    steps,
                    nudges,
                    last_message: step.assistant_message().cloned(),
                });
            }
            if let StepResult::Stop(message) = &step {
                if nudges >= self.max_nudges {
                    tracing::warn!("{label}: stopped without submitting after {nudges} nudges");
                    return Ok(RunReport {
                        end: RunEnd::Stopped,
                        steps,
                        nudges,
                        last_message: Some(message.clone()),
                    });
                }
                nudges += 1;
                tracing::info!("{label}: model stopped without submitting; nudge {nudges}");
                step = agent
                    .step_with_user(request.nudge.clone(), llm, prefix, request.settings.clone())
                    .await
                    .wrap_err_with(|| format!("{label}: nudge step failed"))?;
                steps += 1;
                continue;
            }
            if let Some(checkpoint) = &request.checkpoint
                && let Some(due) = next_checkpoint
                && steps >= due
                && !checkpoint.flag.load(Ordering::SeqCst)
            {
                next_checkpoint = Some(due + checkpoint.by_step);
                tracing::info!(
                    "{label}: no progress by step {due}; sending the checkpoint message"
                );
                step = agent
                    .step_with_user(
                        checkpoint.message.clone(),
                        llm,
                        prefix,
                        request.settings.clone(),
                    )
                    .await
                    .wrap_err_with(|| format!("{label}: checkpoint step failed"))?;
                steps += 1;
                continue;
            }
            step = agent
                .step(llm, prefix, request.settings.clone())
                .await
                .wrap_err_with(|| format!("{label}: step {} failed", steps + 1))?;
            steps += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RunBudget;

    #[test]
    fn a_provider_concurrency_cap_is_transient() {
        assert!(RunBudget::is_transient(
            "poc-A1xP1-S01#0: step 3 failed: responses api error: Concurrency limit exceeded for account, please retry later"
        ));
        assert!(!RunBudget::is_transient("the harness does not compile"));
    }
}
