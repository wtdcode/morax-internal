use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use clap::Args;
use evolve_knowdit_project::CodeIndex;
use evolve_knowdit_types::{CallStep, SequenceCoverage};

/// Forge runtime knobs, flattened into the CLI. The runtime itself (host binary, host binary
/// inside a memory-capped cgroup, or docker) is the `knowdit-forge` crate vendored from knowdit (`crates/knowdit-forge`).
#[derive(Args, Clone, Debug)]
pub struct ForgeArgs {
    /// Revert-reason prefix a harness uses on its violation assertion; a test that reverts with
    /// it counts as a demonstrated violation.
    #[arg(long = "violation-marker", default_value = "EVOLVE_VIOLATION_REACHED:")]
    pub violation_marker: String,
    /// Path to a local `forge` binary. Unset: docker when present, else `forge` on PATH.
    #[arg(long = "forge-bin", env = "EVOLVE_FORGE_BIN")]
    pub forge_bin: Option<PathBuf>,
    /// Docker image used when no local forge is given and docker is available.
    #[arg(long = "forge-docker-image", default_value = "lazymio/knowdit:foundry")]
    pub docker_image: String,
    /// Memory ceiling per forge invocation in GiB (cgroup v2 for a local binary, --memory for
    /// docker); 0 disables the cap.
    #[arg(long = "forge-mem-cap-gb", default_value_t = 12)]
    pub mem_cap_gb: u64,
    /// Timeout of one `forge test` invocation in seconds.
    #[arg(long = "forge-timeout-secs", default_value_t = 600)]
    pub timeout_secs: u64,
    /// Trace verbosity passed to `forge test` (`-v` repeated this many times).
    #[arg(long = "forge-verbosity", default_value_t = 3)]
    pub verbosity: usize,
}

/// Digest of a text-mode `forge test` run.
#[derive(Debug, Clone, Default)]
pub struct ForgeTestSummary {
    pub compile_failed: bool,
    pub setup_failed: bool,
    pub tests_seen: usize,
    pub passed: usize,
    pub failed: usize,
    /// A `test_…` function reverted with the violation marker.
    pub violated: bool,
    pub violation_reason: Option<String>,
    pub failure_reasons: Vec<String>,
}

impl ForgeTestSummary {
    pub fn parse(stdout: &str, stderr: &str, violation_marker: &str) -> Self {
        let mut s = Self::default();
        let compile_markers = [
            "Compiler run failed",
            "Error: Compiler run failed",
            "Failed to compile",
            "Error (",
            "error[E",
        ];
        if compile_markers
            .iter()
            .any(|m| stderr.contains(m) || stdout.contains(m))
        {
            s.compile_failed = true;
        }
        // Forge prints every failing test twice (suite run + closing summary); count each
        // test name once.
        let mut seen_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for line in stdout.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("[PASS]") {
                let name = rest.trim().split('(').next().unwrap_or("").to_string();
                if !seen_names.insert(format!("pass:{name}")) {
                    continue;
                }
                s.tests_seen += 1;
                if name.starts_with("test") {
                    s.passed += 1;
                }
            } else if let Some(rest) = trimmed.strip_prefix("[FAIL") {
                let (reason, name) = match rest.rsplit_once(']') {
                    Some((reason, name)) => (
                        reason.trim_start_matches(':').trim().to_string(),
                        name.trim().to_string(),
                    ),
                    None => (rest.to_string(), String::new()),
                };
                let bare = name.split('(').next().unwrap_or("").to_string();
                if !seen_names.insert(format!("fail:{bare}")) {
                    continue;
                }
                s.tests_seen += 1;
                if name.starts_with("setUp") {
                    s.setup_failed = true;
                    s.failure_reasons.push(format!("setUp: {reason}"));
                    continue;
                }
                if name.starts_with("test") {
                    s.failed += 1;
                    if reason.contains(violation_marker) {
                        s.violated = true;
                        let idx = reason.find(violation_marker).unwrap_or(0);
                        s.violation_reason = Some(reason[idx..].to_string());
                    } else {
                        s.failure_reasons.push(format!("{name}: {reason}"));
                    }
                }
            } else if trimmed.contains("failed to set up") {
                s.setup_failed = true;
                s.failure_reasons.push(trimmed.chars().take(300).collect());
            }
        }
        s
    }

    pub fn render(&self) -> String {
        let mut parts = vec![format!(
            "compile_failed={} setup_failed={} tests={} passed={} failed={} violated={}",
            self.compile_failed,
            self.setup_failed,
            self.tests_seen,
            self.passed,
            self.failed,
            self.violated
        )];
        if let Some(reason) = &self.violation_reason {
            parts.push(format!("violation: {reason}"));
        }
        for reason in self.failure_reasons.iter().take(5) {
            parts.push(format!("failure: {reason}"));
        }
        parts.join("\n")
    }
}

