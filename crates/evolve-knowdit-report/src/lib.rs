//! Turns verification records into findings, merges duplicates, computes coverage over the
//! matrix and renders the report.

pub mod coverage;
pub mod findings;
pub mod merge;
pub mod prompts;
pub mod render;

pub use coverage::CoverageBuilder;
pub use findings::FindingCollector;
pub use merge::{FindingMerger, MergeArgs};
pub use render::ReportRenderer;
