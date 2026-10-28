use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use color_eyre::eyre::{Result, WrapErr};
use evolve_knowdit_types::{ForgeRunDigest, ForgeRunKind, FoundryLayout, UpdatedClaim};
use llmy::agent::LLMYError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use knowdit_forge::{ForgeRunner, RunKind};

use crate::forge::{ForgeTestSummary, OutputExcerpt};

/// Where harness files live inside the project: `<test dir>/evolve_harness/<task key>/`.
#[derive(Debug, Clone)]
pub struct HarnessWorkspace {
    pub root: PathBuf,
}

impl HarnessWorkspace {
    pub fn new(layout: &FoundryLayout) -> Self {
        Self {
            root: layout.root.join(&layout.test_dir).join("evolve_harness"),
        }
    }

    /// Creates the harness root and returns its canonical path; the runner needs absolute
    /// paths for `FOUNDRY_TEST` and for computing the `--match-path` glob.
    pub async fn ensure(&mut self) -> Result<()> {
        tokio::fs::create_dir_all(&self.root)
            .await
            .wrap_err_with(|| format!("failed to create {}", self.root.display()))?;
        self.root = self.root.canonicalize()?;
        Ok(())
    }

    /// A fresh scratch directory under the harness root (`<task key>_a<attempt>` for a PoC
    /// attempt) plus the forge runner scoped to it.
    pub async fn scratch(&self, name: &str, runner: &ForgeRunner) -> Result<TaskScratch> {
        let dir = self.root.join(name);
        tokio::fs::create_dir_all(&dir).await?;
        let dir = dir.canonicalize().unwrap_or(dir);
        let rel = dir
            .strip_prefix(runner.work_dir())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| dir.to_string_lossy().to_string());
        Ok(TaskScratch {
            match_path_glob: format!("{}/*", rel.trim_end_matches('/')),
            runner: runner.clone().with_test_dir(dir.clone()),
            dir,
        })
    }
}

#[derive(Debug, Clone)]
pub struct TaskScratch {
    pub dir: PathBuf,
    pub match_path_glob: String,
    pub runner: ForgeRunner,
}

/// State one PoC attempt accumulates through its tools.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct HarnessAttempt {
    pub harness_filename: Option<String>,
    pub harness_source: String,
    pub runs: Vec<ForgeRunDigest>,
    pub violation_observed: bool,
    pub concluded: Option<String>,
    pub last_summary: Option<String>,
    /// The claim `update_claim` put in place of the task's; a retry inherits it.
    #[serde(default)]
    pub updated_claim: Option<UpdatedClaim>,
}

/// The attempt state shared between the PoC tools and the run loop. `done` is raised when a
/// forge run reaches the violation marker or the agent concludes, which ends the run.
#[derive(Clone, Default)]
pub struct AttemptHandle {
    pub state: Arc<Mutex<HarnessAttempt>>,
    pub done: Arc<AtomicBool>,
    /// Raised by the first forge run; the run loop's progress checkpoint reads it.
    pub ran_forge: Arc<AtomicBool>,
}

impl std::fmt::Debug for AttemptHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AttemptHandle")
    }
}

impl AttemptHandle {
    pub fn record_harness(&self, filename: String, source: String) {
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        g.harness_filename = Some(filename);
        g.harness_source = source;
    }

    pub fn record_run(&self, digest: ForgeRunDigest, summary: String) {
        self.ran_forge.store(true, Ordering::SeqCst);
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if digest.violated {
            g.violation_observed = true;
            self.done.store(true, Ordering::SeqCst);
        }
        g.runs.push(digest);
        g.last_summary = Some(summary);
    }

    pub fn record_conclusion(&self, reason: String) {
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        g.concluded = Some(reason);
        self.done.store(true, Ordering::SeqCst);
    }

    /// Puts `claim` in place of the task's claim unless one was already put; false then.
    pub fn record_claim_update(&self, claim: UpdatedClaim) -> bool {
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if g.updated_claim.is_some() {
            return false;
        }
        g.updated_claim = Some(claim);
        true
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct WriteHarnessFileArgs {
    /// The task's harness file name, as the system prompt gives it; writing again overwrites.
    pub filename: String,
    /// Full Solidity source of the test file.
    pub content: String,
}

#[derive(Debug, Clone)]
#[llmy::agent::tool(
    arguments = WriteHarnessFileArgs,
    invoke = invoke,
    name = "write_harness_file",
    description = "Write (or overwrite) this task's Foundry test file, the one file the system prompt names, inside its harness directory. Call again after every compile error or logic fix, then run_forge.",
)]
pub struct WriteHarnessFileTool {
    pub attempt: AttemptHandle,
    pub dir: PathBuf,
    /// The only file this attempt may write.
    pub filename: String,
}

