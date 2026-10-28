use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The three audit dimensions, one per level of the blockchain model: the transaction level
/// yields operations, the contract level yields modules, and the asset level yields assets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    Module,
    OperationFlow,
    AssetLifecycle,
}

impl Dimension {
    pub fn all() -> [Dimension; 3] {
        [
            Dimension::Module,
            Dimension::OperationFlow,
            Dimension::AssetLifecycle,
        ]
    }

    /// Stable directory / file slug.
    pub fn slug(&self) -> &'static str {
        match self {
            Dimension::Module => "module",
            Dimension::OperationFlow => "operation_flow",
            Dimension::AssetLifecycle => "asset_lifecycle",
        }
    }

    /// Short code used inside task and finding identifiers.
    pub fn code(&self) -> &'static str {
        match self {
            Dimension::Module => "MOD",
            Dimension::OperationFlow => "OF",
            Dimension::AssetLifecycle => "AL",
        }
    }

    pub fn title(&self) -> &'static str {
        match self {
            Dimension::Module => "Module",
            Dimension::OperationFlow => "Operation Flow",
            Dimension::AssetLifecycle => "Asset Lifecycle",
        }
    }

    /// The level of the blockchain model this dimension reads.
    pub fn level(&self) -> &'static str {
        match self {
            Dimension::Module => "contract",
            Dimension::OperationFlow => "transaction",
            Dimension::AssetLifecycle => "asset",
        }
    }

    /// Accepts the slug, the code or the title in any case, with `-` or spaces for `_`.
    pub fn parse_slug(value: &str) -> Option<Dimension> {
        let norm = value.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        Dimension::all().into_iter().find(|d| {
            d.slug() == norm
                || d.code().eq_ignore_ascii_case(&norm)
                || d.title().to_ascii_lowercase().replace(' ', "_") == norm
        })
    }
}

impl<'de> Deserialize<'de> for Dimension {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Dimension::parse_slug(&raw).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown dimension `{raw}`; expected module, operation_flow or asset_lifecycle"
            ))
        })
    }
}

impl fmt::Display for Dimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}
