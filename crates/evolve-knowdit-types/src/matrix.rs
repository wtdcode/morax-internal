use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dimension::Dimension;

/// A location in the protocol source. `file` is relative to the Foundry project root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub struct SourceAnchor {
    /// Source file path relative to the project root, e.g. `src/Vault.sol`.
    pub file: String,
    /// Contract, library or interface name declared in that file, when the anchor is narrower
    /// than the file.
    #[serde(default)]
    pub contract: Option<String>,
    /// Function name inside `contract`, when the anchor is a single callable.
    #[serde(default)]
    pub function: Option<String>,
    /// State variable name inside `contract`, when the anchor is a storage item.
    #[serde(default)]
    pub state: Option<String>,
}

impl fmt::Display for SourceAnchor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.file)?;
        if let Some(contract) = &self.contract {
            write!(f, ":{contract}")?;
        }
        if let Some(function) = &self.function {
            write!(f, ".{function}()")?;
        }
        if let Some(state) = &self.state {
            write!(f, ".{state}")?;
        }
        Ok(())
    }
}

/// A row of the coverage matrix: one operation, one module or one asset, read from the source.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Subject {
    /// Short unique id carrying the level code, e.g. `OF01`, `MOD03`, `AL02`.
    pub id: String,
    /// The level the subject belongs to: `operation_flow`, `module` or `asset_lifecycle`.
    pub dimension: Dimension,
    /// Human-readable name, e.g. `deposit(uint256)` or `Vault` or `share supply accounting`.
    pub name: String,
    /// One or two sentences: what this subject is and what its extent of code is.
    pub summary: String,
    /// Source anchors that define the subject: entry points for an operation, the contract for
    /// a module, the state variables and the functions that create/move/price/destroy an asset.
    pub anchors: Vec<SourceAnchor>,
}

/// A column of the coverage matrix: a property subjects are asked to satisfy against an
/// unprivileged external caller.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Property {
    /// Short unique id, `P01`, `P02`, ...
    pub id: String,
    /// The level whose reading produced the property.
    pub origin: Dimension,
    /// Short name, e.g. `settled accounting`.
    pub name: String,
    /// The property as a checkable statement, phrased so that its violation is an observable
    /// post-state an unprivileged external caller can reach.
    pub statement: String,
}

/// A cell of the coverage matrix.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub struct CellRef {
    pub subject_id: String,
    pub property_id: String,
}

impl CellRef {
    /// The cell as a file and id component: `AL01xP03`.
    pub fn key(&self) -> String {
        format!("{}x{}", self.subject_id, self.property_id)
    }
}

impl fmt::Display for CellRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}×{}", self.subject_id, self.property_id)
    }
}

/// The obligation a cell carries: the property instantiated for the subject, stating which
/// functions and state of the subject it binds and which post-state counts as its violation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Obligation {
    pub cell: CellRef,
    pub statement: String,
    /// The functions and state the obligation binds (from the row decision).
    #[serde(default)]
    pub anchors: Vec<SourceAnchor>,
    /// The row agent's argument for anchors outside the subject's listed functions: the call
    /// path from each into the subject. Empty when every anchor was listed.
    #[serde(default)]
    pub reach: String,
}

/// A cell no line owns, with the stated reason. `-` in the rendered matrix.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Exclusion {
    pub cell: CellRef,
    /// Why the property cannot apply to the subject, grounded in source.
    pub reason: String,
}

/// A candidate subject the source inventory offers that the matrix deliberately does not model.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SubjectExclusion {
    /// The inventory entry, e.g. `src/Vault.sol:Vault.totalAssets()`.
    pub anchor: SourceAnchor,
    /// Why no subject covers it (pure view, thin wrapper of a modeled entry point, ...).
    pub reason: String,
}

