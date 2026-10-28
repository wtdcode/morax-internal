use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::matrix::CellRef;
use crate::task::VerificationTask;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ForgeRunKind {
    Test,
    Coverage,
}

/// One forge invocation, kept short enough to sit in a record file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgeRunDigest {
    pub kind: ForgeRunKind,
    pub forge_args: Vec<String>,
    pub exit_code: i32,
    pub duration_ms: i64,
    pub timed_out: bool,
    /// The test reverted with the violation marker.
    pub violated: bool,
    pub stdout_tail: String,
    pub stderr_tail: String,
}

/// How the dynamic stage ended for one task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    /// A Foundry test reached the violating post-state.
    Demonstrated,
    /// The agent exhausted its budget without a test reaching the post-state; the obligation
    /// stays open with the attempts as evidence.
    NotDemonstrated,
    /// The stage could not run (forge or agent failure).
    Aborted,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub enum Severity {
    High,
    Medium,
    Low,
    Informational,
}

/// What the verdict agent concludes about a demonstrated violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum VerdictClass {
    /// The test drives real project code from an unprivileged caller and reaches a state the
    /// property forbids.
    ValidFinding,
    /// The violation comes from the test itself: scaffolding that carries the violating logic,
    /// an assertion over harness-written state, a path unrelated to the task, or a fabricated
    /// precondition no ordinary call sequence reaches.
    HarnessArtifact,
    /// The project documents the reached state as intended, and it has no exploit consequence.
    ExpectedBehavior,
    /// Reaching the state needs a privileged role or only harms the caller.
    OutOfThreatModel,
    /// The root cause lives in out-of-scope code or in a class the engagement excludes.
    OutOfScope,
}

/// The client-facing write-up the verdict agent produces for a valid finding.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FindingDraft {
    pub title: String,
    pub severity: Severity,
    /// `Contract.function (path:line-line)` of the defect.
    pub location: String,
    /// One line naming the defect as `<function>: <missing or wrong invariant>`; stable across
    /// re-reviews so duplicates cluster.
    pub root_cause: String,
    pub description: String,
    pub impact: String,
    pub recommendation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Verdict {
    pub class: VerdictClass,
    pub rationale: String,
    /// For every class but `ValidFinding`: the project fact that rules the demonstration out,
    /// stated as a rule a reader who never sees the run can apply.
    #[serde(default)]
    pub ruled_out_claim: Option<String>,
    /// Where `ruled_out_claim` can be re-checked (`path:line` of a document, comment, modifier
    /// or test); absent when nothing anchors it.
    #[serde(default)]
    pub ruled_out_evidence: Option<String>,
    #[serde(default)]
    pub finding: Option<FindingDraft>,
    /// The sequence coverage the verdict was given; filled by the runner, not the agent.
    #[serde(default)]
    #[schemars(skip)]
    pub sequence_coverage: Option<SequenceCoverage>,
}

impl Verdict {
    /// What a verdict must carry before it is accepted: a rationale that cites evidence, the
    /// project fact behind a rejection, and a complete finding write-up.
    pub fn structural_errors(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.rationale.trim().len() < 40 {
            errors.push(
                "the rationale must cite the functions and harness lines the verdict rests on"
                    .to_string(),
            );
        }
        if self.class != VerdictClass::ValidFinding
            && self.ruled_out_claim.as_deref().unwrap_or("").trim().len() < 40
        {
            errors.push(format!("{:?} requires `ruled_out_claim`: the project fact that rules the demonstration out, stated as a rule", self.class));
        }
        match (self.class, &self.finding) {
            (VerdictClass::ValidFinding, None) => {
                errors.push("ValidFinding requires the `finding` object".to_string());
            }
            (VerdictClass::ValidFinding, Some(f)) => {
                for (key, value) in [
                    ("title", &f.title),
                    ("location", &f.location),
                    ("root_cause", &f.root_cause),
                    ("description", &f.description),
                    ("impact", &f.impact),
                    ("recommendation", &f.recommendation),
                ] {
                    if value.trim().is_empty() {
                        errors.push(format!("finding.{key} is empty"));
                    }
                }
            }
            _ => {}
        }
        errors
    }
}

