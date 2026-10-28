//! Which LLM each agent stage runs on. With a llmy config, a profile named after one or more
//! stages (`poc`, or `task,unit,poc`) serves those stages, one client for all of them; every other
//! stage runs on the default LLM (the config's fallback chain, or the env/flag endpoint when no
//! config is given).

use std::collections::BTreeMap;

use llmy::client::billing::TokenUsage;
use llmy::client::client::LLM;
use llmy::client::rust_decimal::Decimal;

/// The agent stages of the pipeline, named as the llmy config profiles that select them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    Draft,
    Properties,
    Row,
    Plan,
    Task,
    Unit,
    Poc,
    Verdict,
    Merge,
}

impl Stage {
    pub const ALL: [Stage; 9] = [
        Stage::Draft,
        Stage::Properties,
        Stage::Row,
        Stage::Plan,
        Stage::Task,
        Stage::Unit,
        Stage::Poc,
        Stage::Verdict,
        Stage::Merge,
    ];

    /// The stage named in a profile name, if any.
    pub fn from_name(name: &str) -> Option<Stage> {
        Stage::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The name that selects this stage in a profile name.
    pub fn name(self) -> &'static str {
        match self {
            Stage::Draft => "draft",
            Stage::Properties => "properties",
            Stage::Row => "row",
            Stage::Plan => "plan",
            Stage::Task => "task",
            Stage::Unit => "unit",
            Stage::Poc => "poc",
            Stage::Verdict => "verdict",
            Stage::Merge => "merge",
        }
    }
}

/// The default LLM plus one LLM per stage profile, and which stages each profile serves.
#[derive(Debug, Clone)]
pub struct StageLlms {
    pub default: LLM,
    pub profiles: Vec<LLM>,
    /// Stage → index into `profiles`; stages absent run on `default`.
    pub stages: BTreeMap<Stage, usize>,
}

impl StageLlms {
    /// The LLM a stage runs on: its profile's when the config names one, else the default.
    pub fn of(&self, stage: Stage) -> &LLM {
        self.stages
            .get(&stage)
            .map(|&i| &self.profiles[i])
            .unwrap_or(&self.default)
    }

    /// The same LLMs under a child billing scope each, for per-phase usage.
    pub fn scope(&self, name: &str) -> Self {
        Self {
            default: self.default.scope(Some(name.to_string()), None),
            profiles: self
                .profiles
                .iter()
                .map(|llm| llm.scope(Some(name.to_string()), None))
                .collect(),
            stages: self.stages.clone(),
        }
    }

    fn all(&self) -> impl Iterator<Item = &LLM> {
        std::iter::once(&self.default).chain(self.profiles.iter())
    }

    pub fn usage(&self) -> TokenUsage {
        self.all().fold(TokenUsage::default(), |acc, llm| acc + llm.usage())
    }

    pub fn cost(&self) -> Decimal {
        self.all().map(LLM::cost).sum()
    }
}