/// The executed lines of a `forge coverage --report lcov` run, by source file as lcov names it
/// (relative to the Foundry root).
#[derive(Debug, Clone, Default)]
pub struct LcovReport {
    executed: BTreeMap<String, BTreeSet<usize>>,
}

impl LcovReport {
    /// Reads every `DA:<line>,<hits>` with a positive hit count under its `SF:` block.
    pub fn parse(text: &str) -> Self {
        let mut report = Self::default();
        let mut file: Option<String> = None;
        for line in text.lines() {
            let line = line.trim_end();
            if let Some(path) = line.strip_prefix("SF:") {
                file = Some(path.trim().to_string());
            } else if line == "end_of_record" {
                file = None;
            } else if let (Some(file), Some(rest)) = (&file, line.strip_prefix("DA:")) {
                let mut parts = rest.split(',');
                if let (Some(Ok(number)), Some(Ok(hits))) = (
                    parts.next().map(|p| p.trim().parse::<usize>()),
                    parts.next().map(|p| p.trim().parse::<i64>()),
                ) && hits > 0
                {
                    report
                        .executed
                        .entry(file.clone())
                        .or_default()
                        .insert(number);
                }
            }
        }
        report
    }

    pub fn is_empty(&self) -> bool {
        self.executed.is_empty()
    }

    /// Each sequence step resolved to its project function and counted as hit when an
    /// executed line falls inside one of its spans.
    pub fn sequence_coverage(&self, sequence: &[CallStep], index: &CodeIndex) -> SequenceCoverage {
        let mut coverage = SequenceCoverage::default();
        for step in sequence {
            let label = format!("{}.{}", step.contract, step.function);
            let spans = index.function_spans(&step.contract, &step.function);
            if spans.is_empty() {
                coverage.unresolved.push(label);
                continue;
            }
            coverage.expected += 1;
            let hit = spans.iter().any(|(file, span)| {
                self.executed
                    .get(file)
                    .is_some_and(|lines| lines.range(span.start_line..=span.end_line).next().is_some())
            });
            if hit {
                coverage.hit += 1;
            } else {
                coverage.missed.push(label);
            }
        }
        coverage
    }
}

/// Bounded excerpts of forge output for tool results and records.
pub struct OutputExcerpt;

impl OutputExcerpt {
    /// Last `n` bytes of `s` on a char boundary.
    pub fn tail(s: &str, n: usize) -> String {
        if s.len() <= n {
            return s.to_string();
        }
        let mut start = s.len() - n;
        while start < s.len() && !s.is_char_boundary(start) {
            start += 1;
        }
        format!("…[{} bytes earlier]\n{}", start, &s[start..])
    }

