use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::matrix::CellRef;

/// One call of the sequence a task's violation requires.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CallStep {
    /// Who sends the call: `unprivileged caller`, `second unprivileged account`,
    /// `a helper contract the caller deploys`, or a project contract reached internally.
    pub caller: String,
    /// Project contract name that receives the call.
    pub contract: String,
    /// Function name on that contract.
    pub function: String,
    /// What the step establishes toward the violation.
    pub intention: String,
}

/// One obligation a task discharges, with the observable state that witnesses its failure.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskCell {
    pub cell: CellRef,
    /// The post-state, over project state, that shows this obligation failing after the
    /// sequence. Each cell has its own witness so a demonstration can be attributed cell by
    /// cell.
    pub violation: String,
}

/// A function an obligation binds, named as the extent listing names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FunctionRef {
    pub contract: String,
    pub function: String,
}

impl std::fmt::Display for FunctionRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}()", self.contract, self.function)
    }
}

/// How a function comes to read a consumed input: the kind tells the unit agent how an open
/// input of that kind is moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    /// A call parameter the caller passes.
    Parameter,
    /// The attached `msg.value`.
    Value,
    /// Storage read before the checks or the effect, which another entry point can write.
    Storage,
    /// A value returned by an external callee, or storage read after an external call.
    External,
    /// A field an authorization (signature, permit, approval, allowlist) is meant to cover.
    Authorization,
    /// Configuration or a component a privileged role can change.
    Configuration,
    /// A moment: block, timestamp, epoch boundary, ordering with other calls.
    Moment,
    /// A record selected by key or index among many.
    Record,
    /// A value the code fixes (a constant, an immutable, a literal bound): no participant
    /// chooses it, so it is never open.
    Constant,
}

impl InputKind {
    /// How a sequence moves an open input of this kind between the check and the effect.
    /// Generic by construction: the unit agent's guidance, and the move of a `task`-mode unit.
    pub fn move_template(self, input: &str, function: &str) -> String {
        match self {
            InputKind::Parameter => format!(
                "The caller chooses `{input}` freely: {function} never fixes it. Move it to what the checks never compared: zero, the maximum, a value different from what another party expected or approved, an address that is a contract the caller deploys; then run {function} and observe a listed cell's promise."
            ),
            InputKind::Value => format!(
                "The caller chooses `{input}`: attach none, more than the effect needs, or a value that does not match the parameters; run {function} and observe where the difference goes and who ends up holding it."
            ),
            InputKind::Storage => format!(
                "`{input}` is read from storage and no check of {function} fixes it: change it first through another entry point that writes it (or in the same block, or through a helper contract), then run {function} and observe the effect computed from the changed value."
            ),
            InputKind::External => format!(
                "`{input}` comes from an external callee or is read after an external call: make the callee (a token, a callback receiver, a hook, a contract the caller deploys) return the unexpected value or change this contract's state during the call, then observe the effect {function} settles on."
            ),
            InputKind::Authorization => format!(
                "The authorization {function} relies on does not cover `{input}`: submit the authorized party's signature, permit or approval with `{input}` set to a value that party never approved, run {function} and observe the effect the party never authorized."
            ),
            InputKind::Configuration => format!(
                "`{input}` is a configuration the code lets a role change at this moment: have the role change it between the check and the effect (a privileged trigger the code permits), then an unprivileged participant's ordinary action through {function}; observe what participants lose."
            ),
            InputKind::Moment => format!(
                "The effect of {function} depends on `{input}`: run it at the boundary (expiry, epoch start, period finish), twice in one block, before and after a partial failure, or reordered against the other calls that read the same state; observe the divergence."
            ),
            InputKind::Record => format!(
                "`{input}` selects one record among many: use the last one, an empty one, a duplicate, one another account owns, or one already consumed; run {function} and observe the effect on the wrong record."
            ),
            InputKind::Constant => format!(
                "`{input}` is fixed by the code of {function}; no sequence moves it."
            ),
        }
    }
}

