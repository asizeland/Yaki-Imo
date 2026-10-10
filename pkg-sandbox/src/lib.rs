//! pkg-sandbox — sandbox profiles, sandboxed execution via `bwrap`, and
//! cgroup v2 resource limits (SPEC: pkg-sandbox).
//!
//! Public modules are re-exported flat (`pub use x::*`).

pub mod cgroups;
pub mod profile;
pub mod run;

pub use cgroups::*;
pub use profile::*;
pub use run::*;
