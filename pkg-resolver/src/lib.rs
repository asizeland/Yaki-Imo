//! `pkg-resolver` — dependency resolution for the workspace (SPEC:
//! "pkg-resolver contracts").
//!
//! Layers, bottom to top: [`pubgrub`] holds the plain-solver
//! integration (`Catalog`, `resolve_with_constraints`, `ResolveError`
//! with its derivation tree); [`pin`] and [`profile`] supply the two
//! out-of-graph constraint sources (`PinSet`, `ProfileSet`,
//! `effective_constraints`) that `resolve_with_constraints` folds into
//! candidate filtering; [`graph`] layers virtuals and OR-groups
//! (§5.2) on top via synthetic-package materialization, with
//! preferred-provider bias from `alternatives.toml`; [`explain`]
//! renders failures and answers `pkg why`.
//!
//! Per SPEC, every public module is re-exported flat at the crate root
//! (`pkg_resolver::Catalog`, `pkg_resolver::PinSet`, ...).
//!
//! Note on naming: the module [`pubgrub`] has the same name as the
//! external `pubgrub` crate this one depends on. A crate-root module
//! shadows the extern prelude in Rust 2018 uniform paths, so modules
//! that need the *external* crate refer to it by its absolute path
//! `::pubgrub` — see the imports in `pubgrub.rs` and `explain.rs`.

pub mod explain;
pub mod graph;
pub mod pin;
pub mod profile;
pub mod pubgrub;

pub use explain::*;
pub use graph::*;
pub use pin::*;
pub use profile::*;
pub use pubgrub::*;