/// How a task's open inputs become reproductions: the scenarios its unit agent writes
/// (`subtask`) or one unit per task moving every open input (`task`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskMode {
    Subtask,
    Task,
}

impl std::str::FromStr for TaskMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "subtask" => Ok(TaskMode::Subtask),
            "task" => Ok(TaskMode::Task),
            other => Err(format!(
                "unknown task mode `{other}` (expected `subtask` or `task`)"
            )),
        }
    }
}

impl std::fmt::Display for TaskMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TaskMode::Subtask => "subtask",
            TaskMode::Task => "task",
        })
    }
}

/// One entry of what a function's execution reads to decide its effect.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConsumedInput {
    /// The value, identity, record or moment, named as the body names it.
    pub input: String,
    pub kind: InputKind,
    /// Who decides its value as the function runs: `the caller`, `the signer`, `the owner`,
    /// `the callee <contract>`, `storage written by <entry point>`, `the block`, `a constant`.
    pub decided_by: String,
}

/// One consumed input a check of the function fixes, with the check that fixes it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BoundInput {
    /// Exactly one entry of the task's `consumes`.
    pub input: String,
    /// The check that fixes it: what it requires, where (`Contract.function`, file:line).
    pub check: String,
    /// What the check fixes the input to, and so who decided its value: `msg.sender`, `the
    /// signer's signed field <f>`, `the caller's own balance`, `the stored entitlement <x> of
    /// the acting account`, `a constant`, `an owner-set parameter`. Excluding one value (a
    /// non-zero check) or bounding a range leaves the value to the caller and is not a fix.
    #[serde(default)]
    pub fixes: String,
}

/// One open input a scenario moves.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScenarioMove {
    /// One open entry of the task's `consumes`, copied exactly.
    pub input: String,
    /// How the scenario moves it: the value it takes, when and through which call.
    pub how: String,
}

/// One reproduction of a task, written by the task's unit agent: one distinct reproduction
/// test of the task's function.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Scenario {
    /// One line naming what the test reproduces.
    pub title: String,
    /// The open inputs this scenario moves together, each with how.
    pub moves: Vec<ScenarioMove>,
    /// The calls, in order, from the first one the caller sends to the one after which the
    /// promise is observed.
    pub sequence: Vec<CallStep>,
    /// The post-state, over project state, that shows the cell's promise failing.
    pub observe: String,
}

/// The scenarios one task's unit agent writes; together they move every open input.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ScenarioSet {
    pub scenarios: Vec<Scenario>,
}

impl ScenarioSet {
    /// Drops control characters from the moved input names, as the task's are.
    pub fn strip_control_characters(&mut self) {
        for m in self.scenarios.iter_mut().flat_map(|s| s.moves.iter_mut()) {
            m.input.retain(|ch| !ch.is_control());
        }
    }
}

/// A verification task: the ledger of the function through which one obligation cell's
/// violation is reached. `consumes` lists what the function's execution reads to decide its
/// effect; `binds` the entries a check fixes; every other entry is open and moved by one of the
/// scenarios the task's unit agent writes ([`TaskSet::units`]). A unit is also a
/// `VerificationTask`: one scenario of one task, carrying that task's cells and claim, with the
/// scenario's sequence and observation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct VerificationTask {
    /// The cell's key for a task (`AL01xP03`), `<cell key>-S01` for a unit: both are file
    /// names under the run (no `.`, which `with_extension` would cut).
    pub id: String,
    pub title: String,
    /// The obligations this task discharges, each with its own witness.
    pub cells: Vec<TaskCell>,
    /// The function this task walks: one the obligations bind.
    #[serde(default)]
    pub function: Option<FunctionRef>,
    /// The falsifiable claim: `an unprivileged external caller can reach <the cells'
    /// violation> through <function>`; each subtask states the input it moves to get there.
    pub claim: String,
    /// Every value, identity, record and moment the function's execution reads to decide its
    /// effect, one entry each: amounts moved, recipients, prices, records read or written,
    /// configuration read at execution, `msg.value`, and when each is read relative to the checks.
    #[serde(default)]
    pub consumes: Vec<ConsumedInput>,
    /// The entries of `consumes` a check fixes, each with the check.
    #[serde(default)]
    pub binds: Vec<BoundInput>,
    /// The state the test must build first (a unit's own setup).
    #[serde(default)]
    pub setup: String,
    /// The ordered calls that reach the violating post-state (a unit's own sequence).
    #[serde(default)]
    pub sequence: Vec<CallStep>,
    /// The post-state that shows a cell's promise failing (a unit's own observation).
    #[serde(default)]
    pub violation_condition: String,
}

