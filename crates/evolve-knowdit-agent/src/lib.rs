//! The one loop every agent of the pipeline runs: llmy's `Agent` stepped until a submit tool
//! raises the shared `accepted` flag, within a step budget, re-asking a model that stops early.

pub mod budget;
pub mod stages;

pub use budget::{ProgressCheckpoint, RunBudget, RunEnd, RunReport, RunRequest};
pub use stages::{Stage, StageLlms};
