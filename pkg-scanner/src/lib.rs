//! pkg-scanner — ELF/link-interface scanning, dlopen heuristics, build
//! gating, and system checks (SPEC: pkg-scanner).
//!
//! Public modules are re-exported flat (`pub use x::*`).

pub mod check;
pub mod elf;
pub mod gate;
pub mod heuristic;

pub use check::*;
pub use elf::*;
pub use gate::*;
pub use heuristic::*;