/// Stage-1 output for one level: the subjects the source offers at that level, the candidate
/// properties that reading suggests, and the inventory entries no subject models.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MatrixDraft {
    pub dimension: Dimension,
    /// Two to five sentences: what the protocol does and what this level's reading covers.
    pub scope_snapshot: String,
    pub subjects: Vec<Subject>,
    /// Candidate columns; the property stage merges duplicates across levels.
    pub properties: Vec<Property>,
    #[serde(default)]
    pub subject_exclusions: Vec<SubjectExclusion>,
}

impl MatrixDraft {
    /// Subject ids of a level start with the level code so the union across levels is
    /// collision-free.
    pub fn structural_errors(&self, max_properties: usize) -> Vec<String> {
        let mut errors = Vec::new();
        if self.subjects.is_empty() {
            errors.push("no subjects: the inventory must yield at least one subject".to_string());
        }
        if self.properties.is_empty() {
            errors.push("no properties: propose at least one candidate column".to_string());
        }
        if self.properties.len() > max_properties {
            errors.push(format!(
                "{} properties exceed the maximum of {max_properties}; merge properties that decide the same fact",
                self.properties.len()
            ));
        }
        let code = self.dimension.code();
        let mut seen = BTreeSet::new();
        for s in &self.subjects {
            if !seen.insert(s.id.clone()) {
                errors.push(format!("duplicate subject id {}", s.id));
            }
            if !s.id.starts_with(code) {
                errors.push(format!(
                    "subject id {} must start with the level code `{code}` (e.g. {code}01)",
                    s.id
                ));
            }
            if s.dimension != self.dimension {
                errors.push(format!(
                    "subject {} has dimension `{}` but this draft is `{}`",
                    s.id, s.dimension, self.dimension
                ));
            }
            if s.anchors.is_empty() {
                errors.push(format!("subject {} has no source anchors", s.id));
            }
            if s.name.trim().is_empty() || s.summary.trim().is_empty() {
                errors.push(format!("subject {} has an empty name or summary", s.id));
            }
        }
        let mut seen = BTreeSet::new();
        for p in &self.properties {
            if !seen.insert(p.id.clone()) {
                errors.push(format!("duplicate property id {}", p.id));
            }
            if p.origin != self.dimension {
                errors.push(format!(
                    "property {} has origin `{}` but this draft is `{}`",
                    p.id, p.origin, self.dimension
                ));
            }
            if p.statement.trim().is_empty() || p.name.trim().is_empty() {
                errors.push(format!("property {} has an empty name or statement", p.id));
            }
        }
        for ex in &self.subject_exclusions {
            if ex.reason.trim().is_empty() {
                errors.push(format!("subject exclusion of {} has no reason", ex.anchor));
            }
        }
        errors
    }

    pub fn describe(&self) -> String {
        format!(
            "{} subjects, {} candidate properties, {} inventory entries excluded",
            self.subjects.len(),
            self.properties.len(),
            self.subject_exclusions.len()
        )
    }
}

/// Stage-2a output: the merged columns of the matrix and the protocol snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PropertySet {
    pub scope_snapshot: String,
    pub properties: Vec<Property>,
}

impl PropertySet {
    pub fn structural_errors(&self, max_properties: usize) -> Vec<String> {
        let mut errors = Vec::new();
        if self.properties.is_empty() {
            errors.push("no properties".to_string());
        }
        if self.properties.len() > max_properties {
            errors.push(format!(
                "{} properties exceed the maximum of {max_properties}; merge properties whose violations are the same post-state",
                self.properties.len()
            ));
        }
        if self.scope_snapshot.trim().is_empty() {
            errors.push("scope_snapshot is empty".to_string());
        }
        for (idx, p) in self.properties.iter().enumerate() {
            let expected = format!("P{:02}", idx + 1);
            if p.id != expected {
                errors.push(format!(
                    "property #{} must be named {expected}, found {}",
                    idx + 1,
                    p.id
                ));
            }
            if p.statement.trim().len() < 40 || p.name.trim().is_empty() {
                errors.push(format!(
                    "property {} needs a name and a statement that says what is checked and what its violation looks like",
                    p.id
                ));
            }
        }
        errors
    }
}

