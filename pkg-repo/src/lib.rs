//! pkg-repo: repository trust chain, index sync, recipe manifests, and
//! source fetching (SPEC.md "pkg-repo"; v11 §10.5–10.6, v12 §2, v14 §4).
//!
//! Flat re-exports per SPEC: every public item is available at the
//! crate root.

pub mod fetch;
pub mod recipe_manifest;
pub mod sync;
pub mod trust;

pub use fetch::*;
pub use recipe_manifest::*;
pub use sync::*;
pub use trust::*;
