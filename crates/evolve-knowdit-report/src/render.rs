use std::collections::BTreeMap;

use evolve_knowdit_types::{
    CellRef, CellStatus, CoverageMatrix, CoverageSummary, Finding, FindingCluster, RunSummary,
    TaskOutcome, TaskSet, VerdictClass, VerificationRecord, VerificationTask,
};

/// Renders the coverage matrix and the findings as Markdown.
pub struct ReportRenderer;

impl ReportRenderer {
    pub fn coverage_markdown(
        matrix: &CoverageMatrix,
        tasks: &TaskSet,
        units: &[VerificationTask],
        coverage: &CoverageSummary,
        records: &[VerificationRecord],
    ) -> String {
        let mut out = String::from(
            "# Coverage matrix\n\nLegend: `-` excluded with a reason; `…` obligation without a task yet; `:k` k reproductions stated, none verified; `✓F-k` a valid finding discharged the cell; `·` verified without result (or every input bound); `✗` demonstrated but rejected by the verdict.\n\n",
        );
        out.push_str(&matrix.scope_snapshot);
        out.push_str("\n\n## Properties\n");
        for p in matrix.properties.values() {
            out.push_str(&format!(
                "- **{}** {} [{}]: {}\n",
                p.id,
                p.name,
                p.origin.slug(),
                p.statement
            ));
        }
        out.push_str("\n## Matrix\n\n| subject |");
        for id in matrix.properties.keys() {
            out.push_str(&format!(" {id} |"));
        }
        out.push_str("\n|---|");
        for _ in matrix.properties.keys() {
            out.push_str("---|");
        }
        out.push('\n');
        let by_cell: BTreeMap<_, _> = coverage.cells.iter().map(|c| (c.cell.clone(), c)).collect();
        let mut last_dimension = None;
        for s in matrix.subjects.values() {
            if last_dimension != Some(s.dimension) {
                last_dimension = Some(s.dimension);
                out.push_str(&format!(
                    "| *{} ({} level)* |{}\n",
                    s.dimension.title(),
                    s.dimension.level(),
                    " |".repeat(matrix.properties.len())
                ));
            }
            out.push_str(&format!("| **{}** {} |", s.id, s.name));
            for property_id in matrix.properties.keys() {
                let key = CellRef {
                    subject_id: s.id.clone(),
                    property_id: property_id.clone(),
                };
                let text = match by_cell.get(&key) {
                    None => "?".to_string(),
                    Some(c) => match c.status {
                        CellStatus::Excluded => "-".to_string(),
                        CellStatus::Violated => format!("✓{}", c.finding_ids.join(",")),
                        CellStatus::Held => "·".to_string(),
                        CellStatus::Rejected => "✗".to_string(),
                        CellStatus::Stated => format!(":{}", c.task_keys.len()),
                        CellStatus::Pending => "…".to_string(),
                    },
                };
                out.push_str(&format!(" {text} |"));
            }
            out.push('\n');
        }
        out.push_str("\n## Subjects\n");
        for s in matrix.subjects.values() {
            out.push_str(&format!(
                "- **{}** {} [{}]: {} ({})\n",
                s.id,
                s.name,
                s.dimension.slug(),
                s.summary,
                s.anchors
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        out.push_str("\n## Tasks by function\n");
        let mut by_function: BTreeMap<String, Vec<&VerificationTask>> = BTreeMap::new();
        for t in &tasks.tasks {
            let key = t
                .function
                .as_ref()
                .map(|f| f.to_string())
                .unwrap_or_default();
            by_function.entry(key).or_default().push(t);
        }
        for (function, cell_tasks) in &by_function {
            out.push_str(&format!("\n### {function} — {} cell task(s)\n\n", cell_tasks.len()));
            for t in cell_tasks {
                out.push_str(&format!(
                    "- `{}` {}: {}\n  consumes {}: bound {}\n",
                    t.id,
                    t.title,
                    t.claim,
                    t.consumes.len(),
                    if t.binds.is_empty() {
                        "none".to_string()
                    } else {
                        t.binds
                            .iter()
                            .map(|b| format!("`{}` ({})", b.input, b.check))
                            .collect::<Vec<_>>()
                            .join("; ")
                    }
                ));
            }
            for u in units
                .iter()
                .filter(|u| u.function.as_ref().map(|f| f.to_string()).as_deref() == Some(function.as_str()))
            {
                let outcome = records
                    .iter()
                    .find(|r| r.task_key == u.id)
                    .map(|r| {
                        let outcome = match (&r.outcome, &r.verdict) {
                            (TaskOutcome::Demonstrated, Some(v)) => format!("{:?}: {:?}", r.outcome, v.class),
                            (o, _) => format!("{o:?}"),
                        };
                        if r.updated_claim.is_some() {
                            format!("{outcome} (claim updated)")
                        } else {
                            outcome
                        }
                    })
                    .unwrap_or_else(|| "pending".to_string());
                out.push_str(&format!(
                    "  - `{}` for {}: {} → {}\n",
                    u.id,
                    u.cells
                        .iter()
                        .map(|c| c.cell.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                    u.title,
                    outcome
                ));
            }
        }
        let with_task: std::collections::BTreeSet<&CellRef> = tasks
            .tasks
            .iter()
            .flat_map(|t| t.cells.iter().map(|c| &c.cell))
            .collect();
        let missing: Vec<String> = matrix
            .obligations
            .iter()
            .filter(|o| !with_task.contains(&o.cell))
            .map(|o| format!("- {}: {}", o.cell, o.statement))
            .collect();
        if !missing.is_empty() {
            out.push_str(&format!(
                "\n## Obligations without a task ({})\n{}\n",
                missing.len(),
                missing.join("\n")
            ));
        }
        if !matrix.exclusions.is_empty() {
            out.push_str("\n## Excluded cells\n");
            for ex in &matrix.exclusions {
                out.push_str(&format!("- {}: {}\n", ex.cell, ex.reason));
            }
        }
        if !matrix.subject_exclusions.is_empty() {
            out.push_str("\n## Inventory entries not modeled\n");
            for ex in &matrix.subject_exclusions {
                out.push_str(&format!("- {}: {}\n", ex.anchor, ex.reason));
            }
        }
        out.push_str("\n## Cell counts\n");
        for (k, v) in &coverage.counts {
            out.push_str(&format!("- {k}: {v}\n"));
        }
        for (dimension, counts) in &coverage.counts_by_dimension {
            out.push_str(&format!(
                "- {dimension}: {}\n",
                counts
                    .iter()
                    .map(|(k, v)| format!("{k} {v}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        out
    }

    pub fn findings_markdown(
        findings: &[Finding],
        clusters: &[FindingCluster],
        records: &[VerificationRecord],
    ) -> String {
        let mut out = String::from("# Findings\n\n");
        if clusters.is_empty() {
            out.push_str("No valid findings.\n");
        }
        let mut ordered: Vec<&FindingCluster> = clusters.iter().collect();
        ordered.sort_by_key(|c| {
            findings
                .iter()
                .find(|f| f.id == c.representative)
                .map(|f| f.draft.severity)
                .unwrap_or(evolve_knowdit_types::Severity::Informational)
        });
        for (idx, cluster) in ordered.iter().enumerate() {
            let Some(rep) = findings.iter().find(|f| f.id == cluster.representative) else {
                continue;
            };
            out.push_str(&format!(
                "## {}. [{:?}] {}\n\n",
                idx + 1,
                rep.draft.severity,
                rep.draft.title
            ));
            out.push_str(&format!("**Location**: {}\n\n", rep.draft.location));
            out.push_str(&format!("**Root cause**: {}\n\n", rep.draft.root_cause));
            out.push_str(&format!("**Description**\n\n{}\n\n", rep.draft.description));
            out.push_str(&format!("**Impact**\n\n{}\n\n", rep.draft.impact));
            out.push_str(&format!(
                "**Recommendation**\n\n{}\n\n",
                rep.draft.recommendation
            ));
            let cells = rep
                .cells
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!(
                "**Evidence**: unit `{}`, harness `{}`; {}.\n\n",
                rep.task_key,
                rep.harness_path.clone().unwrap_or_default(),
                match &rep.updated_claim {
                    Some(claim) => format!(
                        "found while verifying cells {cells}, on a claim the reproduction updated: {claim}"
                    ),
                    None => format!("cells {cells}"),
                }
            ));
            if cluster.members.len() > 1 {
                let others: Vec<String> = cluster
                    .members
                    .iter()
                    .filter(|m| **m != cluster.representative)
                    .filter_map(|m| findings.iter().find(|f| &f.id == m))
                    .map(|f| format!("{} ({}: {})", f.id, f.task_key, f.draft.title))
                    .collect();
                out.push_str(&format!("Also demonstrated by: {}\n\n", others.join("; ")));
            }
            out.push_str(&format!(
                "<details><summary>Verdict rationale</summary>\n\n{}\n\n</details>\n\n",
                rep.verdict_rationale
            ));
        }
        out.push_str("\n# Demonstrated but rejected\n\n");
        let mut any = false;
        for record in records {
            if record.outcome != TaskOutcome::Demonstrated {
                continue;
            }
            let Some(v) = &record.verdict else {
                // The demonstration stands but its judgement failed (a provider error); the
                // task is listed so it is not mistaken for a rejection or a held obligation.
                any = true;
                out.push_str(&format!(
                    "- `{}` {} — verdict missing (the judgement did not complete; re-run the verify stage to judge it): {}\n",
                    record.task_key,
                    record.task.title,
                    record.outcome_reason.chars().take(300).collect::<String>()
                ));
                continue;
            };
            if v.class == VerdictClass::ValidFinding {
                continue;
            }
            any = true;
            out.push_str(&format!(
                "- `{}` {} — {:?}: {}\n",
                record.task_key,
                record.task.title,
                v.class,
                v.rationale.chars().take(400).collect::<String>()
            ));
        }
        if !any {
            out.push_str("(none)\n");
        }
        out.push_str("\n# Obligations held (not demonstrated)\n\n");
        for record in records {
            if record.outcome == TaskOutcome::Demonstrated {
                continue;
            }
            out.push_str(&format!(
                "- `{}` {} — {:?}: {}\n",
                record.task_key,
                record.task.title,
                record.outcome,
                record.outcome_reason.chars().take(300).collect::<String>()
            ));
        }
        out
    }

    pub fn summary(
        matrix: &CoverageMatrix,
        tasks: &TaskSet,
        units: &[VerificationTask],
        records: &[VerificationRecord],
        findings: &[Finding],
        clusters: &[FindingCluster],
        llm_cost_usd: String,
    ) -> RunSummary {
        RunSummary {
            subjects: matrix.subjects.len(),
            properties: matrix.properties.len(),
            cells: matrix.subjects.len() * matrix.properties.len(),
            excluded_cells: matrix.exclusions.len(),
            tasks: tasks.tasks.len(),
            units: units.len(),
            demonstrated: records
                .iter()
                .filter(|r| r.outcome == TaskOutcome::Demonstrated)
                .count(),
            not_demonstrated: records
                .iter()
                .filter(|r| r.outcome == TaskOutcome::NotDemonstrated)
                .count(),
            aborted: records
                .iter()
                .filter(|r| r.outcome == TaskOutcome::Aborted)
                .count(),
            valid_findings: findings.len(),
            clusters: clusters.len(),
            llm_cost_usd,
        }
    }
}