/// One decided cell of a subject row: the obligation the property becomes for that subject.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RowObligation {
    pub property_id: String,
    /// The property instantiated for this subject: the functions and state it binds and the
    /// post-state that counts as its violation.
    pub statement: String,
    /// The functions (and state variables) of this subject the obligation binds: every function
    /// through which an unprivileged sequence could reach the violation post-state, and the state
    /// that post-state is read from. Validated against the code index; the task stage walks
    /// exactly these functions for this obligation.
    #[serde(default)]
    pub anchors: Vec<SourceAnchor>,
    /// For anchors outside the subject's listed functions: the call path from each into this
    /// subject's code or state. Empty when every anchor is listed.
    #[serde(default)]
    pub reach: String,
}

/// One excluded cell of a subject row.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RowExclusion {
    pub property_id: String,
    /// Why the property is meaningless for this subject, grounded in source.
    pub reason: String,
}

/// Stage-2b output: every property decided for one subject.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SubjectRow {
    pub subject_id: String,
    #[serde(default)]
    pub obligations: Vec<RowObligation>,
    #[serde(default)]
    pub exclusions: Vec<RowExclusion>,
}

impl SubjectRow {
    /// Every property decided exactly once, with a substantive statement or reason.
    pub fn structural_errors(&self, subject_id: &str, properties: &[Property]) -> Vec<String> {
        let mut errors = Vec::new();
        if self.subject_id != subject_id {
            errors.push(format!(
                "subject_id must be `{subject_id}`, found `{}`",
                self.subject_id
            ));
        }
        let known: BTreeSet<&str> = properties.iter().map(|p| p.id.as_str()).collect();
        let mut decided: BTreeMap<&str, usize> = BTreeMap::new();
        for o in &self.obligations {
            *decided.entry(o.property_id.as_str()).or_default() += 1;
            if !known.contains(o.property_id.as_str()) {
                errors.push(format!(
                    "obligation names unknown property {}",
                    o.property_id
                ));
            }
            if o.statement.trim().len() < 60 {
                errors.push(format!(
                    "obligation {} needs a statement that names the functions or state of this subject it binds and the post-state that counts as its violation",
                    o.property_id
                ));
            }
        }
        for e in &self.exclusions {
            *decided.entry(e.property_id.as_str()).or_default() += 1;
            if !known.contains(e.property_id.as_str()) {
                errors.push(format!(
                    "exclusion names unknown property {}",
                    e.property_id
                ));
            }
            if e.reason.trim().len() < 40 {
                errors.push(format!(
                    "exclusion {} needs a reason grounded in source for why the property is meaningless for this subject",
                    e.property_id
                ));
            }
        }
        for (id, count) in &decided {
            if *count > 1 {
                errors.push(format!(
                    "property {id} is decided {count} times; decide it once"
                ));
            }
        }
        let missing: Vec<&str> = properties
            .iter()
            .map(|p| p.id.as_str())
            .filter(|id| !decided.contains_key(id))
            .collect();
        if !missing.is_empty() {
            errors.push(format!(
                "properties not decided for this subject (obligation or exclusion): {}",
                missing.join(", ")
            ));
        }
        errors
    }

    pub fn describe(&self) -> String {
        format!(
            "{} obligations, {} exclusions",
            self.obligations.len(),
            self.exclusions.len()
        )
    }
}

/// The coverage matrix: every subject of every level as a row, every property as a column,
/// every cell either an obligation or an exclusion with a reason. Assembled by the program from
/// the level drafts, the property set and the subject rows; every obligation gets its own task.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoverageMatrix {
    /// A short description of the protocol and of the in-scope code this matrix models.
    pub scope_snapshot: String,
    /// Subject id → subject.
    pub subjects: BTreeMap<String, Subject>,
    /// Property id → property.
    pub properties: BTreeMap<String, Property>,
    pub obligations: Vec<Obligation>,
    pub exclusions: Vec<Exclusion>,
    #[serde(default)]
    pub subject_exclusions: Vec<SubjectExclusion>,
}

