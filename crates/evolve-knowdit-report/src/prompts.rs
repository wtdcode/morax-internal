//! Prompts of the report stage: the agent that clusters confirmed findings by root cause.

use evolve_knowdit_types::{Finding, ScopeContext};

pub struct MergePrompt;

impl MergePrompt {
    pub fn system(scope: &ScopeContext, overview: &str, tool_guide: &str) -> String {
        let mut out = String::from(
            "You cluster the confirmed findings of a smart-contract audit by root cause, so the report states each defect once. Two findings share a cluster when fixing one defect in the code fixes both: the same function (or the same missing invariant across an inheritance chain) and the same mechanism. Different mechanisms in the same function are different clusters; the same mechanism reached through different entry points is one cluster.\n\
             Do not decide from the write-ups alone: whenever two findings might share a root cause, read the functions they cite with the tools and check whether one fix closes both. Then call `submit_clusters` once with every finding placed in exactly one cluster; pick as representative the member with the most precise write-up. A rejected submission returns the problems to fix.\n\n",
        );
        if !scope.readme_excerpt.is_empty() {
            out.push_str("## Project README (excerpt)\n");
            out.push_str(&scope.readme_excerpt);
            out.push_str("\n\n");
        }
        out.push_str(&format!("## Project overview\n{overview}\n\n{tool_guide}\n\nFinish by calling `submit_clusters`."));
        out
    }

    pub fn user(findings: &[Finding]) -> String {
        let mut out = String::from("Findings:\n");
        for f in findings {
            out.push_str(&format!(
                "- {}: [{:?}] {} | location: {} | root cause: {} | impact: {} | cells: {}\n",
                f.id,
                f.draft.severity,
                f.draft.title,
                f.draft.location,
                f.draft.root_cause,
                f.draft.impact.chars().take(240).collect::<String>(),
                f.cells
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        out.push_str(
            "\n## Payload shape of `submit_clusters`\n```json\n{\n  \"clusters\": [\n    {\"representative\": \"F-001\", \"members\": [\"F-001\", \"F-004\"], \"root_cause\": \"one line naming the shared defect\"}\n  ]\n}\n```\nEvery finding id appears in exactly one cluster.\n\nCheck the cited functions, then call `submit_clusters`.",
        );
        out
    }

    pub fn nudge() -> String {
        "You ended your turn without calling `submit_clusters`. Call it now with every finding placed in exactly one cluster.".to_string()
    }
}
