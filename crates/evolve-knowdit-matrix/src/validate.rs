use std::collections::BTreeSet;
use std::sync::Arc;

use evolve_knowdit_project::{CodeIndex, ExtentFunction};
use evolve_knowdit_types::{
    CellRef, Dimension, MatrixDraft, Property, ScenarioSet, Subject, SubjectRow,
    VerificationTask,
};

/// What a validator found in one submission. `errors` must be fixed before the submission is
/// accepted. `doubts` question a choice the agent may hold to (a function outside the listing
/// it was given, a grouping the level definition discourages): each is answered once, and a
/// later submission that keeps the questioned choice is accepted on that point.
#[derive(Debug, Clone, Default)]
pub struct Problems {
    pub errors: Vec<String>,
    pub doubts: Vec<String>,
}

/// Checks a submitted level draft against the structure rules and the code index.
#[derive(Debug, Clone)]
pub struct DraftValidator {
    pub index: Arc<CodeIndex>,
    pub dimension: Dimension,
    pub max_properties: usize,
}

impl DraftValidator {
    pub fn validate(&self, draft: &MatrixDraft) -> Problems {
        let mut problems = Problems::default();
        let errors = &mut problems.errors;
        if draft.dimension != self.dimension {
            errors.push(format!(
                "dimension must be `{}`, found `{}`",
                self.dimension, draft.dimension
            ));
        }
        errors.extend(draft.structural_errors(self.max_properties));
        for subject in &draft.subjects {
            let mut anchored = true;
            for anchor in &subject.anchors {
                if let Some(err) = self.index.anchor_error(anchor, true) {
                    errors.push(format!("subject {}: {err}", subject.id));
                    anchored = false;
                }
            }
            if !anchored {
                continue;
            }
            if self.index.extent_functions(&subject.anchors).is_empty() {
                problems.doubts.push(format!(
                    "subject {}: no project function belongs to it through its anchors (none declares, inherits, writes or reads what they name), so its row could anchor no obligation in its own code. Anchor the functions that create, move or settle it, or keep the subject as it is if it truly has none",
                    subject.id
                ));
            }
            if self.dimension == Dimension::OperationFlow {
                let groups = self.index.settlement_groups(&subject.anchors);
                if groups.len() > 1 {
                    problems.doubts.push(format!(
                        "subject {} groups entry points with different settlement paths ({}): an operation subject is one entry point with its overloads and the thin wrappers that call it. Make each group its own subject, or keep the grouping if these entry points do share one settlement path",
                        subject.id,
                        groups.iter().map(|g| g.join(", ")).collect::<Vec<_>>().join(" | ")
                    ));
                }
            }
        }
        for exclusion in &draft.subject_exclusions {
            if let Some(err) = self.index.anchor_error(&exclusion.anchor, false) {
                errors.push(format!("subject exclusion: {err}"));
            }
        }
        let gaps = self.index.completeness_gaps(
            self.dimension,
            &draft.subjects,
            &draft.subject_exclusions,
        );
        if !gaps.is_empty() {
            let shown: Vec<String> = gaps.iter().take(40).map(|g| g.to_string()).collect();
            errors.push(format!(
                "{} inventory entries are neither anchored by a subject nor listed under subject_exclusions with a reason: {}{}",
                gaps.len(),
                shown.join("; "),
                if gaps.len() > shown.len() { "; ..." } else { "" }
            ));
        }
        problems
    }
}

/// Checks a submitted subject row: the structure rules plus the anchors of every obligation,
/// which must name code the index resolves, at least one function each. An anchor outside the
/// subject's listed functions is kept only with the obligation's `reach`, the agent's argument
/// for how it reaches the subject.
#[derive(Debug, Clone)]
pub struct RowValidator {
    pub index: Arc<CodeIndex>,
    pub subject: Subject,
    pub properties: Vec<Property>,
}

