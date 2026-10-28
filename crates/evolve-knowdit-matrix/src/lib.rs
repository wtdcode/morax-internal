//! Stages 1–3: the coverage matrix.
//!
//! For each level of the blockchain model an agent reads the source and drafts the subjects
//! that level yields and the candidate properties its reading suggests (stage 1). One agent
//! merges the candidates into the property columns (stage 2a); one agent per subject decides
//! every cell of that subject's row, writing the obligation the property becomes for it or an
//! exclusion with a reason (stage 2b); the program assembles and validates the matrix. For
//! every obligation cell an agent writes its task: the function through which the cell's
//! violation is reached and that function's ledger (stage 3). The program merges the tasks on
//! one function and derives one reproduction per open input.
//!
//! The matrix is program-managed structured data: agents submit through validated tools and
//! never edit files; every accepted submission is persisted before the next stage starts.

pub mod prompts;
pub mod stage;
pub mod tools;
pub mod validate;

pub use stage::{MatrixArgs, MatrixConfig, MatrixOutput, MatrixStage};
