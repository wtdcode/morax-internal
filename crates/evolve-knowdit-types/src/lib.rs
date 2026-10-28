//! Data types shared by every stage of the pipeline.
//!
//! Data flow through the pipeline:
//!
//! ```text
//! ProjectManifest ──▶ CoverageMatrix (per-level drafts, then one assembled matrix: every
//!                     subject × every property, each cell an obligation or excluded)
//!                 ──▶ TaskSet (one VerificationTask per obligation cell, written by its agent)
//!                 ──▶ ScenarioSet (per task, written by its unit agent) ──▶ units (one per scenario)
//!                 ──▶ VerificationRecord (per unit: harness attempts + forge runs + verdict)
//!                 ──▶ Finding (only for ValidFinding verdicts) ──▶ FindingCluster ──▶ report
//! ```
//!
//! Every struct here is plain serde data so each stage can persist its output through
//! [`store::JsonStore`] and a later run can resume from what already exists on disk.

pub mod dimension;
pub mod matrix;
pub mod project;
pub mod report;
pub mod store;
pub mod task;
pub mod verify;

pub use dimension::Dimension;
pub use matrix::{
    CellRef, CoverageMatrix, Exclusion, MatrixDraft, Obligation, Property, PropertySet,
    RowExclusion, RowObligation, SourceAnchor, Subject, SubjectExclusion, SubjectRow,
};
pub use project::{FoundryLayout, ProjectManifest, ScopeContext};
pub use report::{CellStatus, CoverageCell, CoverageSummary, RunSummary};
pub use store::{JsonStore, RunLayout};
pub use task::{
    BoundInput, CallStep, ConsumedInput, FunctionRef, InputKind, Scenario, ScenarioMove,
    ScenarioSet, TaskCell, TaskMode, TaskSet, VerificationTask,
};
pub use verify::{
    Finding, FindingCluster, FindingDraft, ForgeRunDigest, ForgeRunKind, SequenceCoverage,
    Severity, TaskOutcome, UpdatedClaim, Verdict, VerdictClass, VerificationRecord,
};