impl WriteHarnessFileTool {
    async fn invoke(&self, args: WriteHarnessFileArgs) -> Result<String, LLMYError> {
        if args.filename != self.filename {
            return Ok(format!(
                "error: this task writes only `{}`; write the whole test into that file",
                self.filename
            ));
        }
        if args.content.trim().is_empty() {
            return Ok("error: content is empty".to_string());
        }
        let path = self.dir.join(&args.filename);
        if let Err(e) = tokio::fs::write(&path, &args.content).await {
            return Ok(format!("error: failed to write {}: {e}", path.display()));
        }
        let bytes = args.content.len();
        self.attempt.record_harness(args.filename, args.content);
        Ok(format!("ok: wrote {bytes} bytes to {}", path.display()))
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RunForgeArgs {
    /// Test contract name to run, e.g. `Test_OF_SL_001_T01`.
    pub match_contract: String,
    /// Optional test function name filter.
    #[serde(default)]
    pub match_test: Option<String>,
}

#[derive(Debug, Clone)]
#[llmy::agent::tool(
    arguments = RunForgeArgs,
    invoke = invoke,
    name = "run_forge",
    description = "Compile and run the harness with `forge test` (traces on failure). Returns a digest (compile status, setUp status, pass/fail, whether the violation marker was reached) plus the tails of stdout and stderr. The run ends automatically once a test_ function reverts with the violation marker.",
)]
pub struct RunForgeTool {
    pub attempt: AttemptHandle,
    pub runner: ForgeRunner,
    pub match_path_glob: String,
    pub output_tail_bytes: usize,
    /// `-v` repeated, as `forge test` expects it.
    pub verbosity_flag: String,
    /// Revert-reason prefix that marks a demonstrated violation.
    pub violation_marker: String,
}

impl RunForgeTool {
    async fn invoke(&self, args: RunForgeArgs) -> Result<String, LLMYError> {
        let mut argv = vec![
            "test".to_string(),
            self.verbosity_flag.clone(),
            "--match-contract".to_string(),
            args.match_contract.clone(),
            "--match-path".to_string(),
            self.match_path_glob.clone(),
        ];
        if let Some(test) = &args.match_test {
            argv.push("--match-test".to_string());
            argv.push(test.clone());
        }
        let out = match self.runner.run(RunKind::Test, &argv).await {
            Ok(out) => out,
            Err(e) => return Ok(format!("error: forge spawn failed: {e:#}")),
        };
        let summary = ForgeTestSummary::parse(&out.stdout, &out.stderr, &self.violation_marker);
        let digest = ForgeRunDigest {
            kind: ForgeRunKind::Test,
            forge_args: argv,
            exit_code: out.exit_code,
            duration_ms: out.duration_ms,
            timed_out: out.timed_out,
            violated: summary.violated,
            stdout_tail: OutputExcerpt::tail(&out.stdout, 12_000),
            stderr_tail: OutputExcerpt::tail(&out.stderr, 6_000),
        };
        let rendered = summary.render();
        self.attempt.record_run(digest, rendered.clone());
        let mut sections = Vec::new();
        if summary.violated {
            sections.push("[VIOLATION REACHED] the test_ function reverted with the violation marker; the run ends after this turn.".to_string());
        } else if out.timed_out {
            sections.push("[TIMEOUT] forge was killed at the time limit before it finished; the harness compiles or runs for too long (fuzzing, unbounded loops, a via_ir compile that pulls in many files). Narrow what the test imports and does.".to_string());
        } else if out.exit_code == -2 {
            sections.push("[FORGE KILLED] forge died from a signal before finishing, most likely the memory cap on a compile or a run that allocates without bound. Reduce what the harness compiles (fewer imports) or allocates (smaller loops and arrays).".to_string());
        } else if summary.compile_failed {
            sections.push(
                "[COMPILE FAILED] fix the errors below and write the file again.".to_string(),
            );
        } else if summary.setup_failed {
            sections.push("[SETUP FAILED] setUp() reverted; the sequence never ran.".to_string());
        } else if summary.tests_seen == 0 {
            sections.push("[NO TEST RAN] forge found no matching test_ function; check the contract name, the file name suffix `.t.sol`, and that the function name starts with `test_`.".to_string());
        } else if summary.failed == 0 {
            sections.push("[TEST PASSED] the sequence ran and `violated` stayed false: the violating state was not reached this way, or `violated` reads the wrong state. Re-check the claim against the trace; if the claim is false, say so with `conclude`.".to_string());
        } else {
            sections.push("[TEST REVERTED WITHOUT THE MARKER] a step reverted before the assertion; read the trace, fix setup or the sequence.".to_string());
        }
        sections.push(format!(
            "forge exit={}{} duration_ms={}\n{}",
            out.exit_code,
            if out.timed_out { " [TIMEOUT]" } else { "" },
            out.duration_ms,
            rendered
        ));
        if !out.stderr.trim().is_empty() {
            sections.push(format!(
                "--- stderr (head) ---\n{}",
                OutputExcerpt::head(&out.stderr, self.output_tail_bytes)
            ));
        }
        sections.push(format!(
            "--- stdout (tail) ---\n{}",
            OutputExcerpt::tail(&out.stdout, self.output_tail_bytes)
        ));
        Ok(sections.join("\n"))
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ConcludeArgs {
    /// Why the claim cannot be demonstrated: the guard or state that stops every sequence you
    /// tried, with the function and line, and what the traces showed.
    pub reason: String,
}

#[derive(Debug, Clone)]
#[llmy::agent::tool(
    arguments = ConcludeArgs,
    invoke = invoke,
    name = "conclude",
    description = "End the attempt without a demonstration, when you have established from traces and source that the claimed post-state is not reachable by an unprivileged caller. State the evidence. Do not call this merely because a harness is hard to write. When the behaviour that refutes the claim itself breaks a guarantee to some party, call `update_claim` instead.",
)]
pub struct ConcludeTool {
    pub attempt: AttemptHandle,
    /// Refuse the conclusion until the attempt has run forge once.
    pub force_poc: bool,
}

