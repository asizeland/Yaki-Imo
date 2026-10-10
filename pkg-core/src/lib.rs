//! `pkg-core` — the shared type layer at the bottom of the workspace's
//! dependency graph (SPEC: "pkg-core contracts").
//!
//! Types only, no business logic: identity types ([`package`]:
//! `Version`, `AbiTag`, `Arch`, `Track`, `PackageId`, `OptTag`,
//! `Identity`), the manifest format ([`manifest`]), dependency/virtual
//! declarations ([`r#virtual`]: `Dependency`, `VersionConstraint`,
//! `VirtualName`), and the workspace-wide error type ([`error`]:
//! `PkgError`, `PkgResult`).
//!
//! Per SPEC, every public module is re-exported flat at the crate root,
//! so the rest of the workspace writes `pkg_core::Version` rather than
//! `pkg_core::package::Version` — and so crate-internal cross-module
//! imports like `use crate::Version;` (in `r#virtual`) resolve through
//! these same re-exports.
//!
//! `virtual.rs` is declared as `r#virtual` because `virtual` is a
//! reserved keyword (see that module's own docs).

pub mod error;
pub mod manifest;
pub mod package;
pub mod r#virtual;

pub use error::*;
pub use manifest::*;
pub use package::*;
pub use r#virtual::*;
