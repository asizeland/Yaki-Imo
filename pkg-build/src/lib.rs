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
//! Public modules are re-exported flat (`pub use x::*`).

pub mod cache;
pub mod cascade;
pub mod glibc;
pub mod orchestrator;
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