impl ConcludeTool {
    async fn invoke(&self, args: ConcludeArgs) -> Result<String, LLMYError> {
        if args.reason.trim().len() < 40 {
            return Ok(
                "error: give a concrete reason (which check stops the sequence, where)".to_string(),
            );
        }
        if self.force_poc && !self.attempt.ran_forge.load(Ordering::SeqCst) {
            return Ok("rejected: this attempt has not run forge yet. Write the harness with `write_harness_file` and run it with `run_forge` first; conclude only from what a run shows.".to_string());
        }
        self.attempt.record_conclusion(args.reason);
        Ok("recorded: the attempt ends as not demonstrated".to_string())
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct UpdateClaimArgs {
    /// Why the task's claim cannot be reached: the guard or state that stops it, with the
    /// function and line, and what the runs showed.
    pub refutation: String,
    /// The claim the test pursues from now on: the party harmed, the function, and the
    /// post-state breaking a guarantee to that party that your runs showed.
    pub claim: String,
}

#[derive(Debug, Clone)]
#[llmy::agent::tool(
    arguments = UpdateClaimArgs,
    invoke = invoke,
    name = "update_claim",
    description = "Replace the task's claim, once, when your runs show the task's claim is false but the behaviour they show breaks, on its own, a guarantee the protocol owes some party: a legitimate operation that always reverts, an amount, balance or record left wrong. State why the task's claim fails, as `conclude` would, and the new claim. The attempt continues: assert the new claim's violating post-state with the marker and run forge until it fires, or `conclude` if the new claim fails too.",
)]
pub struct UpdateClaimTool {
    pub attempt: AttemptHandle,
    /// Refuse the update until the attempt has run forge once, as `conclude` is refused.
    pub force_poc: bool,
}

