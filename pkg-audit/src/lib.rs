//! pkg-audit: `pkg audit` machinery (SPEC.md "pkg-audit"; v11 §10.4,
//! §16.1, §16.4–16.5; v13 §7; v14 §4).
//!
//! Flat re-exports per SPEC: every public item is available at the
//! crate root.

pub mod advisories;
pub mod inode_scan;
pub mod supply_chain;

pub use advisories::*;
pub use inode_scan::*;
pub use supply_chain::*;
