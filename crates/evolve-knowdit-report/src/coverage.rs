use std::collections::BTreeMap;

use evolve_knowdit_types::{
    CellRef, CellStatus, CoverageCell, CoverageMatrix, CoverageSummary, Finding, TaskOutcome,
    TaskSet, VerdictClass, VerificationRecord, VerificationTask,
};

/// Computes the status of every cell from the matrix, the tasks, the units and the records.
pub struct CoverageBuilder;

impl CoverageBuilder {
    pub fn build(
        matrix: &CoverageMatrix,
        tasks: &TaskSet,
        units: &[VerificationTask],
        records: &[VerificationRecord],
        findings: &[Finding],
    ) -> CoverageSummary {
        let obligations = matrix.obligation_statements();
        let mut cells = Vec::new();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut counts_by_dimension: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
        for subject in matrix.subjects.values() {
            for property_id in matrix.properties.keys() {
                let cell = CellRef {
                    subject_id: subject.id.clone(),
                    property_id: property_id.clone(),
                };
                let has_task = tasks
                    .tasks
                    .iter()
                    .any(|t| t.cells.iter().any(|c| c.cell == cell));
                let (status, task_keys, finding_ids) = if !obligations.contains_key(&cell) {
                    (CellStatus::Excluded, vec![], vec![])
                } else if !has_task {
                    (CellStatus::Pending, vec![], vec![])
                } else {
                    Self::cell_status(&cell, units, records, findings)
                };
                let key = format!("{status:?}").to_lowercase();
                *counts.entry(key.clone()).or_default() += 1;
                *counts_by_dimension
                    .entry(subject.dimension.slug().to_string())
                    .or_default()
                    .entry(key)
                    .or_default() += 1;
                cells.push(CoverageCell {
                    cell,
                    status,
                    task_keys,
                    finding_ids,
                });
            }
        }
        CoverageSummary {
            cells,
            counts_by_dimension,
            counts,
        }
    }

    fn cell_status(
        cell: &CellRef,
        units: &[VerificationTask],
        records: &[VerificationRecord],
        findings: &[Finding],
    ) -> (CellStatus, Vec<String>, Vec<String>) {
        let task_keys: Vec<String> = units
            .iter()
            .filter(|u| u.cells.iter().any(|c| &c.cell == cell))
            .map(|u| u.id.clone())
            .collect();
        if task_keys.is_empty() {
            // Every input the cell's task consumes is bound by a named check: held, with no
            // unit to run.
            return (CellStatus::Held, vec![], vec![]);
        }
        // A finding on a claim the reproduction updated was found at the cell but does not
        // violate it; its record counts as verified, like a conclusion.
        let finding_ids: Vec<String> = findings
            .iter()
            .filter(|f| {
                task_keys.contains(&f.task_key) && f.cells.contains(cell) && f.updated_claim.is_none()
            })
            .map(|f| f.id.clone())
            .collect();
        if !finding_ids.is_empty() {
            return (CellStatus::Violated, task_keys, finding_ids);
        }
        let mut any_record = false;
        let mut any_demonstrated = false;
        for key in &task_keys {
            if let Some(record) = records.iter().find(|r| &r.task_key == key) {
                any_record = true;
                if record.outcome == TaskOutcome::Demonstrated && record.updated_claim.is_none() {
                    let rejected = record
                        .verdict
                        .as_ref()
                        .map(|v| v.class != VerdictClass::ValidFinding)
                        .unwrap_or(true);
                    if rejected {
                        any_demonstrated = true;
                    }
                }
            }
        }
        if !any_record {
            return (CellStatus::Stated, task_keys, vec![]);
        }
        if any_demonstrated {
            return (CellStatus::Rejected, task_keys, vec![]);
        }
        (CellStatus::Held, task_keys, vec![])
    }
}