impl UpdateClaimTool {
    async fn invoke(&self, args: UpdateClaimArgs) -> Result<String, LLMYError> {
        if args.refutation.trim().len() < 40 || args.claim.trim().len() < 40 {
            return Ok("error: give a concrete refutation (which check stops the task's claim, where) and a concrete claim (who is harmed, through which function, what post-state)".to_string());
        }
        if self.force_poc && !self.attempt.ran_forge.load(Ordering::SeqCst) {
            return Ok("rejected: this attempt has not run forge yet. Write the harness with `write_harness_file` and run it with `run_forge` first; update the claim only from what a run shows.".to_string());
        }
        if !self.attempt.record_claim_update(UpdatedClaim {
            refutation: args.refutation,
            claim: args.claim,
        }) {
            return Ok("rejected: the claim was already updated once; pursue the updated claim until the marker fires, or `conclude`.".to_string());
        }
        Ok("recorded: the test now pursues this claim, and the demonstration is judged on it instead of the task's. Rewrite `violated` as this claim's violating post-state (the marker's message names the broken guarantee), run forge and iterate until the marker fires; `conclude` if this claim fails too.".to_string())
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ListTestFilesArgs {
    /// Optional substring filter on the relative path.
    #[serde(default)]
    pub filter: Option<String>,
}

#[derive(Debug, Clone)]
#[llmy::agent::tool(
    arguments = ListTestFilesArgs,
    invoke = invoke,
    name = "list_test_files",
    description = "List the project's existing test and script files (Foundry .t.sol, deploy scripts) so you can imitate their setUp() and deployment patterns before writing the harness.",
)]
pub struct ListTestFilesTool {
    pub repo_root: PathBuf,
    pub max_depth: usize,
}

impl ListTestFilesTool {
    async fn invoke(&self, args: ListTestFilesArgs) -> Result<String, LLMYError> {
        let mut found = Vec::new();
        for sub in [
            "test",
            "tests",
            "script",
            "scripts",
            "deploy",
            "deployments",
        ] {
            let dir = self.repo_root.join(sub);
            if dir.is_dir() {
                Self::walk(&dir, self.max_depth, &mut found);
            }
        }
        found.sort();
        let filter = args.filter.unwrap_or_default();
        let listed: Vec<String> = found
            .iter()
            .filter_map(|p| p.strip_prefix(&self.repo_root).ok())
            .map(|p| p.to_string_lossy().to_string())
            .filter(|p| filter.is_empty() || p.contains(&filter))
            .filter(|p| !p.contains("evolve_harness"))
            .take(400)
            .collect();
        if listed.is_empty() {
            return Ok("no test or script files found".to_string());
        }
        Ok(format!(
            "{} file(s)\n- {}",
            listed.len(),
            listed.join("\n- ")
        ))
    }

    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if depth > 0
                    && !matches!(
                        name.as_str(),
                        "node_modules" | "lib" | "out" | "cache" | ".git"
                    )
                {
                    Self::walk(&path, depth - 1, out);
                }
            } else if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("sol") | Some("ts") | Some("js") | Some("json")
            ) {
                out.push(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn force_poc_refuses_a_conclusion_before_the_first_forge_run() {
        let attempt = AttemptHandle::default();
        let tool = ConcludeTool {
            attempt: attempt.clone(),
            force_poc: true,
        };
        let reason =
            "the onlyOwner modifier on Vault.sweep (src/Vault.sol:42) stops every other caller"
                .to_string();
        let answer = tool
            .invoke(ConcludeArgs {
                reason: reason.clone(),
            })
            .await
            .expect("invoke");
        assert!(answer.starts_with("rejected"), "{answer}");
        assert!(!attempt.done.load(Ordering::SeqCst));
        attempt.ran_forge.store(true, Ordering::SeqCst);
        let answer = tool.invoke(ConcludeArgs { reason }).await.expect("invoke");
        assert!(answer.starts_with("recorded"), "{answer}");
        assert!(attempt.done.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn the_claim_is_updated_once_after_a_forge_run_and_the_attempt_continues() {
        let attempt = AttemptHandle::default();
        let tool = UpdateClaimTool {
            attempt: attempt.clone(),
            force_poc: true,
        };
        let args = || UpdateClaimArgs {
            refutation: "Vault.withdraw (src/Vault.sol:88) burns the owner's shares only up to the caller's allowance".to_string(),
            claim: "a holder whose shares are all burned by withdraw can never call redeem again: it always reverts".to_string(),
        };
        let answer = tool.invoke(args()).await.expect("invoke");
        assert!(answer.starts_with("rejected"), "{answer}");
        attempt.ran_forge.store(true, Ordering::SeqCst);
        let answer = tool.invoke(args()).await.expect("invoke");
        assert!(answer.starts_with("recorded"), "{answer}");
        assert!(!attempt.done.load(Ordering::SeqCst));
        let answer = tool.invoke(args()).await.expect("invoke");
        assert!(answer.contains("already updated"), "{answer}");
        let state = attempt.state.lock().expect("lock").clone();
        assert!(state.updated_claim.expect("updated").claim.contains("redeem"));
    }
}
