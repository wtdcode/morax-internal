//! Dynamic verification of the obligations stated as tasks.
//!
//! For every verification task a PoC agent writes a Foundry test inside the project that tries
//! to reach the task's violating post-state, runs `forge test`, and iterates until the test
//! reverts with the violation marker or the budget ends. A demonstrated violation then goes to
//! a verdict agent that reads the harness and the source and decides whether the test drives
//! real project code from an unprivileged caller (a finding) or not.
//!
//! ```text
//! VerificationTask ──PocRunner──▶ HarnessAttempt (harness file + forge runs)
//!                                       │ violated
//!                                       ▼
//!                               VerdictRunner ──▶ Verdict ──▶ VerificationRecord (persisted)
//! ```

pub mod forge;
pub mod harness;
pub mod pipeline;
pub mod poc;
pub mod prompts;
pub mod verdict;

pub use forge::{ForgeArgs, ForgeTestSummary, OutputExcerpt};
pub use harness::HarnessWorkspace;
pub use pipeline::{Verifier, VerifyArgs, VerifyConfig};
