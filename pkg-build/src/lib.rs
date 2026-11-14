//! pkg-build — source-build orchestration: artifact cache, recipes,
//! parallel execution, rebuild cascades, staging, glibc handling
//! (SPEC.md: "pkg-build").
//!
//! ## v1 changes
//!
//! - Initial wave-B implementation: `cache` (Issue 4.5 personal artifact
//!   cache), `recipe` (sandboxed phase runner), `cascade` (Issue 4.1
//!   link-interface rebuild-set computation), `orchestrator` (Issue 4.2
//!   DAG-parallel executor with token pool and cgroups), `staging`
//!   (§7.5 stage→activate, shared by normal transactions and Issue-5
//!   undo), `glibc` (v13 category-1 inline patches + major-bump
//!   staging).
//!
//! ## v2 changes
//!
//! - `repro` (§10.5.6 critical-set double-build + quarantine),
//!   `peak_rss` build-history recording (§7.9.3), config-driven compiler
//!   wrapper (§7.9.6), and post-activation provenance/link-interface
//!   recording (§9.2.1/§7.7.2) in the orchestrator.
//!
//! ## v3 changes
//!
//! - `debug_shell` provides retained, non-canonical interactive workspaces;
//!   `hooks` provides typed, target-root maintenance with persistent retry state.
//!
//! ## v4 changes
//!
//! - `output` provides concise default output, optional verbose line prefixes,
//!   and transaction/package progress without changing scheduler semantics.
//!
//! Public modules are re-exported flat (`pub use x::*`).

pub mod build_system;
pub mod cache;
pub mod cascade;
pub mod debug_shell;
pub mod dkms;
pub mod glibc;
pub mod hooks;
pub mod orchestrator;
pub mod output;
pub mod recipe;
pub mod repro;
pub mod staging;

pub use cache::*;
pub use cascade::*;
pub use glibc::*;
pub use orchestrator::*;
pub use recipe::*;
pub use repro::*;
pub use staging::*;