impl RowValidator {
    pub fn validate(&self, row: &SubjectRow) -> Problems {
        let mut errors = row.structural_errors(&self.subject.id, &self.properties);
        let subject_functions: BTreeSet<(String, String)> = self
            .index
            .extent_functions(&self.subject.anchors)
            .into_iter()
            .map(|f| (f.contract, f.function))
            .collect();
        for o in &row.obligations {
            if o.anchors.is_empty() {
                errors.push(format!(
                    "obligation {}: list its `anchors`: the functions of this subject through which an unprivileged sequence could reach the violation post-state (file, contract, function) and the state it reads",
                    o.property_id
                ));
                continue;
            }
            let mut named_functions = 0;
            let mut outside = Vec::new();
            for anchor in &o.anchors {
                if let Some(err) = self.index.anchor_error(anchor, false) {
                    errors.push(format!("obligation {} anchor: {err}", o.property_id));
                    continue;
                }
                if anchor.function.is_some() {
                    named_functions += 1;
                }
                let bound = self.index.anchored_functions(std::slice::from_ref(anchor));
                if anchor.function.is_some() && bound.is_empty() {
                    errors.push(format!(
                        "obligation {} anchor {anchor}: that contract neither declares nor inherits the function",
                        o.property_id
                    ));
                    continue;
                }
                if !bound
                    .iter()
                    .any(|f| subject_functions.contains(&(f.contract.clone(), f.function.clone())))
                {
                    outside.push(anchor.to_string());
                }
            }
            if !outside.is_empty() && o.reach.trim().is_empty() {
                errors.push(format!(
                    "obligation {}: {} {} not among this subject's listed functions. If the violation is reached through {}, keep {} and state in `reach` the call path from each into this subject's code or state; otherwise anchor listed functions",
                    o.property_id,
                    outside.join(", "),
                    if outside.len() == 1 { "is" } else { "are" },
                    if outside.len() == 1 { "it" } else { "them" },
                    if outside.len() == 1 { "it" } else { "them" },
                ));
            }
            if named_functions == 0 {
                errors.push(format!(
                    "obligation {}: at least one anchor must name a function (the entry point or writer through which the violation post-state is reached)",
                    o.property_id
                ));
            }
        }
        Problems {
            errors,
            doubts: Vec::new(),
        }
    }
}

/// Checks the task one obligation cell's agent submits: its witness, its function (a project
/// function; one outside the listing handed to the agent is questioned once), and its ledger
/// (every consumed entry with kind and decider; every bind an entry of `consumes` with the
/// check and what it fixes).
#[derive(Debug, Clone)]
pub struct TaskValidator {
    pub cell: CellRef,
    /// The functions listed to the agent: those the subject's and the obligation's anchors
    /// reach ([`CodeIndex::extent_functions`]).
    pub functions: Vec<ExtentFunction>,
    /// Every `(contract, function)` the project exposes, declared or inherited.
    pub exposed: BTreeSet<(String, String)>,
}

impl TaskValidator {
    /// Whether the listing handed to the agent has this function.
    pub fn lists(&self, contract: &str, function: &str) -> bool {
        self.functions
            .iter()
            .any(|f| f.contract == contract && f.function == function)
    }

    pub fn validate(&self, task: &VerificationTask) -> Problems {
        let mut errors = Vec::new();
        let mut doubts = Vec::new();
        match task.cells.as_slice() {
            [only] if only.cell == self.cell => {
                if only.violation.trim().is_empty() {
                    errors.push("`violation` is empty: the post-state, over project state, that shows this obligation failing".to_string());
                }
            }
            _ => errors.push(format!("the task discharges exactly cell {}", self.cell)),
        }
        for (name, value) in [("claim", &task.claim), ("title", &task.title)] {
            if value.trim().is_empty() {
                errors.push(format!("`{name}` is empty"));
            }
        }
        let Some(function) = &task.function else {
            errors.push("no `function`: the task is the ledger of the function through which the cell's violation is reached (copy `contract` and `function` from the listing)".to_string());
            return Problems { errors, doubts };
        };
        if !self.lists(&function.contract, &function.function) {
            if !self
                .exposed
                .contains(&(function.contract.clone(), function.function.clone()))
            {
                errors.push(format!(
                    "function {function} is not a function of the project: `{}` neither declares nor inherits `{}`; copy `contract` and `function` from the listing or the code",
                    function.contract, function.function
                ));
                return Problems { errors, doubts };
            }
            doubts.push(format!(
                "function {function} is not among the functions listed for this cell. If the violation goes through it, keep it and say in `claim` how it reaches the obligation; otherwise name a listed function"
            ));
        }
        if task.consumes.iter().all(|c| c.input.trim().is_empty()) {
            errors.push(format!(
                "`consumes` is empty: list every value, identity, record and moment {function} reads to decide its effect, one entry each with its kind and who decides it"
            ));
        }
        let mut consumes: BTreeSet<&str> = BTreeSet::new();
        for c in &task.consumes {
            if c.input.trim().is_empty() {
                errors.push("a `consumes` entry has no `input`".to_string());
                continue;
            }
            if c.decided_by.trim().len() < 3 {
                errors.push(format!(
                    "`consumes` entry `{}` has no `decided_by`: who decides its value as the function runs (the caller, the signer, the owner, the callee <contract>, storage written by <entry point>, the block, a constant)",
                    c.input
                ));
            }
            if !consumes.insert(c.input.trim()) {
                errors.push(format!("`consumes` lists `{}` twice; keep one entry", c.input));
            }
        }
        for b in &task.binds {
            if !consumes.contains(b.input.trim()) {
                errors.push(format!(
                    "`binds` entry `{}` is not an entry of `consumes`; every bound input is one consumed entry, copied exactly",
                    b.input
                ));
                continue;
            }
            if b.check.trim().is_empty() || b.fixes.trim().is_empty() {
                errors.push(format!(
                    "`binds` entry `{}` needs `check` (what it requires and where: Contract.function, file:line) and `fixes` (what the value is fixed to)",
                    b.input
                ));
            }
        }
        Problems { errors, doubts }
    }
}