/// How much of a unit's sequence the violating test executed, from a `forge coverage` run of
/// it: a step is hit when a covered line falls inside its project function.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SequenceCoverage {
    /// Steps whose function the project defines.
    pub expected: usize,
    pub hit: usize,
    /// `Contract.function` of the defined steps no covered line falls in.
    pub missed: Vec<String>,
    /// `Contract.function` of the steps the project does not define.
    pub unresolved: Vec<String>,
}

impl SequenceCoverage {
    /// `hit / expected`, 1 when no step resolves.
    pub fn fidelity(&self) -> f64 {
        if self.expected == 0 {
            1.0
        } else {
            self.hit as f64 / self.expected as f64
        }
    }

    pub fn render(&self) -> String {
        let mut out = format!(
            "fidelity {:.2}: {} of {} sequence function(s) the project defines executed",
            self.fidelity(),
            self.hit,
            self.expected
        );
        if !self.missed.is_empty() {
            out.push_str(&format!("; never executed: {}", self.missed.join(", ")));
        }
        if !self.unresolved.is_empty() {
            out.push_str(&format!(
                "; not defined by the project: {}",
                self.unresolved.join(", ")
            ));
        }
        out
    }
}

/// A claim the reproduction agent adopted after its runs refuted the task's own claim. The
/// task's claim is then refuted, and the outcome and verdict are about this claim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatedClaim {
    /// Why the task's claim cannot be reached, as a conclusion states it.
    pub refutation: String,
    /// The claim pursued instead: the party harmed, the function, the violating post-state.
    pub claim: String,
}

/// Everything the dynamic stage produced for one task. Written once, atomically, when the
/// task ends, so a resumed run either sees a complete record or none.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationRecord {
    pub task_key: String,
    pub task: VerificationTask,
    pub outcome: TaskOutcome,
    pub outcome_reason: String,
    pub agent_steps: usize,
    pub restarts: usize,
    /// Path of the last harness file relative to the project root.
    pub harness_path: Option<String>,
    pub harness_source: Option<String>,
    pub runs: Vec<ForgeRunDigest>,
    pub verdict: Option<Verdict>,
    /// Set when the reproduction replaced the task's claim; `outcome` and `verdict` are then
    /// about this claim and the task's cells only say where it was found.
    #[serde(default)]
    pub updated_claim: Option<UpdatedClaim>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
}

/// A finding: a valid verdict attached to the task and cells that produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub task_key: String,
    pub cells: Vec<CellRef>,
    pub draft: FindingDraft,
    pub verdict_rationale: String,
    pub harness_path: Option<String>,
    pub evidence_tail: String,
    /// The claim the reproduction updated to, when it did: the finding was found while
    /// verifying `cells` but does not violate them.
    #[serde(default)]
    pub updated_claim: Option<String>,
}

/// Findings that share one root cause, merged for the report.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FindingCluster {
    /// The finding whose write-up represents the cluster.
    pub representative: String,
    /// Every finding id in the cluster, including the representative.
    pub members: Vec<String>,
    /// One line naming the shared root cause.
    pub root_cause: String,
}

impl FindingCluster {
    /// Every finding in exactly one cluster, the representative among its members, a root
    /// cause stated.
    pub fn structural_errors(clusters: &[FindingCluster], finding_ids: &[String]) -> Vec<String> {
        let mut errors = Vec::new();
        let mut seen: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for cluster in clusters {
            if !cluster.members.contains(&cluster.representative) {
                errors.push(format!(
                    "representative {} is not among its cluster's members",
                    cluster.representative
                ));
            }
            if cluster.root_cause.trim().is_empty() {
                errors.push(format!(
                    "cluster of {} has no root_cause",
                    cluster.representative
                ));
            }
            for member in &cluster.members {
                *seen.entry(member.as_str()).or_default() += 1;
                if !finding_ids.contains(member) {
                    errors.push(format!("unknown finding id {member}"));
                }
            }
        }
        for (id, count) in &seen {
            if *count > 1 {
                errors.push(format!("finding {id} appears in {count} clusters"));
            }
        }
        for id in finding_ids {
            if !seen.contains_key(id.as_str()) {
                errors.push(format!("finding {id} appears in no cluster"));
            }
        }
        errors
    }
}