impl CoverageMatrix {
    pub fn assemble(
        drafts: &[MatrixDraft],
        properties: &PropertySet,
        rows: &[SubjectRow],
    ) -> Self {
        let mut obligations = Vec::new();
        let mut exclusions = Vec::new();
        for row in rows {
            for o in &row.obligations {
                obligations.push(Obligation {
                    cell: CellRef {
                        subject_id: row.subject_id.clone(),
                        property_id: o.property_id.clone(),
                    },
                    statement: o.statement.clone(),
                    anchors: o.anchors.clone(),
                    reach: o.reach.clone(),
                });
            }
            for e in &row.exclusions {
                exclusions.push(Exclusion {
                    cell: CellRef {
                        subject_id: row.subject_id.clone(),
                        property_id: e.property_id.clone(),
                    },
                    reason: e.reason.clone(),
                });
            }
        }
        Self {
            scope_snapshot: properties.scope_snapshot.clone(),
            subjects: drafts
                .iter()
                .flat_map(|d| d.subjects.iter().map(|s| (s.id.clone(), s.clone())))
                .collect(),
            properties: properties
                .properties
                .iter()
                .map(|p| (p.id.clone(), p.clone()))
                .collect(),
            obligations,
            exclusions,
            subject_exclusions: drafts
                .iter()
                .flat_map(|d| d.subject_exclusions.iter().cloned())
                .collect(),
        }
    }

    /// The obligation statement of every cell.
    pub fn obligation_statements(&self) -> BTreeMap<CellRef, String> {
        self.obligations
            .iter()
            .map(|o| (o.cell.clone(), o.statement.clone()))
            .collect()
    }

    /// The anchors of every obligation cell (the functions and state it binds).
    pub fn obligation_anchors(&self) -> BTreeMap<CellRef, Vec<SourceAnchor>> {
        self.obligations
            .iter()
            .map(|o| (o.cell.clone(), o.anchors.clone()))
            .collect()
    }

    /// The exclusion reason of every excluded cell.
    pub fn exclusion_reasons(&self) -> BTreeMap<CellRef, String> {
        self.exclusions
            .iter()
            .map(|e| (e.cell.clone(), e.reason.clone()))
            .collect()
    }

    /// Structural validation of the assembled matrix: every subject × property is exactly one
    /// of obligation or exclusion. Returns one message per problem.
    pub fn structural_errors(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let obligations = self.obligation_statements();
        let excluded = self.exclusion_reasons();
        for s in self.subjects.keys() {
            for p in self.properties.keys() {
                let cell = CellRef {
                    subject_id: s.clone(),
                    property_id: p.clone(),
                };
                match (
                    obligations.contains_key(&cell),
                    excluded.contains_key(&cell),
                ) {
                    (true, true) => {
                        errors.push(format!("cell {cell} is both an obligation and excluded"))
                    }
                    (false, false) => {
                        errors.push(format!("cell {cell} is neither an obligation nor excluded"))
                    }
                    _ => {}
                }
            }
        }
        if obligations.is_empty() {
            errors.push("no obligations: every cell is excluded".to_string());
        }
        errors
    }

