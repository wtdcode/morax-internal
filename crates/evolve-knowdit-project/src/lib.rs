//! Project-side facts every agent stage reads: where the Foundry project is, which files the
//! engagement puts in scope, a code index over the project's own sources, and the tool box
//! that lets an agent inspect the project.
//!
//! ```text
//! dataset root ──ProjectResolver──▶ source Foundry root ──Workspace::materialize──▶ working copy
//!                                            │
//!                    ScopeLoader ◀───────────┘──▶ ScopeContext
//!                    FoundryToml ─────────────────▶ FoundryLayout
//!                    AstBuild::load_or_compile ───▶ compiler ASTs (forge build --ast, cached per run)
//!                    CodeIndex::build ────────────▶ CodeIndex (project sources only, scope-marked)
//!                    ProjectConventions::load ────▶ remappings / sample tests for harness prompts
//!                    ProjectTools ────────────────▶ ToolBox (code graph + file tools)
//! ```

pub mod ast_build;
pub mod conventions;
pub mod index;
pub mod layout;
pub mod repair;
pub mod scope;
pub mod tools;
pub mod workspace;

pub use ast_build::AstBuild;
pub use conventions::ProjectConventions;
pub use index::{CodeIndex, ExtentFunction, ExtentRelation, InventoryEntry};
pub use layout::{FoundryToml, ProjectResolver};
pub use repair::FoundryRepair;
pub use scope::ScopeLoader;
pub use tools::{PathFence, ProjectTools};
pub use workspace::Workspace;