    /// First `n` bytes of `s` on a char boundary.
    pub fn head(s: &str, n: usize) -> String {
        if s.len() <= n {
            return s.to_string();
        }
        let mut end = n;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…[{} bytes truncated]", &s[..end], s.len() - end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORGE_OUTPUT: &str = "Compiling 1 files with Solc 0.8.28\nRan 2 tests for test/x.t.sol:Test_PROBE\n[FAIL: some other reason] test_plain_revert() (gas: 280)\n[FAIL: EVOLVE_VIOLATION_REACHED: share supply exceeds backing] test_violation() (gas: 7755)\nSuite result: FAILED. 0 passed; 2 failed; 0 skipped\nFailing tests:\n[FAIL: some other reason] test_plain_revert() (gas: 280)\n[FAIL: EVOLVE_VIOLATION_REACHED: share supply exceeds backing] test_violation() (gas: 7755)\n";

    #[test]
    fn marker_revert_counts_as_violation_and_tests_are_deduplicated() {
        let s = ForgeTestSummary::parse(FORGE_OUTPUT, "", "EVOLVE_VIOLATION_REACHED:");
        assert!(s.violated);
        assert_eq!(s.failed, 2);
        assert_eq!(s.tests_seen, 2);
        assert!(!s.compile_failed && !s.setup_failed);
        assert!(
            s.violation_reason
                .as_deref()
                .unwrap_or("")
                .starts_with("EVOLVE_VIOLATION_REACHED:")
        );
    }

    #[test]
    fn setup_failures_and_compile_errors_are_recognised() {
        let s = ForgeTestSummary::parse(
            "[FAIL: call to non-contract] setUp() (gas: 0)\n",
            "",
            "EVOLVE_VIOLATION_REACHED:",
        );
        assert!(s.setup_failed && !s.violated);
        let s = ForgeTestSummary::parse(
            "",
            "Error: Compiler run failed:\nError (6275): Source not found",
            "EVOLVE_VIOLATION_REACHED:",
        );
        assert!(s.compile_failed);
    }

    #[tokio::test]
    async fn sequence_coverage_counts_a_step_hit_when_an_executed_line_falls_in_its_function() {
        let root = std::env::temp_dir()
            .join("evolve-knowdit-verify-tests")
            .join(format!("lcov-{}", std::process::id()));
        tokio::fs::create_dir_all(root.join("src")).await.expect("fixture dir");
        tokio::fs::write(
            root.join("src/Vault.sol"),
            "pragma solidity ^0.8.0;\ncontract Vault {\n    uint256 total;\n    function deposit(uint256 a) external {\n        total += a;\n    }\n    function withdraw(uint256 a) external {\n        total -= a;\n    }\n}\n",
        )
        .await
        .expect("fixture file");
        let layout = evolve_knowdit_types::FoundryLayout {
            root: root.clone(),
            src_dir: "src".to_string(),
            test_dir: "test".to_string(),
            via_ir: false,
            solc: None,
            remappings: Vec::new(),
            lib_dirs: Vec::new(),
        };
        let index = CodeIndex::build(&layout, &Default::default(), None, &[])
            .await
            .expect("index");
        let lcov = LcovReport::parse(
            "TN:\nSF:src/Vault.sol\nDA:5,2\nDA:8,0\nend_of_record\nSF:test/X.t.sol\nDA:8,1\nend_of_record\n",
        );
        let step = |contract: &str, function: &str| CallStep {
            caller: "unprivileged caller".to_string(),
            contract: contract.to_string(),
            function: function.to_string(),
            intention: String::new(),
        };
        let coverage = lcov.sequence_coverage(
            &[step("Vault", "deposit"), step("Vault", "withdraw"), step("Helper", "poke")],
            &index,
        );
        assert_eq!((coverage.hit, coverage.expected), (1, 2));
        assert_eq!(coverage.missed, vec!["Vault.withdraw".to_string()]);
        assert_eq!(coverage.unresolved, vec!["Helper.poke".to_string()]);
        assert!(coverage.render().starts_with("fidelity 0.50"), "{}", coverage.render());
    }

    #[test]
    fn passing_tests_are_not_violations() {
        let s = ForgeTestSummary::parse(
            "[PASS] test_violation() (gas: 64983)\n",
            "",
            "EVOLVE_VIOLATION_REACHED:",
        );
        assert_eq!(s.passed, 1);
        assert!(!s.violated);
    }
}