    pub fn describe(&self) -> String {
        format!(
            "{} subjects × {} properties: {} obligations, {} excluded cells",
            self.subjects.len(),
            self.properties.len(),
            self.obligations.len(),
            self.exclusions.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor(file: &str, contract: &str) -> SourceAnchor {
        SourceAnchor {
            file: file.to_string(),
            contract: Some(contract.to_string()),
            function: None,
            state: None,
        }
    }

    fn subject(id: &str, dimension: Dimension) -> Subject {
        Subject {
            id: id.to_string(),
            dimension,
            name: id.to_string(),
            summary: "a subject".into(),
            anchors: vec![anchor("src/Vault.sol", "Vault")],
        }
    }

    fn property(id: &str) -> Property {
        Property {
            id: id.to_string(),
            origin: Dimension::Module,
            name: id.to_string(),
            statement: "a statement long enough to describe what is checked and its violation"
                .into(),
        }
    }

    fn parts() -> (Vec<MatrixDraft>, PropertySet, Vec<SubjectRow>) {
        let drafts = vec![
            MatrixDraft {
                dimension: Dimension::Module,
                scope_snapshot: "s".into(),
                subjects: vec![subject("MOD01", Dimension::Module)],
                properties: vec![],
                subject_exclusions: vec![],
            },
            MatrixDraft {
                dimension: Dimension::AssetLifecycle,
                scope_snapshot: "s".into(),
                subjects: vec![subject("AL01", Dimension::AssetLifecycle)],
                properties: vec![],
                subject_exclusions: vec![],
            },
        ];
        let properties = PropertySet {
            scope_snapshot: "the protocol".into(),
            properties: vec![property("P01"), property("P02")],
        };
        let statement = "the vault's share supply equals the sum of balances after every entry point; violated when a call leaves them unequal".to_string();
        let rows = vec![
            SubjectRow {
                subject_id: "MOD01".into(),
                obligations: vec![RowObligation {
                    property_id: "P01".into(),
                    statement: statement.clone(),
                    anchors: Vec::new(),
                    reach: String::new(),
                }],
                exclusions: vec![RowExclusion {
                    property_id: "P02".into(),
                    reason: "a unit has no balance sum; the property binds ledgers only".into(),
                }],
            },
            SubjectRow {
                subject_id: "AL01".into(),
                obligations: vec![
                    RowObligation {
                        property_id: "P01".into(),
                        statement: statement.clone(),
                        anchors: Vec::new(),
                        reach: String::new(),
                    },
                    RowObligation {
                        property_id: "P02".into(),
                        statement,
                        anchors: Vec::new(),
                        reach: String::new(),
                    },
                ],
                exclusions: vec![],
            },
        ];
        (drafts, properties, rows)
    }

    #[test]
    fn rows_assemble_into_a_valid_matrix() {
        let (drafts, properties, rows) = parts();
        for row in &rows {
            assert!(
                row.structural_errors(&row.subject_id, &properties.properties)
                    .is_empty(),
                "{:?}",
                row.structural_errors(&row.subject_id, &properties.properties)
            );
        }
        let m = CoverageMatrix::assemble(&drafts, &properties, &rows);
        assert!(m.structural_errors().is_empty(), "{:?}", m.structural_errors());
        assert_eq!(m.obligations.len(), 3);
        assert_eq!(m.exclusions.len(), 1);
    }

    #[test]
    fn an_undecided_cell_is_reported() {
        let (drafts, properties, mut rows) = parts();
        rows[0].exclusions.clear();
        let m = CoverageMatrix::assemble(&drafts, &properties, &rows);
        assert!(
            m.structural_errors()
                .iter()
                .any(|e| e.contains("neither an obligation nor excluded"))
        );
    }

    #[test]
    fn a_row_must_decide_every_property_once() {
        let (_, properties, mut rows) = parts();
        rows[0].exclusions.clear();
        let errors = rows[0].structural_errors("MOD01", &properties.properties);
        assert!(
            errors.iter().any(|e| e.contains("not decided")),
            "{errors:?}"
        );
        rows[1].obligations.push(RowObligation {
            property_id: "P01".into(),
            statement: "x".repeat(80),
            anchors: Vec::new(),
            reach: String::new(),
        });
        let errors = rows[1].structural_errors("AL01", &properties.properties);
        assert!(
            errors.iter().any(|e| e.contains("decided 2 times")),
            "{errors:?}"
        );
    }

    #[test]
    fn dimension_parses_case_insensitively() {
        assert_eq!(Dimension::parse_slug("Module"), Some(Dimension::Module));
        assert_eq!(
            Dimension::parse_slug("Operation Flow"),
            Some(Dimension::OperationFlow)
        );
        let parsed: Dimension = serde_json::from_str("\"Asset-Lifecycle\"").expect("parses");
        assert_eq!(parsed, Dimension::AssetLifecycle);
    }
}
