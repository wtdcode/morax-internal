//! `forge` invocation runtime shared by every knowdit front end: how to spawn `forge <args>`
//! (host binary, host binary inside a memory-capped cgroup v2 child, or docker), the
//! per-invocation handle whose `Drop` performs backend-specific cleanup, per-kind deadlines,
//! and the environment preflight. No project, database or LLM dependencies, so any crate that
//! only needs to run forge can depend on this one.
//!
//! Vendored from knowdit (<https://github.com/abortfuzz/knowdit>, GPL-2.0-only).

#[cfg(target_os = "linux")]
mod cgroup;
mod forge;

pub use forge::{
    ForgeBackend, ForgeBackendKind, ForgeFeatures, ForgeOutput, ForgeRunner, ForgeTimeouts, RunKind,
};
