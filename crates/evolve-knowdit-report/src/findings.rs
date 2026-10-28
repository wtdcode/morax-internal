use evolve_knowdit_types::{Finding, VerdictClass, VerificationRecord};

/// Extracts findings from the records whose verdict is `ValidFinding`.
pub struct FindingCollector;

impl FindingCollector {
    pub fn collect(records: &[VerificationRecord]) -> Vec<Finding> {
        let mut out = Vec::new();
        for record in records {
            let Some(verdict) = &record.verdict else {
                continue;
            };
            if verdict.class != VerdictClass::ValidFinding {
                continue;
            }
            let Some(draft) = &verdict.finding else {
                continue;
            };
            let evidence_tail = record
                .runs
                .iter()
                .rev()
                .find(|r| r.violated)
                .map(|r| {
                    let t = &r.stdout_tail;
                    if t.len() > 3000 {
                        let mut start = t.len() - 3000;
                        while start < t.len() && !t.is_char_boundary(start) {
                            start += 1;
                        }
                        t[start..].to_string()
                    } else {
                        t.clone()
                    }
                })
                .unwrap_or_default();
            out.push(Finding {
                id: format!("F-{:03}", out.len() + 1),
                task_key: record.task_key.clone(),
                cells: record.task.cells.iter().map(|c| c.cell.clone()).collect(),
                draft: draft.clone(),
                verdict_rationale: verdict.rationale.clone(),
                harness_path: record.harness_path.clone(),
                evidence_tail,
                updated_claim: record.updated_claim.as_ref().map(|c| c.claim.clone()),
            });
        }
        out
    }
}