impl VerificationTask {
    /// Drops control characters from the input names, which later stages must copy exactly: a
    /// name ending in an invisible NUL can never be matched again.
    pub fn strip_control_characters(&mut self) {
        for c in &mut self.consumes {
            c.input.retain(|ch| !ch.is_control());
        }
        for b in &mut self.binds {
            b.input.retain(|ch| !ch.is_control());
        }
    }

    /// The entries of `consumes` that no `binds` entry fixes and the code does not fix as a
    /// constant.
    pub fn open_inputs(&self) -> Vec<&ConsumedInput> {
        self.consumes
            .iter()
            .filter(|c| c.kind != InputKind::Constant)
            .filter(|c| !self.binds.iter().any(|b| b.input.trim() == c.input.trim()))
            .collect()
    }
}

/// Stage-3 output: the task of every obligation cell, each written by the cell's own agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskSet {
    pub tasks: Vec<VerificationTask>,
}

impl TaskSet {
    /// The units the dynamic stage verifies. In `subtask` mode every scenario a task's unit
    /// agent wrote (`scenarios`, keyed by task id) is one unit; in `task` mode a task is one
    /// unit moving all its open inputs. Each unit carries only its task's cells and claim.
    /// Ordered round-robin: every task's first unit, then every task's second, so a run cut
    /// short loses depth, not whole tasks.
    pub fn units(
        &self,
        mode: TaskMode,
        scenarios: &BTreeMap<String, ScenarioSet>,
    ) -> Vec<VerificationTask> {
        let mut units: Vec<(usize, VerificationTask)> = Vec::new();
        for task in &self.tasks {
            let Some(f) = &task.function else {
                continue;
            };
            let function = f.to_string();
            let open: Vec<&ConsumedInput> = task.open_inputs();
            if open.is_empty() {
                continue;
            }
            let observe = task
                .cells
                .iter()
                .map(|c| format!("{}: {}", c.cell, c.violation))
                .collect::<Vec<_>>()
                .join(" | ");
            let setup = "Build the state the sequence needs: deploy and initialise the contracts, fund the accounts, create the positions or records other users hold; use the project's own tests as the model. Everything not built is empty.".to_string();
            match mode {
                TaskMode::Task => {
                    let joined = open
                        .iter()
                        .map(|c| c.input.as_str())
                        .collect::<Vec<_>>()
                        .join(" | ");
                    let how = open
                        .iter()
                        .enumerate()
                        .map(|(j, c)| format!("{}. `{}`: {} (decided by {}.)", j + 1, c.input, c.kind.move_template(&c.input, &function), c.decided_by))
                        .collect::<Vec<_>>()
                        .join("\n");
                    units.push((0, VerificationTask {
                        id: format!("{}-S01", task.id),
                        title: format!("{} — {joined}", task.title),
                        cells: task.cells.clone(),
                        function: task.function.clone(),
                        claim: format!(
                            "{} This reproduction moves, one at a time, {} inputs {function} reads and none of its checks fixes:\n{how}",
                            task.claim.trim_end(),
                            open.len()
                        ),
                        consumes: task.consumes.clone(),
                        binds: task.binds.clone(),
                        setup,
                        sequence: vec![CallStep {
                            caller: "unprivileged caller (or the party the move names)".to_string(),
                            contract: f.contract.clone(),
                            function: f.function.clone(),
                            intention: format!("run the function with each of `{joined}` moved as described, one at a time"),
                        }],
                        violation_condition: format!("after the sequence, the cell's promise fails: {observe}"),
                    }));
                }
                TaskMode::Subtask => {
                    let Some(set) = scenarios.get(&task.id) else {
                        continue;
                    };
                    for (i, scenario) in set.scenarios.iter().enumerate() {
                        let how = scenario
                            .moves
                            .iter()
                            .enumerate()
                            .map(|(j, m)| {
                                let kind = open
                                    .iter()
                                    .find(|c| c.input.trim() == m.input.trim())
                                    .map(|c| format!(" ({:?}, decided by {})", c.kind, c.decided_by))
                                    .unwrap_or_default();
                                format!("{}. `{}`{kind}: {}", j + 1, m.input, m.how)
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        units.push((i, VerificationTask {
                            id: format!("{}-S{:02}", task.id, i + 1),
                            title: format!("{} — {}", task.title, scenario.title),
                            cells: task.cells.clone(),
                            function: task.function.clone(),
                            claim: format!(
                                "{} This reproduction: {}. It moves these inputs {function} reads and none of its checks fixes:\n{how}",
                                task.claim.trim_end(),
                                scenario.title.trim_end_matches('.')
                            ),
                            consumes: task.consumes.clone(),
                            binds: task.binds.clone(),
                            setup: setup.clone(),
                            sequence: scenario.sequence.clone(),
                            violation_condition: format!(
                                "after the sequence: {} (the cell's promise: {observe})",
                                scenario.observe
                            ),
                        }));
                    }
                }
            }
        }
        units.sort_by(|(a, x), (b, y)| a.cmp(b).then_with(|| x.id.cmp(&y.id)));
        units.into_iter().map(|(_, unit)| unit).collect()
    }

    pub fn describe(&self) -> String {
        let functions: std::collections::BTreeSet<String> = self
            .tasks
            .iter()
            .filter_map(|t| t.function.as_ref().map(|f| f.to_string()))
            .collect();
        format!(
            "{} task(s) on {} function(s), {} consumed input(s): {} bound, {} open",
            self.tasks.len(),
            functions.len(),
            self.tasks.iter().map(|t| t.consumes.len()).sum::<usize>(),
            self.tasks.iter().map(|t| t.binds.len()).sum::<usize>(),
            self.tasks.iter().map(|t| t.open_inputs().len()).sum::<usize>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(cell: &str, open: &[&str], bound: &[&str]) -> VerificationTask {
        let (subject_id, property_id) = cell.split_once('x').expect("cell key");
        VerificationTask {
            id: cell.to_string(),
            title: "t".to_string(),
            cells: vec![TaskCell {
                cell: CellRef {
                    subject_id: subject_id.to_string(),
                    property_id: property_id.to_string(),
                },
                violation: format!("{cell} breaks"),
            }],
            function: Some(FunctionRef {
                contract: "C".to_string(),
                function: "f".to_string(),
            }),
            claim: format!("claim of {cell}"),
            consumes: open
                .iter()
                .chain(bound.iter())
                .map(|input| ConsumedInput {
                    input: input.to_string(),
                    kind: InputKind::Parameter,
                    decided_by: "the caller".to_string(),
                })
                .collect(),
            binds: bound
                .iter()
                .map(|input| BoundInput {
                    input: input.to_string(),
                    check: "a check".to_string(),
                    fixes: "the caller's own balance".to_string(),
                })
                .collect(),
            setup: String::new(),
            sequence: Vec::new(),
            violation_condition: String::new(),
        }
    }

    fn scenario(title: &str, inputs: &[&str]) -> Scenario {
        Scenario {
            title: title.to_string(),
            moves: inputs
                .iter()
                .map(|input| ScenarioMove {
                    input: input.to_string(),
                    how: format!("move {input}"),
                })
                .collect(),
            sequence: vec![CallStep {
                caller: "unprivileged caller".to_string(),
                contract: "C".to_string(),
                function: "entry".to_string(),
                intention: "reach f".to_string(),
            }],
            observe: format!("{title} observed"),
        }
    }

    #[test]
    fn a_constant_is_never_an_open_input() {
        let mut t = task("A1xP1", &["amount", "FEE_DENOMINATOR"], &[]);
        t.consumes[1].kind = InputKind::Constant;
        t.consumes[1].decided_by = "a constant".to_string();
        let open: Vec<&str> = t.open_inputs().iter().map(|c| c.input.as_str()).collect();
        assert_eq!(open, vec!["amount"]);
        let parsed: ConsumedInput = serde_json::from_str(
            r#"{"input": "MAX_FEE_BPS", "kind": "constant", "decided_by": "a constant"}"#,
        )
        .expect("the constant kind parses");
        assert_eq!(parsed.kind, InputKind::Constant);
    }

    #[test]
    fn control_characters_are_stripped_from_input_names() {
        let tail = "the token.balanceOf(account) result read after the external call";
        let mut t = task("A1xP1", &[&format!("{tail}\u{0}"), "amount"], &["amount\t"]);
        t.strip_control_characters();
        assert_eq!(t.consumes[0].input, tail);
        assert_eq!(t.binds[0].input, "amount");
        let mut set = ScenarioSet {
            scenarios: vec![scenario("nul", &[&format!("{tail}\u{0}")])],
        };
        set.strip_control_characters();
        assert_eq!(set.scenarios[0].moves[0].input, tail);
    }

    #[test]
    fn every_scenario_is_one_unit_with_its_task_cell_claim_and_sequence() {
        let set = TaskSet {
            tasks: vec![
                task("A1xP1", &["amount", "to"], &[]),
                task("A2xP1", &["amount"], &["to"]),
            ],
        };
        let scenarios = BTreeMap::from([
            (
                "A1xP1".to_string(),
                ScenarioSet {
                    scenarios: vec![scenario("zero amount", &["amount"]), scenario("foreign to", &["to"])],
                },
            ),
            (
                "A2xP1".to_string(),
                ScenarioSet {
                    scenarios: vec![scenario("max amount", &["amount"])],
                },
            ),
        ]);
        let units = set.units(TaskMode::Subtask, &scenarios);
        let ids: Vec<&str> = units.iter().map(|u| u.id.as_str()).collect();
        // Round-robin: every task's first scenario before any task's second.
        assert_eq!(ids, vec!["A1xP1-S01", "A2xP1-S01", "A1xP1-S02"]);
        assert!(units.iter().all(|u| u.cells.len() == 1));
        assert!(units[1].claim.starts_with("claim of A2xP1"));
        assert_eq!(units[0].sequence[0].function, "entry");
        assert!(units[2].violation_condition.contains("foreign to observed"));
        let file = std::path::Path::new("verify").join(&units[0].id).with_extension("json");
        assert_eq!(file, std::path::Path::new("verify").join(format!("{}.json", units[0].id)));
    }

    #[test]
    fn a_task_without_scenarios_has_no_subtask_unit_and_one_task_unit() {
        let names: Vec<String> = (0..13).map(|i| format!("in{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let set = TaskSet { tasks: vec![task("A1xP1", &refs, &[])] };
        assert!(set.units(TaskMode::Subtask, &BTreeMap::new()).is_empty());
        let units = set.units(TaskMode::Task, &BTreeMap::new());
        assert_eq!(units.len(), 1);
        assert!(units[0].claim.contains("13 inputs"));
    }

    #[test]
    fn a_fully_bound_task_has_no_unit() {
        let set = TaskSet { tasks: vec![task("A1xP1", &[], &["amount"])] };
        let scenarios = BTreeMap::from([(
            "A1xP1".to_string(),
            ScenarioSet { scenarios: vec![scenario("s", &["amount"])] },
        )]);
        assert!(set.units(TaskMode::Subtask, &scenarios).is_empty());
    }
}