/// Checks the scenarios one task's unit agent submits: at most `max_scenarios`; every move an
/// open input of the task, copied exactly; every call of a sequence a project function (one
/// outside the listing handed to the agent is questioned once); every open input moved by at
/// least one scenario.
#[derive(Debug, Clone)]
pub struct UnitsValidator {
    pub task: VerificationTask,
    /// The functions listed to the agent: the task agent's listing plus the task's function.
    pub functions: Vec<ExtentFunction>,
    /// Every `(contract, function)` the project exposes, declared or inherited.
    pub exposed: BTreeSet<(String, String)>,
    pub max_scenarios: usize,
}

impl UnitsValidator {
    pub fn validate(&self, set: &ScenarioSet) -> Problems {
        let mut errors = Vec::new();
        let mut doubts = Vec::new();
        let open: BTreeSet<&str> = self.task.open_inputs().iter().map(|c| c.input.trim()).collect();
        if set.scenarios.is_empty() {
            errors.push("`scenarios` is empty: write the reproductions of this task".to_string());
        }
        if set.scenarios.len() > self.max_scenarios {
            errors.push(format!(
                "{} scenarios, at most {}: two scenarios that build the same test are one",
                set.scenarios.len(),
                self.max_scenarios
            ));
        }
        let mut moved: BTreeSet<&str> = BTreeSet::new();
        for (i, scenario) in set.scenarios.iter().enumerate() {
            let n = i + 1;
            if scenario.title.trim().is_empty() {
                errors.push(format!("scenario {n} has no `title`"));
            }
            if scenario.observe.trim().is_empty() {
                errors.push(format!("scenario {n} has no `observe`: the post-state that shows the cell's promise failing"));
            }
            if scenario.moves.is_empty() {
                errors.push(format!("scenario {n} moves nothing: name the open inputs it moves"));
            }
            let mut own: BTreeSet<&str> = BTreeSet::new();
            for m in &scenario.moves {
                let input = m.input.trim();
                if !open.contains(input) {
                    errors.push(format!(
                        "scenario {n} moves `{}`, which is not an open input of the task; copy an entry of the open inputs exactly",
                        m.input
                    ));
                    continue;
                }
                if !own.insert(input) {
                    errors.push(format!("scenario {n} moves `{}` twice; keep one entry", m.input));
                }
                if m.how.trim().is_empty() {
                    errors.push(format!("scenario {n}: `how` of `{}` is empty", m.input));
                }
                moved.insert(input);
            }
            if scenario.sequence.is_empty() {
                errors.push(format!("scenario {n} has no `sequence`: the calls that reach the post-state"));
            }
            for step in &scenario.sequence {
                if self
                    .functions
                    .iter()
                    .any(|f| f.contract == step.contract && f.function == step.function)
                {
                    continue;
                }
                if self
                    .exposed
                    .contains(&(step.contract.clone(), step.function.clone()))
                {
                    doubts.push(format!(
                        "scenario {n} calls {}.{}, which is not among the functions listed for this task. If the sequence needs it, keep it with the step's `intention` saying why; otherwise call a listed function",
                        step.contract, step.function
                    ));
                } else {
                    errors.push(format!(
                        "scenario {n} calls {}.{}, which is not a function of the project; copy `contract` and `function` from the listing or the code",
                        step.contract, step.function
                    ));
                }
            }
        }
        let unmoved: Vec<&str> = open.difference(&moved).copied().collect();
        if !unmoved.is_empty() {
            errors.push(format!(
                "{} open input(s) no scenario moves: {}; every open input is moved by at least one scenario",
                unmoved.len(),
                unmoved.iter().map(|i| format!("`{i}`")).collect::<Vec<_>>().join(", ")
            ));
        }
        Problems { errors, doubts }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use evolve_knowdit_project::ExtentRelation;
    use evolve_knowdit_types::{
        BoundInput, CallStep, ConsumedInput, FunctionRef, InputKind, Scenario, ScenarioMove,
        TaskCell,
    };

    fn validator() -> UnitsValidator {
        let consumed = |input: &str| ConsumedInput {
            input: input.to_string(),
            kind: InputKind::Parameter,
            decided_by: "the caller".to_string(),
        };
        UnitsValidator {
            task: VerificationTask {
                id: "A1xP1".to_string(),
                title: "t".to_string(),
                cells: vec![TaskCell {
                    cell: CellRef {
                        subject_id: "A1".to_string(),
                        property_id: "P1".to_string(),
                    },
                    violation: "v".to_string(),
                }],
                function: Some(FunctionRef {
                    contract: "Vault".to_string(),
                    function: "withdraw".to_string(),
                }),
                claim: "c".to_string(),
                consumes: vec![consumed("shares"), consumed("receiver"), consumed("owner")],
                binds: vec![BoundInput {
                    input: "shares".to_string(),
                    check: "allowance".to_string(),
                    fixes: "the caller's own allowance".to_string(),
                }],
                setup: String::new(),
                sequence: Vec::new(),
                violation_condition: String::new(),
            },
            functions: vec![ExtentFunction {
                contract: "Vault".to_string(),
                function: "withdraw".to_string(),
                signature: "function withdraw(uint256,address,address)".to_string(),
                line: 1,
                read_only: false,
                relation: ExtentRelation::Declared,
            }],
            exposed: [("Vault", "withdraw"), ("Vault", "sweep")]
                .iter()
                .map(|(c, f)| (c.to_string(), f.to_string()))
                .collect(),
            max_scenarios: 2,
        }
    }

    fn scenario(inputs: &[&str], function: &str) -> Scenario {
        Scenario {
            title: "s".to_string(),
            moves: inputs
                .iter()
                .map(|i| ScenarioMove {
                    input: i.to_string(),
                    how: "moved".to_string(),
                })
                .collect(),
            sequence: vec![CallStep {
                caller: "unprivileged caller".to_string(),
                contract: "Vault".to_string(),
                function: function.to_string(),
                intention: "i".to_string(),
            }],
            observe: "o".to_string(),
        }
    }

    #[test]
    fn scenarios_must_move_every_open_input_and_only_open_inputs_through_project_calls() {
        let v = validator();
        let ok = ScenarioSet {
            scenarios: vec![scenario(&["receiver", "owner"], "withdraw")],
        };
        let found = v.validate(&ok);
        assert!(found.errors.is_empty() && found.doubts.is_empty(), "{found:?}");
        let unmoved = ScenarioSet {
            scenarios: vec![scenario(&["receiver"], "withdraw")],
        };
        assert!(v.validate(&unmoved).errors.iter().any(|e| e.contains("`owner`")));
        let bound = ScenarioSet {
            scenarios: vec![scenario(&["receiver", "owner", "shares"], "withdraw")],
        };
        assert!(v.validate(&bound).errors.iter().any(|e| e.contains("not an open input")));
        // A project function outside the listing is questioned, not refused.
        let unlisted = v.validate(&ScenarioSet {
            scenarios: vec![scenario(&["receiver", "owner"], "sweep")],
        });
        assert!(unlisted.errors.is_empty(), "{unlisted:?}");
        assert!(unlisted.doubts.iter().any(|d| d.contains("not among the functions listed")));
        let unknown = v.validate(&ScenarioSet {
            scenarios: vec![scenario(&["receiver", "owner"], "nosuch")],
        });
        assert!(unknown.errors.iter().any(|e| e.contains("not a function of the project")));
        let many = ScenarioSet {
            scenarios: vec![
                scenario(&["receiver"], "withdraw"),
                scenario(&["owner"], "withdraw"),
                scenario(&["owner"], "withdraw"),
            ],
        };
        assert!(v.validate(&many).errors.iter().any(|e| e.contains("at most 2")));
    }
}
