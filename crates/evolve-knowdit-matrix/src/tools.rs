//! The `submit_*` tools of the matrix stages. Each takes the stage's own typed payload as its
//! arguments, runs the stage's validation, and on success stores the value and raises the
//! shared `accepted` flag; a rejected submission answers with the problems to fix and the
//! choices questioned for the first time.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use evolve_knowdit_types::{
    BoundInput, ConsumedInput, FunctionRef, MatrixDraft, PropertySet, ScenarioSet, SubjectRow,
    TaskCell, VerificationTask,
};
use llmy::agent::LLMYError;
use llmy::agent::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::validate::{DraftValidator, Problems, RowValidator, TaskValidator, UnitsValidator};

/// Stage 1: one level's draft.
#[derive(Debug, Clone)]
pub struct SubmitDraftTool {
    pub validator: DraftValidator,
    pub doubts: DoubtLedger,
    pub accepted: Arc<AtomicBool>,
    pub value: Arc<Mutex<Option<MatrixDraft>>>,
}

impl Tool for SubmitDraftTool {
    type ARGUMENTS = MatrixDraft;
    const NAME: &str = "submit_draft";
    const DESCRIPTION: Option<&str> = Some(
        "Submit this level's draft of the coverage matrix: subjects (rows) with source anchors, candidate properties (columns) as checkable statements, and subject_exclusions for inventory entries no subject models. The user message shows the exact payload shape. The draft is validated; a rejected submission returns the problems to fix, an accepted one ends the run.",
    );

    async fn invoke(&self, draft: MatrixDraft) -> Result<String, LLMYError> {
        if let Some(rejection) = self.doubts.answer(Self::NAME, self.validator.validate(&draft)) {
            return Ok(rejection);
        }
        let summary = draft.describe();
        *self.value.lock().unwrap_or_else(|p| p.into_inner()) = Some(draft);
        self.accepted.store(true, Ordering::SeqCst);
        Ok(format!("accepted: {summary}"))
    }
}

/// Stage 2a: the merged property columns.
#[derive(Debug, Clone)]
pub struct SubmitPropertiesTool {
    pub max_properties: usize,
    pub accepted: Arc<AtomicBool>,
    pub value: Arc<Mutex<Option<PropertySet>>>,
}

impl Tool for SubmitPropertiesTool {
    type ARGUMENTS = PropertySet;
    const NAME: &str = "submit_properties";
    const DESCRIPTION: Option<&str> = Some(
        "Submit the merged property columns of the coverage matrix and the protocol snapshot. The user message shows the exact payload shape. The set is validated; a rejected submission returns the problems to fix, an accepted one ends the run.",
    );

    async fn invoke(&self, set: PropertySet) -> Result<String, LLMYError> {
        let errors = set.structural_errors(self.max_properties);
        if !errors.is_empty() {
            return Ok(Rejection::problems(Self::NAME, &errors, &[]));
        }
        let count = set.properties.len();
        *self.value.lock().unwrap_or_else(|p| p.into_inner()) = Some(set);
        self.accepted.store(true, Ordering::SeqCst);
        Ok(format!("accepted: {count} properties"))
    }
}

/// Stage 2b: every property decided for one subject.
#[derive(Debug, Clone)]
pub struct SubmitRowTool {
    pub validator: RowValidator,
    pub doubts: DoubtLedger,
    pub accepted: Arc<AtomicBool>,
    pub value: Arc<Mutex<Option<SubjectRow>>>,
}

impl Tool for SubmitRowTool {
    type ARGUMENTS = SubjectRow;
    const NAME: &str = "submit_row";
    const DESCRIPTION: Option<&str> = Some(
        "Submit this subject's row of the coverage matrix: for every property either an obligation (the property instantiated for this subject, with the post-state that counts as its violation) or an exclusion with a source-grounded reason. The user message shows the exact payload shape. The row is validated; a rejected submission returns the problems to fix, an accepted one ends the run.",
    );

