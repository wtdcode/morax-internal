use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::matrix::CellRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellStatus {
    /// `-` in the matrix.
    Excluded,
    /// At least one unit carries the cell, none verified yet.
    Stated,
    /// At least one task verified, none demonstrated.
    Held,
    /// At least one demonstrated task with a valid finding.
    Violated,
    /// Demonstrated but every verdict rejected the demonstration.
    Rejected,
    /// An obligation whose task was not written (its agent did not finish).
    Pending,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageCell {
    pub cell: CellRef,
    pub status: CellStatus,
    pub task_keys: Vec<String>,
    pub finding_ids: Vec<String>,
}

/// Status of every cell of the matrix plus counts per level and overall.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageSummary {
    pub cells: Vec<CoverageCell>,
    /// Level slug → status → count.
    pub counts_by_dimension: BTreeMap<String, BTreeMap<String, usize>>,
    pub counts: BTreeMap<String, usize>,
}

/// Top-level numbers for one run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunSummary {
    pub subjects: usize,
    pub properties: usize,
    pub cells: usize,
    pub excluded_cells: usize,
    /// Obligation cells with a task.
    pub tasks: usize,
    /// Reproductions derived from the tasks.
    pub units: usize,
    pub demonstrated: usize,
    pub not_demonstrated: usize,
    pub aborted: usize,
    pub valid_findings: usize,
    pub clusters: usize,
    pub llm_cost_usd: String,
}
