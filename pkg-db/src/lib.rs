//! `pkg-db` — the SQLite system database layer (SPEC: "pkg-db contracts").
//!
//! Modules: [`schema`] (DDL, migrations, row types), [`queries`] (all
//! SQL the rest of the workspace runs — no other crate writes SQL),
//! [`integrity`] (staged-glibc manifest verification), [`lock`] (the
//! system-wide flock with shared/exclusive split).
//!
//! Per SPEC, every public module is re-exported flat at the crate root,
//! so callers write `pkg_db::init_schema` / `pkg_db::SharedGuard` rather
//! than reaching through module paths.

pub mod integrity;
pub mod lock;
pub mod queries;
pub mod schema;

pub use integrity::*;
pub use lock::*;
pub use queries::*;
pub use schema::*;