    async fn invoke(&self, row: SubjectRow) -> Result<String, LLMYError> {
        if let Some(rejection) = self.doubts.answer(Self::NAME, self.validator.validate(&row)) {
            return Ok(rejection);
        }
        let summary = row.describe();
        *self.value.lock().unwrap_or_else(|p| p.into_inner()) = Some(row);
        self.accepted.store(true, Ordering::SeqCst);
        Ok(format!("accepted: {summary}"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskPayload {
    /// Short title naming the function and the obligation.
    pub title: String,
    /// The post-state, over project state, that shows this cell's obligation failing.
    pub violation: String,
    /// The contract of the function, copied from the listing.
    pub contract: String,
    /// The function through which the cell's violation is reached, copied from the listing.
    pub function: String,
    /// `an unprivileged external caller can reach <the violation> through <function>`.
    pub claim: String,
    /// Every value, identity, record and moment the function reads to decide its effect.
    pub consumes: Vec<ConsumedInput>,
    /// The entries of `consumes` a check fixes.
    #[serde(default)]
    pub binds: Vec<BoundInput>,
}

/// Stage 3: the task of one obligation cell, the ledger of the function through which its
/// violation is reached.
#[derive(Debug, Clone)]
pub struct SubmitTaskTool {
    pub validator: TaskValidator,
    pub doubts: DoubtLedger,
    pub accepted: Arc<AtomicBool>,
    pub value: Arc<Mutex<Option<VerificationTask>>>,
    /// Raised on the first call, accepted or not: the agent's progress flag for the step
    /// checkpoint.
    pub attempted: Arc<AtomicBool>,
}

impl Tool for SubmitTaskTool {
    type ARGUMENTS = TaskPayload;
    const NAME: &str = "submit_task";
    const DESCRIPTION: Option<&str> = Some(
        "Submit this cell's task: the function through which its violation is reached, the violation post-state, a claim, and the function's ledger (consumes: every input its execution reads with its kind and decider; binds: the inputs a check fixes, with the check and what it fixes). The user message shows the exact payload shape. The task is validated; a rejected submission returns the problems to fix, an accepted one lists the open inputs the reproductions will move and ends the run.",
    );

    async fn invoke(&self, args: TaskPayload) -> Result<String, LLMYError> {
        self.attempted.store(true, Ordering::SeqCst);
        let cell = self.validator.cell.clone();
        let mut task = VerificationTask {
            id: cell.key(),
            title: args.title,
            cells: vec![TaskCell {
                cell,
                violation: args.violation,
            }],
            function: Some(FunctionRef {
                contract: args.contract,
                function: args.function,
            }),
            claim: args.claim,
            consumes: args.consumes,
            binds: args.binds,
            setup: String::new(),
            sequence: Vec::new(),
            violation_condition: String::new(),
        };
        task.strip_control_characters();
        if let Some(rejection) = self.doubts.answer(Self::NAME, self.validator.validate(&task)) {
            return Ok(rejection);
        }
        let summary = format!("accepted.{}", open_inputs_line(&task));
        *self.value.lock().unwrap_or_else(|p| p.into_inner()) = Some(task);
        self.accepted.store(true, Ordering::SeqCst);
        Ok(summary)
    }
}

/// Stage 4: the scenarios of one task, the reproductions the dynamic stage runs.
#[derive(Debug, Clone)]
pub struct SubmitUnitsTool {
    pub validator: UnitsValidator,
    pub doubts: DoubtLedger,
    pub accepted: Arc<AtomicBool>,
    pub value: Arc<Mutex<Option<ScenarioSet>>>,
    /// Raised on the first call, accepted or not: the agent's progress flag for the step
    /// checkpoint.
    pub attempted: Arc<AtomicBool>,
}

impl Tool for SubmitUnitsTool {
    type ARGUMENTS = ScenarioSet;
    const NAME: &str = "submit_units";
    const DESCRIPTION: Option<&str> = Some(
        "Submit this task's scenarios: each one distinct reproduction test with the open inputs it moves (copied exactly) and how, the calls from the first one the caller sends, and the post-state that shows the cell's promise failing. Every open input is moved by at least one scenario. The user message shows the exact payload shape. The set is validated; a rejected submission returns the problems to fix, an accepted one ends the run.",
    );

    async fn invoke(&self, mut set: ScenarioSet) -> Result<String, LLMYError> {
        self.attempted.store(true, Ordering::SeqCst);
        set.strip_control_characters();
        if let Some(rejection) = self.doubts.answer(Self::NAME, self.validator.validate(&set)) {
            return Ok(rejection);
        }
        let count = set.scenarios.len();
        *self.value.lock().unwrap_or_else(|p| p.into_inner()) = Some(set);
        self.accepted.store(true, Ordering::SeqCst);
        Ok(format!("accepted: {count} scenario(s)"))
    }
}

/// The per-task line an answer carries: the consumed inputs still open.
fn open_inputs_line(task: &VerificationTask) -> String {
    let open = task.open_inputs();
    let function = task
        .function
        .as_ref()
        .map(|f| f.to_string())
        .unwrap_or_else(|| "?".to_string());
    if open.is_empty() {
        format!(
            "\n{} ({function}): {} consumed, all bound; nothing to reproduce",
            task.id,
            task.consumes.len()
        )
    } else {
        format!(
            "\n{} ({function}): {} consumed, {} bound, {} open input(s), which the task's reproductions will move:\n  - {}",
            task.id,
            task.consumes.len(),
            task.binds.len(),
            open.len(),
            open.iter()
                .map(|c| format!("`{}` [{:?}, decided by {}]", c.input, c.kind, c.decided_by))
                .collect::<Vec<_>>()
                .join("\n  - ")
        )
    }
}

pub struct Rejection;

impl Rejection {
    pub fn problems(tool: &str, errors: &[String], doubts: &[String]) -> String {
        let mut out = format!(
            "rejected: {} problem(s). Call {tool} again with the whole payload.",
            errors.len() + doubts.len()
        );
        if !errors.is_empty() {
            out.push_str(&format!("\nFix these:\n- {}", errors.join("\n- ")));
        }
        if !doubts.is_empty() {
            out.push_str(&format!(
                "\nQuestioned choices (each is raised once): change the choice, or keep it and submit again; a submission that keeps a questioned choice is accepted on that point.\n- {}",
                doubts.join("\n- ")
            ));
        }
        out
    }
}

/// The doubts one agent attempt has been answered with. A doubt is raised once: a later
/// submission that keeps the questioned choice is the agent holding to it, and is accepted on
/// that point.
#[derive(Debug, Clone, Default)]
pub struct DoubtLedger {
    pub raised: Arc<Mutex<BTreeSet<String>>>,
}

impl DoubtLedger {
    /// The rejection a submission is answered with: every error and every doubt not raised
    /// before (now recorded as raised); `None` when there is neither, so the submission is
    /// accepted.
    pub fn answer(&self, tool: &str, problems: Problems) -> Option<String> {
        let mut raised = self.raised.lock().unwrap_or_else(|p| p.into_inner());
        let (held, fresh): (Vec<String>, Vec<String>) =
            problems.doubts.into_iter().partition(|d| raised.contains(d));
        if problems.errors.is_empty() && fresh.is_empty() {
            for doubt in &held {
                tracing::info!("{tool}: accepted with a questioned choice kept: {doubt}");
            }
            return None;
        }
        raised.extend(fresh.iter().cloned());
        Some(Rejection::problems(tool, &problems.errors, &fresh))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_doubt_is_raised_once_and_a_submission_keeping_the_choice_is_accepted() {
        let ledger = DoubtLedger::default();
        let doubted = || Problems {
            errors: Vec::new(),
            doubts: vec!["function X.f is not among the functions listed".to_string()],
        };
        let first = ledger.answer("submit_task", doubted()).expect("raised the first time");
        assert!(first.contains("Questioned choices"), "{first}");
        assert!(ledger.answer("submit_task", doubted()).is_none());
        // An error still rejects, and a held doubt is not repeated with it.
        let with_error = ledger
            .answer(
                "submit_task",
                Problems {
                    errors: vec!["`claim` is empty".to_string()],
                    ..doubted()
                },
            )
            .expect("errors reject");
        assert!(!with_error.contains("Questioned choices"), "{with_error}");
    }

    #[test]
    fn task_payload_schema_types_the_ledger_entries() {
        let schema = serde_json::to_value(schemars::schema_for!(TaskPayload)).expect("schema");
        assert_eq!(schema["properties"]["consumes"]["type"], "array");
        assert!(
            schema["required"]
                .as_array()
                .expect("required")
                .iter()
                .any(|v| v == "violation")
        );
        let text = schema.to_string();
        assert!(text.contains("decided_by"), "{text}");
    }
}
