//! CRUD operations for `packages` and `dependencies` (§9), plus the
//! glibc major-bump pending-swap state (spec v13).
//!
//! This pass covers `packages`, `dependencies`, and the v3
//! `glibc_pending` singleton. `files`, `virtual_providers`,
//! `alternative_prefs`, and `conffile_state` aren't covered yet; their
//! row types already exist in `schema.rs`, waiting for CRUD functions
//! here.
//!
//! The glibc gate raises `PkgError::GlibcPending`, added in error-v3.rs
//! (spec v13 §4) — the interim `PkgError::Resolution` carrier noted in an
//! earlier draft of this file has been replaced by the real variant.
//!
//! ## v2 changes
//!
//! - [MAJOR] `reverse_dependents` now takes a `DependencyKind` filter:
//!   `pkg remove`'s removal protection queries with
//!   `DependencyKind::Runtime`, because build-time-only edges must not
//!   block removal of a package at runtime. Callers that want all edges
//!   (e.g. `pkg why` walking the full graph) call it once per kind.
//! - [MINOR] `autoremove_candidates` now uses a NULL-safe `NOT EXISTS`
//!   subquery instead of `NOT IN (SELECT depends_on_id ...)` — a single
//!   NULL in `depends_on_id` would have made `NOT IN` return no rows at
//!   all. (The schema also declares the column `NOT NULL`; this is
//!   defense in depth, plus `NOT EXISTS` can't silently change meaning
//!   if that constraint is ever relaxed.)
//! - [MINOR] `set_installed_explicitly` now checks the affected-row
//!   count and returns `PkgError::NotFound` when no row with the given
//!   id exists, instead of silently succeeding.
//! - [MINOR] Documented the caller-holds-lock precondition (§9.4) on
//!   all mutating queries.
//!
//! ## v3 changes
//!
//! - [MAJOR] New `glibc_pending` singleton access (spec v13):
//!   `PendingGlibc`, `set_pending_glibc`, `pending_glibc`,
//!   `clear_pending_glibc`. Presence of the row == a glibc major-bump
//!   swap is pending reboot; the `/var/lib/pkg/glibc-pending` flag file
//!   remains the boot-time source of truth, and the DB row exists so
//!   pre-build checks can fail fast.
//! - [MAJOR] `check_glibc_gate`: pre-build gate refusing to build a
//!   package that needs the pending major glibc before the reboot swap
//!   has happened.
//! - [MAJOR] `pkg_clean_glibc_guard`: refuses `pkg clean`'s removal of
//!   the staged glibc recovery copy when the running glibc doesn't match
//!   the DB-recorded active one (rescue-boot into the old glibc).
//! - [MINOR] `GLIBC_PENDING_PROTOCOL` constant, deliberately duplicated
//!   with pkg-init's `glibc-apply.rs` (see its comment).
//!
//! ## v4 changes
//!
//! - [MAJOR] `DependencyKind` is now three-valued (`Link/Runtime/Build`,
//!   schema-v4). `reverse_dependents`' kind filter widened from a single
//!   kind to a `&[DependencyKind]` set so removal protection can match
//!   `Link + Runtime` together — build edges never block removal. The
//!   canonical removal-protection call is the new `removal_blockers`
//!   convenience wrapper (kinds = [`REMOVAL_BLOCKING_KINDS`]); callers
//!   that want other slices of the graph (e.g. `pkg why`) still pass
//!   their own kind list to `reverse_dependents`.
//! - [MAJOR] `PackageRow` gains `opt_tag` (schema-v4 `packages`
//!   column). `insert_package` keeps its signature and inserts the
//!   all-default (`''`) tag; the new `insert_package_with_opt_tag` and
//!   `find_package_by_identity_opt` are the option-aware entry points.
//!   (`PackageId` itself has no `opt_tag` yet — that arrives with
//!   pkg-core's `Identity` in the concurrent v14 wave.)
//! - [MAJOR] New-table CRUD (spec v14): `package_options`
//!   (`insert_package_option`, `package_options`,
//!   `delete_package_options`), `generations` (`record_generation`,
//!   `record_undo_generation`, `get_generation`, `list_generations`),
//!   `generation_packages` (`generation_package_set`), `provenance`
//!   (`insert_provenance`, `provenance_for_package`), `build_history`
//!   (`record_build`, `build_history_for`), `link_interface`
//!   (`insert_link_interface`, `link_interfaces_for_package`).
//! - [MAJOR] Generations (Issue 2): `record_generation` snapshots the
//!   current installed set into `generation_packages` inside the
//!   caller's transaction; `restorable` checks a generation's store
//!   dirs still exist on disk; `generations_in_cascade` /
//!   `packages_in_cascade` group the per-package transactions of one
//!   rebuild cascade (§4.2.5).
//! - [MAJOR] `build_eta` (§4.2.4): median of the last 5 successful
//!   builds of a package name.
//! - [MAJOR] `link_interface_hash(name, version)` (§4.1.1): the
//!   measured link-interface hash the rebuild cascade prunes on.
//! - [MINOR] `BuildRecord`/`BuildHistoryRow` gain `peak_rss`
//!   (KiB, nullable; §7.9.3); `record_build` writes it.
//! - [MINOR] All v3 functions (`glibc_pending` gate, `pkg clean` guard)
//!   are unchanged in behavior.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pkg_core::{AbiTag, Arch, Identity, OptTag, PackageId, PkgError, PkgResult, Track, Version};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, Value, ValueRef};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};

use crate::{
    BuildHistoryRow, DependencyKind, GenerationKind, GenerationPackageRow, GenerationRow,
    LinkInterfaceRow, PackageOptionRow, PackageRow, ProvenanceRow, SCHEMA_VERSION,
};

/// Pending-flag protocol version recorded in `glibc_pending.protocol`.
///
/// pkg-init's `glibc-apply.rs` defines the same constant and the two
/// must move together. The duplication across crates is deliberate:
/// pkg-init depends only on pkg-core, not on pkg-db, so it cannot
/// import this from here. Follow-up: move the constant to pkg-core so
/// both crates share a single definition.
pub const GLIBC_PENDING_PROTOCOL: u8 = 1;

// ---------------------------------------------------------------------
// SQL wrapper newtypes
// ---------------------------------------------------------------------

/// Wrapper types implementing `rusqlite`'s `ToSql`/`FromSql` for
/// `pkg-core`'s types.
///
/// `schema.rs` hit the orphan rule with `PkgError`: neither `PkgError`
/// nor `rusqlite::Error` belongs to `pkg-db`, and the "for" position in
/// that impl was pinned to `PkgError` specifically, since that's the
/// type this crate's functions have to return. There was no way around
/// it there — hence the explicit `.map_err(...)` at every call site.
///
/// Here, the situation looks the same at first — `Version` belongs to
/// `pkg-core`, `ToSql`/`FromSql` belong to `rusqlite`, neither is local —
/// but this time we get to *choose* what sits in the "for" position.
/// Nothing requires it to be `Version` itself. A small local wrapper type
/// is local to this crate by definition, so implementing the trait on
/// the wrapper instead satisfies the orphan rule cleanly, and unwrapping
/// it (`.0`) gets back to the real type immediately after. That freedom
/// to substitute a local stand-in is exactly what wasn't available for
/// `PkgError` — there, the "for" type was fixed by what the crate's
/// public API needed to return; here, it's just a conversion detail
/// internal to this file.
///
/// (Four near-identical wrapper types, each with the same two-impl
/// shape below, is exactly the kind of repetition a `macro_rules!` macro
/// exists to collapse — worth doing once that's a comfortable tool;
/// written out longhand for now.)
struct SqlVersion(Version);
struct SqlAbiTag(AbiTag);
struct SqlArch(Arch);
struct SqlTrack(Track);

impl ToSql for SqlVersion {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.0.to_string()))
    }
}

impl FromSql for SqlVersion {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse::<Version>()
            .map(SqlVersion)
            .map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

impl ToSql for SqlAbiTag {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.0.to_string()))
    }
}

impl FromSql for SqlAbiTag {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse::<AbiTag>()
            .map(SqlAbiTag)
            .map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

impl ToSql for SqlArch {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.0.to_string()))
    }
}

impl FromSql for SqlArch {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse::<Arch>()
            .map(SqlArch)
            .map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

impl ToSql for SqlTrack {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.0.to_string()))
    }
}

impl FromSql for SqlTrack {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse::<Track>()
            .map(SqlTrack)
            .map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

// ---------------------------------------------------------------------
// Row mapping
// ---------------------------------------------------------------------

/// Shared by every query that returns `packages` rows, so the column
/// list only needs to agree with `PackageRow`'s shape in one place.
/// Column access is by name (`row.get("name")`, not `row.get(0)`)
/// specifically so this doesn't silently break if a query's SELECT list
/// is ever reordered — the alternative, positional access, would still
/// compile and would fail only by quietly returning the wrong field's
/// value into the wrong struct field.
fn map_package_row(row: &Row) -> rusqlite::Result<PackageRow> {
    let version: SqlVersion = row.get("version")?;
    let abi_tag: SqlAbiTag = row.get("abi_tag")?;
    let arch: SqlArch = row.get("arch")?;
    let track: SqlTrack = row.get("track")?;
    let installed_explicitly: i64 = row.get("installed_explicitly")?;

    Ok(PackageRow {
        id: row.get("id")?,
        name: row.get("name")?,
        version: version.0,
        abi_tag: abi_tag.0,
        opt_tag: row.get("opt_tag")?,
        arch: arch.0,
        track: track.0,
        installed_at: row.get("installed_at")?,
        installed_explicitly: installed_explicitly != 0,
    })
}

const PACKAGE_COLUMNS: &str =
    "id, name, version, abi_tag, opt_tag, arch, track, installed_at, installed_explicitly";

// ---------------------------------------------------------------------
// packages
// ---------------------------------------------------------------------

/// Inserts a package with the all-default option selection
/// (`opt_tag = ''`) and returns its new row id.
///
/// Option-aware installs (spec v14, Issue 1) go through
/// [`insert_package_with_opt_tag`]; this kept-signature wrapper exists
/// for the pre-options callers (e.g. `integrity.rs::regenerate_orphans`,
/// which reconstructs rows whose option selection is recovered
/// separately into `package_options`).
///
/// Lock precondition (§9.4): mutating; the caller must hold the
/// exclusive `/var/lib/pkg/lock` for the enclosing critical section.
pub fn insert_package(
    conn: &Connection,
    id: &PackageId,
    track: Track,
    installed_at: i64,
    installed_explicitly: bool,
) -> PkgResult<i64> {
    insert_package_with_opt_tag(conn, id, "", track, installed_at, installed_explicitly)
}

/// Inserts a package with an explicit option-selection tag and returns
/// its new row id. `opt_tag` is the 8-hex truncated SHA-256 of the
/// canonical sorted `name=value,` list, or `""` for an all-default
/// selection (spec v14, Issue 1); it is part of the package's identity
/// — `UNIQUE(name, version, abi_tag, opt_tag, arch)`.
///
/// Lock precondition (§9.4): mutating; the caller must hold the
/// exclusive `/var/lib/pkg/lock` for the enclosing critical section.
pub fn insert_package_with_opt_tag(
    conn: &Connection,
    id: &PackageId,
    opt_tag: &str,
    track: Track,
    installed_at: i64,
    installed_explicitly: bool,
) -> PkgResult<i64> {
    conn.execute(
        "INSERT INTO packages (name, version, abi_tag, opt_tag, arch, track, installed_at, installed_explicitly)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id.name,
            SqlVersion(id.version.clone()),
            SqlAbiTag(id.abi_tag),
            opt_tag,
            SqlArch(id.arch),
            SqlTrack(track),
            installed_at,
            installed_explicitly as i64,
        ],
    )
    .map_err(|e| PkgError::Db(e.to_string()))?;
    Ok(conn.last_insert_rowid())
}

/// Looks up a package by its database row id.
pub fn get_package_by_id(conn: &Connection, row_id: i64) -> PkgResult<Option<PackageRow>> {
    conn.query_row(
        &format!("SELECT {PACKAGE_COLUMNS} FROM packages WHERE id = ?1"),
        [row_id],
        map_package_row,
    )
    .optional()
    .map_err(|e| PkgError::Db(e.to_string()))
}

/// Looks up a package by its full identity (§7.7) — name, version, ABI
/// tag, and architecture together. Used to check whether an exact build
/// is already installed before starting a transaction.
///
/// v4 note: this matches *without* `opt_tag`, so on an options-aware
/// system it can be ambiguous (the same name/version/abi/arch installed
/// with two different option selections). Pre-options callers
/// (integrity regeneration) only ever see `opt_tag = ''` rows, where
/// this stays exact; option-aware callers must use
/// [`find_package_by_identity_opt`], which matches the full v4 UNIQUE
/// key.
pub fn find_package_by_identity(
    conn: &Connection,
    id: &PackageId,
) -> PkgResult<Option<PackageRow>> {
    conn.query_row(
        &format!(
            "SELECT {PACKAGE_COLUMNS} FROM packages
             WHERE name = ?1 AND version = ?2 AND abi_tag = ?3 AND arch = ?4"
        ),
        params![
            id.name,
            SqlVersion(id.version.clone()),
            SqlAbiTag(id.abi_tag),
            SqlArch(id.arch),
        ],
        map_package_row,
    )
    .optional()
    .map_err(|e| PkgError::Db(e.to_string()))
}

/// Looks up a package by its full v4 identity (spec v14, Issue 1):
/// name, version, ABI tag, option-selection tag, and architecture
/// together — the `packages` table's full UNIQUE key.
pub fn find_package_by_identity_opt(
    conn: &Connection,
    id: &PackageId,
    opt_tag: &str,
) -> PkgResult<Option<PackageRow>> {
    conn.query_row(
        &format!(
            "SELECT {PACKAGE_COLUMNS} FROM packages
             WHERE name = ?1 AND version = ?2 AND abi_tag = ?3 AND opt_tag = ?4 AND arch = ?5"
        ),
        params![
            id.name,
            SqlVersion(id.version.clone()),
            SqlAbiTag(id.abi_tag),
            opt_tag,
            SqlArch(id.arch),
        ],
        map_package_row,
    )
    .optional()
    .map_err(|e| PkgError::Db(e.to_string()))
}

/// All installed packages, ordered by name.
pub fn list_installed_packages(conn: &Connection) -> PkgResult<Vec<PackageRow>> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {PACKAGE_COLUMNS} FROM packages ORDER BY name"
        ))
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let rows = stmt
        .query_map([], map_package_row)
        .map_err(|e| PkgError::Db(e.to_string()))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| PkgError::Db(e.to_string()))
}

/// Flips a package's `installed_explicitly` flag (§9.2, §16.6) — set
/// when a package that was previously present only as a dependency
/// becomes the direct target of `pkg install <name>`.
///
/// Returns `PkgError::NotFound` if no `packages` row with `row_id`
/// exists: a silent no-op here would let a caller believe a package was
/// marked explicit when it wasn't, corrupting later autoremove
/// decisions.
///
/// Lock precondition (§9.4): mutating; the caller must hold the
/// exclusive `/var/lib/pkg/lock` for the enclosing critical section.
pub fn set_installed_explicitly(
    conn: &Connection,
    row_id: i64,
    explicitly: bool,
) -> PkgResult<()> {
    let affected = conn
        .execute(
            "UPDATE packages SET installed_explicitly = ?1 WHERE id = ?2",
            params![explicitly as i64, row_id],
        )
        .map_err(|e| PkgError::Db(e.to_string()))?;
    if affected == 0 {
        return Err(PkgError::NotFound(format!(
            "no packages row with id {row_id}"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------
// dependencies
// ---------------------------------------------------------------------

/// Records that `package_id` depends on `depends_on_id`, as resolved and
/// installed — not the declared expression from the manifest (which may
/// have been an OR-group or a virtual), but the concrete edge that
/// resulted from resolving it.
///
/// Lock precondition (§9.4): mutating; the caller must hold the
/// exclusive `/var/lib/pkg/lock` for the enclosing critical section.
pub fn insert_dependency(
    conn: &Connection,
    package_id: i64,
    depends_on_id: i64,
    kind: DependencyKind,
) -> PkgResult<i64> {
    conn.execute(
        "INSERT INTO dependencies (package_id, depends_on_id, kind) VALUES (?1, ?2, ?3)",
        params![package_id, depends_on_id, kind.to_string()],
    )
    .map_err(|e| PkgError::Db(e.to_string()))?;
    Ok(conn.last_insert_rowid())
}

/// The id of the installed row for (`name`, `arch`), if any.
pub fn package_id_by_name_arch(
    conn: &Connection,
    name: &str,
    arch: Arch,
) -> PkgResult<Option<i64>> {
    conn.query_row(
        "SELECT id FROM packages WHERE name = ?1 AND arch = ?2",
        params![name, arch.to_string()],
        |row| row.get(0),
    )
    .optional()
    .map_err(|e| PkgError::Db(e.to_string()))
}

/// Records the resolved dependency edges of the installed package
/// (`name`, `arch`), idempotently: `INSERT OR IGNORE` against the
/// `UNIQUE(package_id, depends_on_id, kind)` constraint, so re-running
/// after an undo or a re-activation is a no-op, not a conflict.
///
/// Entries naming a package with no installed row under the same arch
/// are skipped — by the time activation records edges the resolver has
/// already guaranteed the dep is installed, so a missing row means the
/// edge points outside this arch's world (or races a removal), neither
/// of which should fail the enclosing transaction. Self-edges are
/// skipped as meaningless. Returns how many edges the package has.
///
/// `kind` is pkg-core's manifest-level `DependencyKind` — the single
/// source of truth for the vocabulary the `CHECK` constraint enforces
/// (this crate's own `schema::DependencyKind` mirrors it).
pub fn record_dependency_edges(
    conn: &Connection,
    name: &str,
    arch: Arch,
    deps: &[(String, pkg_core::DependencyKind)],
) -> PkgResult<usize> {
    let Some(pkg_id) = package_id_by_name_arch(conn, name, arch)? else {
        return Ok(0);
    };
    let mut count = 0;
    for (dep_name, kind) in deps {
        if dep_name == name {
            continue;
        }
        let Some(dep_id) = package_id_by_name_arch(conn, dep_name, arch)? else {
            continue;
        };
        conn.execute(
            "INSERT OR IGNORE INTO dependencies (package_id, depends_on_id, kind) VALUES (?1, ?2, ?3)",
            params![pkg_id, dep_id, kind.to_string()],
        )
        .map_err(|e| PkgError::Db(e.to_string()))?;
        count += 1;
    }
    Ok(count)
}

/// The dependency kinds that block removal: `Link` and `Runtime`.
/// `Build` edges are consumed during the build and never block removal
/// of the thing they built — see `removal_blockers`.
pub const REMOVAL_BLOCKING_KINDS: &[DependencyKind] =
    &[DependencyKind::Link, DependencyKind::Runtime];

/// Packages that depend on `package_id` via an edge whose kind is in
/// `kinds` — the set `pkg remove` (§16.6) checks before allowing a
/// removal, and what `pkg why`'s explanation (§5.5) walks to show a
/// dependency chain.
///
/// The kind filter is not optional decoration: it decides semantics.
/// Removal protection must query with [`REMOVAL_BLOCKING_KINDS`]
/// (`Link + Runtime` — the v4 default; see `removal_blockers`), because
/// a package that is only needed at *build* time (a `Build` edge) must
/// not block removing the thing it built — build dependencies are
/// consumed during the build, not linked to the artifact's runtime
/// lifetime. Conversely, tooling that wants the full picture (e.g.
/// `pkg why` explaining "needed to build X") passes
/// `&[DependencyKind::Build]` — or calls once per kind and merges.
///
/// v4 signature change: `kinds` is a slice (was a single
/// `DependencyKind` in v3) because the three-kind schema makes
/// "everything that blocks removal" a two-kind set rather than a single
/// kind. An empty slice matches nothing and returns an empty vec
/// without touching the database.
pub fn reverse_dependents(
    conn: &Connection,
    package_id: i64,
    kinds: &[DependencyKind],
) -> PkgResult<Vec<PackageRow>> {
    if kinds.is_empty() {
        return Ok(Vec::new());
    }
    // One placeholder per kind. Parameter count is data-dependent, so
    // the placeholders are built as text and the values bound via
    // `params_from_iter` — never the kind strings themselves as text,
    // which would be SQL injection if a kind value were ever
    // user-influenced.
    let placeholders = vec!["?"; kinds.len()].join(", ");
    let mut stmt = conn
        .prepare(&format!(
            "SELECT p.id, p.name, p.version, p.abi_tag, p.opt_tag, p.arch, p.track, p.installed_at, p.installed_explicitly
             FROM packages p
             JOIN dependencies d ON d.package_id = p.id
             WHERE d.depends_on_id = ? AND d.kind IN ({placeholders})
             ORDER BY p.name"
        ))
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let mut bind = Vec::with_capacity(kinds.len() + 1);
    bind.push(Value::Integer(package_id));
    bind.extend(kinds.iter().map(|k| Value::Text(k.to_string())));
    let rows = stmt
        .query_map(params_from_iter(bind), map_package_row)
        .map_err(|e| PkgError::Db(e.to_string()))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| PkgError::Db(e.to_string()))
}

/// The packages that block `pkg remove <package_id>` (§16.6): reverse
/// dependents through `Link` or `Runtime` edges. `Build` edges never
/// block removal — a build dependency is consumed during the build and
/// imposes no constraint on the artifact's runtime lifetime.
///
/// This is the canonical removal-protection query; `reverse_dependents`
/// with [`REMOVAL_BLOCKING_KINDS`] inlined, so no caller can
/// accidentally protect on the wrong kind set.
pub fn removal_blockers(conn: &Connection, package_id: i64) -> PkgResult<Vec<PackageRow>> {
    reverse_dependents(conn, package_id, REMOVAL_BLOCKING_KINDS)
}

/// The forward direction of [`reverse_dependents`]: the installed
/// packages that `package_id` itself depends on through edges of the
/// given kinds. The build sandbox walks this to bind the runtime
/// closure of a build-only dependency — a build tool (meson, python3)
/// runs during the build, so the libraries IT needs must be present
/// even though the package being built never links them. An empty
/// `kinds` slice matches nothing.
pub fn forward_dependencies(
    conn: &Connection,
    package_id: i64,
    kinds: &[DependencyKind],
) -> PkgResult<Vec<PackageRow>> {
    if kinds.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; kinds.len()].join(", ");
    let mut stmt = conn
        .prepare(&format!(
            "SELECT p.id, p.name, p.version, p.abi_tag, p.opt_tag, p.arch, p.track, p.installed_at, p.installed_explicitly
             FROM packages p
             JOIN dependencies d ON d.depends_on_id = p.id
             WHERE d.package_id = ? AND d.kind IN ({placeholders})
             ORDER BY p.name"
        ))
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let mut bind = Vec::with_capacity(kinds.len() + 1);
    bind.push(Value::Integer(package_id));
    bind.extend(kinds.iter().map(|k| Value::Text(k.to_string())));
    let rows = stmt
        .query_map(params_from_iter(bind), map_package_row)
        .map_err(|e| PkgError::Db(e.to_string()))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| PkgError::Db(e.to_string()))
}

/// Candidates for `pkg autoremove` (§16.6): packages with
/// `installed_explicitly = 0` that nothing currently depends on.
///
/// This is a direct-dependents check, not a full transitive closure. If
/// removing one candidate would itself orphan a second package (A, kept
/// alive only by candidate B, where B is itself only kept alive by
/// nothing), that second-order orphan won't show up in a single call —
/// it only becomes a candidate once B is actually removed and its
/// `dependencies` row is gone too. A real `pkg autoremove` driver calls
/// this in a loop, removing candidates and re-querying until a pass
/// finds nothing new (fixpoint), or does the equivalent with a recursive
/// query. This is the one-pass building block either approach is built
/// on; the fixpoint loop lives in the CLI driver, not here.
///
/// The `NOT EXISTS` form (rather than `NOT IN (SELECT depends_on_id
/// FROM dependencies)`) is deliberate: `NOT IN` against a subquery that
/// can yield NULL never matches anything, which would silently empty the
/// candidate set. `depends_on_id` is declared `NOT NULL` in the schema,
/// but `NOT EXISTS` keeps this query correct by construction regardless.
pub fn autoremove_candidates(conn: &Connection) -> PkgResult<Vec<PackageRow>> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {PACKAGE_COLUMNS} FROM packages
             WHERE installed_explicitly = 0
             AND NOT EXISTS (
                 SELECT 1 FROM dependencies d WHERE d.depends_on_id = packages.id
             )
             ORDER BY name"
        ))
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let rows = stmt
        .query_map([], map_package_row)
        .map_err(|e| PkgError::Db(e.to_string()))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| PkgError::Db(e.to_string()))
}

// ---------------------------------------------------------------------
// glibc_pending (spec v13)
// ---------------------------------------------------------------------

/// The single `glibc_pending` row: a glibc major-bump swap staged and
/// waiting for reboot. Its existence in the table is itself the pending
/// state — there is no "not pending" row.
///
/// This mirrors the `/var/lib/pkg/glibc-pending` flag file, which
/// remains the boot-time source of truth (the early-boot `glibc-apply`
/// runs before the database may be trustable). The DB row exists so
/// pre-build checks (`check_glibc_gate`) can fail fast from the
/// database alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingGlibc {
    /// Pending-flag protocol version — currently always
    /// `GLIBC_PENDING_PROTOCOL`.
    pub protocol: u8,
    /// The major-bump target being staged, e.g. `2.40`.
    pub target_version: Version,
    /// Manifest hash of `/usr/lib/pkg/.stage/glibc-active/`.
    pub staged_sha256: String,
    /// Hash of the glibc-apply binary at queue time, for drift
    /// detection: if the binary on disk at apply time doesn't match,
    /// something replaced it after the swap was queued.
    pub glibc_apply_sha256: String,
    /// Unix timestamp the swap was queued.
    pub created_at: i64,
}

fn map_pending_glibc_row(row: &Row) -> rusqlite::Result<PendingGlibc> {
    let protocol: i64 = row.get("protocol")?;
    let target_version: SqlVersion = row.get("target_version")?;

    Ok(PendingGlibc {
        protocol: u8::try_from(protocol).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Integer,
                Box::new(PkgError::Parse(format!(
                    "glibc_pending protocol {protocol} out of range"
                ))),
            )
        })?,
        target_version: target_version.0,
        staged_sha256: row.get("staged_sha256")?,
        glibc_apply_sha256: row.get("glibc_apply_sha256")?,
        created_at: row.get("created_at")?,
    })
}

/// Records (or replaces) the pending glibc major-bump swap.
///
/// INSERT OR REPLACE is deliberate: a second major bump queued while
/// one is already pending REPLACES it — latest-staged-wins, there is no
/// queue. Rebooting applies exactly one staged swap, the most recent.
///
/// Lock precondition (§9.4): mutating; the caller must hold the
/// exclusive `/var/lib/pkg/lock` for the enclosing critical section.
/// The caller is also responsible for writing the
/// `/var/lib/pkg/glibc-pending` flag file, and that write must happen
/// FIRST: the flag file is the boot-time source of truth, so if a crash
/// lands between the two writes, the flag must be the side that exists.
pub fn set_pending_glibc(conn: &Connection, pending: &PendingGlibc) -> PkgResult<()> {
    conn.execute(
        "INSERT OR REPLACE INTO glibc_pending
         (id, protocol, target_version, staged_sha256, glibc_apply_sha256, created_at)
         VALUES (1, ?1, ?2, ?3, ?4, ?5)",
        params![
            pending.protocol as i64,
            SqlVersion(pending.target_version.clone()),
            pending.staged_sha256,
            pending.glibc_apply_sha256,
            pending.created_at,
        ],
    )
    .map_err(|e| PkgError::Db(e.to_string()))?;
    Ok(())
}

/// Reads the pending glibc swap, or `None` when no major bump is
/// staged. Singleton table, so at most one row can exist.
pub fn pending_glibc(conn: &Connection) -> PkgResult<Option<PendingGlibc>> {
    conn.query_row(
        "SELECT id, protocol, target_version, staged_sha256, glibc_apply_sha256, created_at
         FROM glibc_pending WHERE id = 1",
        [],
        map_pending_glibc_row,
    )
    .optional()
    .map_err(|e| PkgError::Db(e.to_string()))
}

/// Clears the pending-swap row. Called after successful boot swap
/// verification — i.e. once the system has rebooted and `glibc-apply`
/// has confirmed the staged glibc is now the running one (at which
/// point the flag file has also been consumed).
///
/// Lock precondition (§9.4): mutating; the caller must hold the
/// exclusive `/var/lib/pkg/lock` for the enclosing critical section.
pub fn clear_pending_glibc(conn: &Connection) -> PkgResult<()> {
    conn.execute("DELETE FROM glibc_pending WHERE id = 1", [])
        .map_err(|e| PkgError::Db(e.to_string()))?;
    Ok(())
}

/// Pre-build gate (spec v13): refuses to build a package whose glibc
/// floor requires the pending major bump, because that glibc is staged
/// but not yet running — the build would produce binaries linked
/// against a glibc the current system can't execute.
///
/// Errors when a pending row exists and `required_floor` reaches the
/// pending target's release core (same release core, or anything
/// beyond it). Below the pending major, the running glibc still
/// satisfies the floor and the build may proceed.
///
/// Errors with `PkgError::GlibcPending` (error-v3) when a pending row
/// exists and `required_floor` reaches the pending target's release core
/// (same release core, or anything beyond it). Below the pending major,
/// the running glibc still satisfies the floor and the build may proceed.
pub fn check_glibc_gate(conn: &Connection, required_floor: &Version) -> Result<(), PkgError> {
    let Some(pending) = pending_glibc(conn)? else {
        return Ok(());
    };
    let target = &pending.target_version;
    let reaches_pending =
        required_floor.same_release_core(target) || required_floor > target;
    if reaches_pending {
        return Err(PkgError::GlibcPending {
            required: required_floor.to_string(),
            pending_target: target.to_string(),
        });
    }
    Ok(())
}

/// `pkg clean` guard (spec v13): refuses to let cleanup delete the
/// staged glibc recovery copy when the *running* glibc differs from the
/// DB-recorded active glibc — the signature of the system having been
/// booted into the old glibc via the rescue path. In that state the
/// staged copy is the only way back, and deleting it would brick the
/// recovery route.
///
/// Also errors when no `glibc` package row exists at all: the active
/// version can't be verified, so the safe answer is to refuse.
///
/// Version comparison happens in Rust on `Version` values — never in
/// SQL, where TEXT ordering is lexicographic and wrong.
pub fn pkg_clean_glibc_guard(conn: &Connection, running_version: &Version) -> Result<(), PkgError> {
    let glibc_rows: Vec<PackageRow> = list_installed_packages(conn)?
        .into_iter()
        .filter(|p| p.name == "glibc")
        .collect();
    let Some(active) = glibc_rows.first() else {
        return Err(PkgError::NotFound(format!(
            "no glibc package recorded in the database; cannot verify the running \
             glibc {running_version} against the active one — refusing pkg clean"
        )));
    };
    if &active.version != running_version {
        return Err(PkgError::Resolution(format!(
            "pkg clean refused: running glibc {running_version} differs from the active \
             recorded glibc {} (rescue boot into the old glibc?); the staged recovery \
             copy must not be deleted",
            active.version
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------
// package_options (spec v14, Issue 1)
// ---------------------------------------------------------------------

fn map_package_option_row(row: &Row) -> rusqlite::Result<PackageOptionRow> {
    let value: i64 = row.get("value")?;
    let abi_affecting: i64 = row.get("abi_affecting")?;
    Ok(PackageOptionRow {
        package_id: row.get("package_id")?,
        name: row.get("name")?,
        value: value != 0,
        abi_affecting: abi_affecting != 0,
    })
}

/// Records one option's assigned value for an installed package. The
/// caller records the FULL assignment (every declared option, not just
/// deltas from defaults) so `pkg db regenerate` can reconstruct the
/// identical rows from the store manifest.
///
/// Lock precondition (§9.4): mutating; the caller must hold the
/// exclusive `/var/lib/pkg/lock` for the enclosing critical section.
pub fn insert_package_option(
    conn: &Connection,
    package_id: i64,
    name: &str,
    value: bool,
    abi_affecting: bool,
) -> PkgResult<()> {
    conn.execute(
        "INSERT INTO package_options (package_id, name, value, abi_affecting)
         VALUES (?1, ?2, ?3, ?4)",
        params![package_id, name, value as i64, abi_affecting as i64],
    )
    .map_err(|e| PkgError::Db(e.to_string()))?;
    Ok(())
}

/// The full option assignment recorded for an installed package,
/// ordered by option name (the same canonical order the `opt_tag` hash
/// is computed over).
pub fn package_options(conn: &Connection, package_id: i64) -> PkgResult<Vec<PackageOptionRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT package_id, name, value, abi_affecting FROM package_options
             WHERE package_id = ?1 ORDER BY name",
        )
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let rows = stmt
        .query_map([package_id], map_package_option_row)
        .map_err(|e| PkgError::Db(e.to_string()))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| PkgError::Db(e.to_string()))
}

/// Drops all option rows for a package. Called when the package itself
/// is removed (the schema deliberately does not cascade this, since
/// package removal is a multi-step operation the caller drives).
///
/// Lock precondition (§9.4): mutating; the caller must hold the
/// exclusive `/var/lib/pkg/lock` for the enclosing critical section.
pub fn delete_package_options(conn: &Connection, package_id: i64) -> PkgResult<()> {
    conn.execute(
        "DELETE FROM package_options WHERE package_id = ?1",
        [package_id],
    )
    .map_err(|e| PkgError::Db(e.to_string()))?;
    Ok(())
}

// ---------------------------------------------------------------------
// generations + generation_packages (spec v14, Issue 2)
// ---------------------------------------------------------------------

fn map_generation_row(row: &Row) -> rusqlite::Result<GenerationRow> {
    let kind: String = row.get("kind")?;
    let kind = kind.parse::<GenerationKind>().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(e),
        )
    })?;
    Ok(GenerationRow {
        id: row.get("id")?,
        created_at: row.get("created_at")?,
        command: row.get("command")?,
        kind,
        undoes: row.get("undoes")?,
        schema_ver: row.get("schema_ver")?,
        cascade_id: row.get("cascade_id")?,
    })
}

const GENERATION_COLUMNS: &str = "id, created_at, command, kind, undoes, schema_ver, cascade_id";

fn map_generation_package_row(row: &Row) -> rusqlite::Result<GenerationPackageRow> {
    let version: SqlVersion = row.get("version")?;
    let abi_tag: SqlAbiTag = row.get("abi_tag")?;
    let arch: SqlArch = row.get("arch")?;
    let installed_explicitly: i64 = row.get("installed_explicitly")?;
    Ok(GenerationPackageRow {
        generation_id: row.get("generation_id")?,
        name: row.get("name")?,
        version: version.0,
        abi_tag: abi_tag.0,
        opt_tag: row.get("opt_tag")?,
        arch: arch.0,
        installed_explicitly: installed_explicitly != 0,
    })
}

const GENERATION_PACKAGE_COLUMNS: &str =
    "generation_id, name, version, abi_tag, opt_tag, arch, installed_explicitly";

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Shared body of `record_generation` / `record_undo_generation`.
fn record_generation_impl(
    conn: &Connection,
    command: &str,
    kind: GenerationKind,
    undoes: Option<i64>,
    cascade_id: Option<&str>,
) -> PkgResult<i64> {
    conn.execute(
        "INSERT INTO generations (created_at, command, kind, undoes, schema_ver, cascade_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            unix_now(),
            command,
            kind.to_string(),
            undoes,
            SCHEMA_VERSION,
            cascade_id,
        ],
    )
    .map_err(|e| PkgError::Db(e.to_string()))?;
    let gen_id = conn.last_insert_rowid();

    // Snapshot the CURRENT installed set. Plain INSERT .. SELECT over
    // `packages`, so the snapshot and the package-set change commit
    // atomically iff the caller runs this in the same transaction (see
    // the transaction requirement on `record_generation`).
    conn.execute(
        "INSERT INTO generation_packages
             (generation_id, name, version, abi_tag, opt_tag, arch, installed_explicitly)
         SELECT ?1, name, version, abi_tag, opt_tag, arch, installed_explicitly
         FROM packages",
        [gen_id],
    )
    .map_err(|e| PkgError::Db(e.to_string()))?;
    Ok(gen_id)
}

/// Records a generation for a successful activation (spec v14, Issue
/// 2): inserts the `generations` row and snapshots the current
/// installed package set into `generation_packages`. Returns the new
/// generation id (monotonic — it is the table's INTEGER PRIMARY KEY).
///
/// TRANSACTION REQUIREMENT (critical): the caller MUST invoke this
/// inside the same SQLite transaction that makes the package-set change
/// the generation describes — the generation record and the state it
/// snapshots are atomically consistent only by construction. It must
/// also be called AFTER the package-set changes have been applied (the
/// snapshot is the post-transaction set) and BEFORE commit. Callers
/// also hold the exclusive `/var/lib/pkg/lock` (§9.4) for the enclosing
/// critical section.
///
/// `command` is the invoking command line, e.g. `"pkg install ffmpeg
/// --opt x11=0"`. `cascade_id` groups the per-package generations of
/// one rebuild cascade (§4.2.5); `None` for standalone transactions.
pub fn record_generation(
    conn: &Connection,
    command: &str,
    kind: GenerationKind,
    cascade_id: Option<&str>,
) -> PkgResult<i64> {
    record_generation_impl(conn, command, kind, None, cascade_id)
}

/// `record_generation` for an undo activation: identical, except
/// `undoes` names the generation this one reverts. Same transaction
/// requirement as [`record_generation`].
pub fn record_undo_generation(
    conn: &Connection,
    command: &str,
    undoes: i64,
    cascade_id: Option<&str>,
) -> PkgResult<i64> {
    record_generation_impl(conn, command, GenerationKind::Undo, Some(undoes), cascade_id)
}

/// Looks up a generation by id.
pub fn get_generation(conn: &Connection, gen_id: i64) -> PkgResult<Option<GenerationRow>> {
    conn.query_row(
        &format!("SELECT {GENERATION_COLUMNS} FROM generations WHERE id = ?1"),
        [gen_id],
        map_generation_row,
    )
    .optional()
    .map_err(|e| PkgError::Db(e.to_string()))
}

/// All generations, oldest first (id is monotonic).
pub fn list_generations(conn: &Connection) -> PkgResult<Vec<GenerationRow>> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {GENERATION_COLUMNS} FROM generations ORDER BY id"
        ))
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let rows = stmt
        .query_map([], map_generation_row)
        .map_err(|e| PkgError::Db(e.to_string()))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| PkgError::Db(e.to_string()))
}

/// The package set a generation pins (its post-transaction installed
/// set), ordered by name then arch.
pub fn generation_package_set(
    conn: &Connection,
    gen_id: i64,
) -> PkgResult<Vec<GenerationPackageRow>> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {GENERATION_PACKAGE_COLUMNS} FROM generation_packages
             WHERE generation_id = ?1 ORDER BY name, arch"
        ))
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let rows = stmt
        .query_map([gen_id], map_generation_package_row)
        .map_err(|e| PkgError::Db(e.to_string()))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| PkgError::Db(e.to_string()))
}

/// Every store dir name pinned by ANY recorded generation for `arch`
/// (Issue 2 §3: generation membership pins store dirs against
/// `pkg clean`). `scan_store` treats these as recognized residents —
/// after an undo or upgrade, the superseded dirs are retained for
/// restorability, not orphaned.
pub fn generation_pinned_dir_names(
    conn: &Connection,
    arch: Arch,
) -> PkgResult<std::collections::HashSet<String>> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {GENERATION_PACKAGE_COLUMNS} FROM generation_packages
             WHERE arch = ?1"
        ))
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let rows = stmt
        .query_map([arch.to_string()], map_generation_package_row)
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let mut out = std::collections::HashSet::new();
    for row in rows {
        let row = row.map_err(|e| PkgError::Db(e.to_string()))?;
        let identity = Identity {
            name: row.name,
            version: row.version,
            abi_tag: row.abi_tag,
            opt_tag: row.opt_tag.parse().unwrap_or_else(|_| OptTag::none()),
            arch: row.arch,
        };
        out.insert(identity.store_dir_name());
    }
    Ok(out)
}

/// Whether every store dir a generation pins still exists under
/// `store_root` — the precondition for `pkg undo <gen>` (Issue 2):
/// because generation membership pins store dirs against `pkg clean`,
/// a retained generation's packages are *expected* present on disk, and
/// this verifies that expectation before the symlink swap begins.
///
/// Store dir naming follows the v14 contract (`pkg-core`'s
/// `Identity::store_dir_name`, being updated concurrently):
/// `name-version+ab<abi>`, plus `+p<opt>` when the option-selection tag
/// is non-empty. pkg-core's `Identity` type is not on this branch yet,
/// so the format is spelled out here; when it lands, this should
/// delegate to it.
///
/// `store_root` is the `.store/` root for the tree being checked. The
/// §6.1 multilib split gives i686 packages their own tree
/// (`lib32/pkg/`); a generation spanning both arches must be checked
/// once per arch root — this function deliberately takes one root and
/// checks the generation's full set against it, so callers with
/// multilib generations filter or call per arch.
pub fn restorable(conn: &Connection, store_root: &Path, gen_id: i64) -> PkgResult<bool> {
    if get_generation(conn, gen_id)?.is_none() {
        return Err(PkgError::NotFound(format!(
            "no generation with id {gen_id}"
        )));
    }
    for pkg in generation_package_set(conn, gen_id)? {
        // Store dir naming goes through Identity::store_dir_name (the
        // v14 contract: normalized version, +p<opt_tag> only when
        // non-default) — hand-rolled formatting here would drift from
        // what activation actually creates on disk.
        let identity = Identity {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
            abi_tag: pkg.abi_tag,
            opt_tag: pkg.opt_tag.parse().unwrap_or_else(|_| OptTag::none()),
            arch: pkg.arch,
        };
        if !store_root.join(identity.store_dir_name()).is_dir() {
            return Ok(false);
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------
// provenance (spec v14, §3.5)
// ---------------------------------------------------------------------

fn map_provenance_row(row: &Row) -> rusqlite::Result<ProvenanceRow> {
    Ok(ProvenanceRow {
        package_id: row.get("package_id")?,
        recipe_commit: row.get("recipe_commit")?,
        source_hashes: row.get("source_hashes")?,
        index_version: row.get("index_version")?,
        attestations: row.get("attestations")?,
        build_host: row.get("build_host")?,
        builder_version: row.get("builder_version")?,
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
        reproducibility: row.get("reproducibility")?,
    })
}

const PROVENANCE_COLUMNS: &str = "package_id, recipe_commit, source_hashes, index_version, \
     attestations, build_host, builder_version, started_at, finished_at, reproducibility";

/// Appends the provenance record for an installed artifact (§3.5). One
/// row per package (`package_id` is the PRIMARY KEY). A rebuild-in-place
/// (same identity, changed recipe) re-activates the same package row, so
/// the insert replaces: the newest build is the truth about the
/// installed artifact. (A same-content rebuild would produce the same
/// row anyway — the recorded fields are all inputs to the build.)
///
/// Lock precondition (§9.4): mutating; the caller must hold the
/// exclusive `/var/lib/pkg/lock` for the enclosing critical section.
pub fn insert_provenance(conn: &Connection, row: &ProvenanceRow) -> PkgResult<()> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO provenance ({PROVENANCE_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
        ),
        params![
            row.package_id,
            row.recipe_commit,
            row.source_hashes,
            row.index_version,
            row.attestations,
            row.build_host,
            row.builder_version,
            row.started_at,
            row.finished_at,
            row.reproducibility,
        ],
    )
    .map_err(|e| PkgError::Db(e.to_string()))?;
    Ok(())
}

/// The provenance record for an installed package, if one was recorded.
pub fn provenance_for_package(
    conn: &Connection,
    package_id: i64,
) -> PkgResult<Option<ProvenanceRow>> {
    conn.query_row(
        &format!("SELECT {PROVENANCE_COLUMNS} FROM provenance WHERE package_id = ?1"),
        [package_id],
        map_provenance_row,
    )
    .optional()
    .map_err(|e| PkgError::Db(e.to_string()))
}

// ---------------------------------------------------------------------
// build_history (spec v14, §4.2.4)
// ---------------------------------------------------------------------

/// One build-history entry as recorded by `record_build`. The duration
/// is a `Duration` at the API boundary and whole seconds
/// (`duration_s INTEGER`) in the table — sub-second precision buys
/// nothing for an ETA displayed to a human.
#[derive(Debug, Clone, PartialEq)]
pub struct BuildRecord {
    pub name: String,
    pub version: Version,
    pub abi_tag: AbiTag,
    pub option_set_hash: String,
    pub duration: Duration,
    pub success: bool,
    pub started_at: i64,
    /// Compiler-cache hit rate; `None` when the cache is disabled.
    pub cc_hit_rate: Option<f64>,
    /// Peak RSS of the build's child processes in KiB
    /// (`getrusage(RUSAGE_CHILDREN)` `ru_maxrss`); `None` when the
    /// measurement was unavailable. Note this is a process-wide running
    /// maximum across *all* reaped children — see the measurement site
    /// in pkg-build for why per-build attribution is approximate.
    pub peak_rss: Option<i64>,
    pub host_jobs: u32,
}

fn map_build_history_row(row: &Row) -> rusqlite::Result<BuildHistoryRow> {
    let version: SqlVersion = row.get("version")?;
    let abi_tag: SqlAbiTag = row.get("abi_tag")?;
    let success: i64 = row.get("success")?;
    let host_jobs: i64 = row.get("host_jobs")?;
    Ok(BuildHistoryRow {
        id: row.get("id")?,
        name: row.get("name")?,
        version: version.0,
        abi_tag: abi_tag.0,
        option_set_hash: row.get("option_set_hash")?,
        duration_s: row.get("duration_s")?,
        success: success != 0,
        started_at: row.get("started_at")?,
        cc_hit_rate: row.get("cc_hit_rate")?,
        peak_rss: row.get("peak_rss")?,
        host_jobs: u32::try_from(host_jobs).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Integer,
                Box::new(PkgError::Parse(format!(
                    "build_history host_jobs {host_jobs} out of range"
                ))),
            )
        })?,
    })
}

const BUILD_HISTORY_COLUMNS: &str = "id, name, version, abi_tag, option_set_hash, duration_s, \
     success, started_at, cc_hit_rate, peak_rss, host_jobs";

/// Records one finished (or failed) build (§4.2.4). Returns the new row
/// id. Historical rows only ever feed ETA display and scheduling order,
/// never correctness, so this is best-effort appending — but failures
/// here are still reported rather than swallowed, since a systematically
/// failing insert would silently degrade every ETA.
pub fn record_build(conn: &Connection, build: &BuildRecord) -> PkgResult<i64> {
    conn.execute(
        "INSERT INTO build_history
             (name, version, abi_tag, option_set_hash, duration_s, success, started_at, cc_hit_rate, peak_rss, host_jobs)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            build.name,
            SqlVersion(build.version.clone()),
            SqlAbiTag(build.abi_tag),
            build.option_set_hash,
            build.duration.as_secs() as i64,
            build.success as i64,
            build.started_at,
            build.cc_hit_rate,
            build.peak_rss,
            build.host_jobs as i64,
        ],
    )
    .map_err(|e| PkgError::Db(e.to_string()))?;
    Ok(conn.last_insert_rowid())
}

/// All recorded builds of `name`, most recent first.
pub fn build_history_for(conn: &Connection, name: &str) -> PkgResult<Vec<BuildHistoryRow>> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {BUILD_HISTORY_COLUMNS} FROM build_history
             WHERE name = ?1 ORDER BY started_at DESC, id DESC"
        ))
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let rows = stmt
        .query_map([name], map_build_history_row)
        .map_err(|e| PkgError::Db(e.to_string()))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| PkgError::Db(e.to_string()))
}

/// Estimated build time for `name` (§4.2.4): the median duration of its
/// last 5 successful builds, or `None` if it has never built
/// successfully (the caller's fallback is then the similar-source-size
/// median, which needs fetched-tarball bytes this layer doesn't track).
///
/// Any version counts — version bumps change duration far less than
/// packages differ from each other. The `(host_jobs_recorded /
/// host_jobs_now)^0.7` scaling from §4.2.4 is applied by the caller,
/// which knows the current job count; this returns the raw median.
pub fn build_eta(conn: &Connection, name: &str) -> PkgResult<Option<Duration>> {
    let mut stmt = conn
        .prepare(
            "SELECT duration_s FROM build_history
             WHERE name = ?1 AND success != 0
             ORDER BY started_at DESC, id DESC LIMIT 5",
        )
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let rows = stmt
        .query_map([name], |row| row.get::<_, i64>(0))
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let mut durations: Vec<i64> = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| PkgError::Db(e.to_string()))?;
    if durations.is_empty() {
        return Ok(None);
    }
    durations.sort_unstable();
    let n = durations.len();
    let median = if n % 2 == 1 {
        durations[n / 2]
    } else {
        (durations[n / 2 - 1] + durations[n / 2]) / 2
    };
    Ok(Some(Duration::from_secs(median.max(0) as u64)))
}

// ---------------------------------------------------------------------
// link_interface (spec v14, §4.1.1)
// ---------------------------------------------------------------------

fn map_link_interface_row(row: &Row) -> rusqlite::Result<LinkInterfaceRow> {
    Ok(LinkInterfaceRow {
        package_id: row.get("package_id")?,
        soname: row.get("soname")?,
        symbol_versions: row.get("symbol_versions")?,
        header_fingerprint: row.get("header_fingerprint")?,
        link_interface_hash: row.get("link_interface_hash")?,
    })
}

const LINK_INTERFACE_COLUMNS: &str =
    "package_id, soname, symbol_versions, header_fingerprint, link_interface_hash";

/// Records one exported soname's measured link interface for an
/// installed package (§4.1.1), as extracted by pkg-scanner after the
/// build. INSERT OR REPLACE: a same-package re-measure (e.g. after a
/// store repair re-scan) refreshes the row rather than erroring — the
/// interface is a measurement, latest wins.
///
/// Lock precondition (§9.4): mutating; the caller must hold the
/// exclusive `/var/lib/pkg/lock` for the enclosing critical section.
pub fn insert_link_interface(conn: &Connection, row: &LinkInterfaceRow) -> PkgResult<()> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO link_interface ({LINK_INTERFACE_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5)"
        ),
        params![
            row.package_id,
            row.soname,
            row.symbol_versions,
            row.header_fingerprint,
            row.link_interface_hash,
        ],
    )
    .map_err(|e| PkgError::Db(e.to_string()))?;
    Ok(())
}

/// All measured link-interface rows for an installed package, ordered
/// by soname.
pub fn link_interfaces_for_package(
    conn: &Connection,
    package_id: i64,
) -> PkgResult<Vec<LinkInterfaceRow>> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {LINK_INTERFACE_COLUMNS} FROM link_interface
             WHERE package_id = ?1 ORDER BY soname"
        ))
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let rows = stmt
        .query_map([package_id], map_link_interface_row)
        .map_err(|e| PkgError::Db(e.to_string()))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| PkgError::Db(e.to_string()))
}

/// The measured link-interface hash of an installed package version
/// (§4.1.1) — what the rebuild cascade compares to decide whether a
/// dependency rebuild prunes the cascade at that node. `None` when the
/// package version isn't installed or its interface was never measured.
///
/// A package can export several sonames (one `link_interface` row
/// each), but the hash is computed over the package's whole interface,
/// so every row for the same package carries the same value; the
/// `DISTINCT` + deterministic `ORDER BY` picks it without assuming that
/// invariant.
pub fn link_interface_hash(
    conn: &Connection,
    name: &str,
    version: &Version,
) -> PkgResult<Option<String>> {
    conn.query_row(
        "SELECT DISTINCT li.link_interface_hash
         FROM link_interface li
         JOIN packages p ON p.id = li.package_id
         WHERE p.name = ?1 AND p.version = ?2
         ORDER BY li.link_interface_hash LIMIT 1",
        params![name, SqlVersion(version.clone())],
        |row| row.get(0),
    )
    .optional()
    .map_err(|e| PkgError::Db(e.to_string()))
}

/// Which installed packages provide `soname`, per their recorded
/// `link_interface` rows. Feeds the build-failure provider hints: a
/// packager whose recipe underdeclared a dependency learns *which*
/// package to add, not just the soname that's missing.
pub fn packages_providing_soname(conn: &Connection, soname: &str) -> PkgResult<Vec<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT p.name FROM link_interface li
             JOIN packages p ON p.id = li.package_id
             WHERE li.soname = ?1
             ORDER BY p.name",
        )
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let rows = stmt
        .query_map(params![soname], |row| row.get(0))
        .map_err(|e| PkgError::Db(e.to_string()))?;
    rows.collect::<rusqlite::Result<Vec<String>>>()
        .map_err(|e| PkgError::Db(e.to_string()))
}

// ---------------------------------------------------------------------
// cascades (spec v14, §4.2.5)
// ---------------------------------------------------------------------

/// All generations recorded under one rebuild cascade, oldest first. A
/// cascade is N per-package transactions sharing a `cascade_id`, so
/// this is the set `pkg history` groups into one logical operation and
/// `pkg undo <cascade_id>` walks.
pub fn generations_in_cascade(
    conn: &Connection,
    cascade_id: &str,
) -> PkgResult<Vec<GenerationRow>> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {GENERATION_COLUMNS} FROM generations
             WHERE cascade_id = ?1 ORDER BY id"
        ))
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let rows = stmt
        .query_map([cascade_id], map_generation_row)
        .map_err(|e| PkgError::Db(e.to_string()))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| PkgError::Db(e.to_string()))
}

/// The union of every package-set entry recorded across a cascade's
/// generations, ordered by generation then name. Because each
/// generation snapshots the full installed set, a package activated
/// mid-cascade appears in that generation and all later ones — callers
/// wanting the cascade's *changed* packages diff consecutive
/// generations or take the first appearance per (name, arch).
pub fn packages_in_cascade(
    conn: &Connection,
    cascade_id: &str,
) -> PkgResult<Vec<GenerationPackageRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT gp.generation_id, gp.name, gp.version, gp.abi_tag, gp.opt_tag, gp.arch, gp.installed_explicitly
             FROM generation_packages gp
             JOIN generations g ON g.id = gp.generation_id
             WHERE g.cascade_id = ?1
             ORDER BY gp.generation_id, gp.name, gp.arch",
        )
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let rows = stmt
        .query_map([cascade_id], map_generation_package_row)
        .map_err(|e| PkgError::Db(e.to_string()))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| PkgError::Db(e.to_string()))
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_schema;

    fn fresh_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    fn test_id(name: &str) -> PackageId {
        PackageId {
            name: name.to_string(),
            version: "1.0.0".parse().unwrap(),
            abi_tag: AbiTag(1),
            arch: Arch::Amd64,
        }
    }

    fn test_pending() -> PendingGlibc {
        PendingGlibc {
            protocol: GLIBC_PENDING_PROTOCOL,
            target_version: "2.40".parse().unwrap(),
            staged_sha256: "aaaa".to_string(),
            glibc_apply_sha256: "bbbb".to_string(),
            created_at: 1_700_000_000,
        }
    }

    #[test]
    fn insert_and_get_package_round_trips_all_fields() {
        let conn = fresh_db();
        let id = test_id("openssl");
        let row_id = insert_package(&conn, &id, Track::Binary, 1_700_000_000, true).unwrap();

        let row = get_package_by_id(&conn, row_id).unwrap().unwrap();
        assert_eq!(row.name, "openssl");
        assert_eq!(row.version, id.version);
        assert_eq!(row.abi_tag, AbiTag(1));
        assert_eq!(row.arch, Arch::Amd64);
        assert_eq!(row.track, Track::Binary);
        assert_eq!(row.installed_at, 1_700_000_000);
        assert!(row.installed_explicitly);
    }

    #[test]
    fn sql_version_round_trips_patch_suffix() {
        let conn = fresh_db();
        let id = PackageId {
            name: "glibc".to_string(),
            version: "2.39-r1".parse().unwrap(),
            abi_tag: AbiTag(1),
            arch: Arch::Amd64,
        };
        let row_id = insert_package(&conn, &id, Track::Binary, 0, false).unwrap();
        let row = get_package_by_id(&conn, row_id).unwrap().unwrap();
        assert_eq!(row.version.to_string(), "2.39-r1");
    }

    #[test]
    fn get_package_by_id_returns_none_for_missing_row() {
        let conn = fresh_db();
        assert_eq!(get_package_by_id(&conn, 999).unwrap(), None);
    }

    #[test]
    fn find_package_by_identity_finds_exact_match() {
        let conn = fresh_db();
        let id = test_id("openssl");
        insert_package(&conn, &id, Track::Binary, 0, true).unwrap();
        assert!(find_package_by_identity(&conn, &id).unwrap().is_some());
    }

    #[test]
    fn find_package_by_identity_none_for_different_abi_tag() {
        let conn = fresh_db();
        let id = test_id("openssl");
        insert_package(&conn, &id, Track::Binary, 0, true).unwrap();

        let different_abi = PackageId {
            abi_tag: AbiTag(2),
            ..test_id("openssl")
        };
        assert_eq!(find_package_by_identity(&conn, &different_abi).unwrap(), None);
    }

    #[test]
    fn insert_package_rejects_duplicate_identity() {
        let conn = fresh_db();
        let id = test_id("openssl");
        insert_package(&conn, &id, Track::Binary, 0, true).unwrap();
        assert!(insert_package(&conn, &id, Track::Binary, 0, true).is_err());
    }

    #[test]
    fn list_installed_packages_orders_by_name() {
        let conn = fresh_db();
        insert_package(&conn, &test_id("zlib"), Track::Binary, 0, true).unwrap();
        insert_package(&conn, &test_id("curl"), Track::Binary, 0, true).unwrap();

        let all = list_installed_packages(&conn).unwrap();
        let names: Vec<&str> = all.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["curl", "zlib"]);
    }

    #[test]
    fn set_installed_explicitly_updates_flag() {
        let conn = fresh_db();
        let row_id = insert_package(&conn, &test_id("openssl"), Track::Binary, 0, false).unwrap();

        assert!(!get_package_by_id(&conn, row_id).unwrap().unwrap().installed_explicitly);
        set_installed_explicitly(&conn, row_id, true).unwrap();
        assert!(get_package_by_id(&conn, row_id).unwrap().unwrap().installed_explicitly);
    }

    #[test]
    fn set_installed_explicitly_errors_on_missing_row() {
        let conn = fresh_db();
        let err = set_installed_explicitly(&conn, 999, true).unwrap_err();
        assert!(
            matches!(err, PkgError::NotFound(_)),
            "expected NotFound for a nonexistent row, got {err:?}"
        );
    }

    #[test]
    fn reverse_dependents_finds_direct_dependents() {
        let conn = fresh_db();
        let curl_id = insert_package(&conn, &test_id("curl"), Track::Binary, 0, true).unwrap();
        let openssl_id = insert_package(&conn, &test_id("openssl"), Track::Binary, 0, false).unwrap();
        insert_dependency(&conn, curl_id, openssl_id, DependencyKind::Runtime).unwrap();

        let dependents =
            reverse_dependents(&conn, openssl_id, &[DependencyKind::Runtime]).unwrap();
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].name, "curl");
    }

    #[test]
    fn reverse_dependents_empty_when_nothing_depends_on_it() {
        let conn = fresh_db();
        let row_id = insert_package(&conn, &test_id("openssl"), Track::Binary, 0, true).unwrap();
        assert!(
            reverse_dependents(&conn, row_id, &[DependencyKind::Runtime])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn reverse_dependents_empty_kind_slice_matches_nothing() {
        let conn = fresh_db();
        let row_id = insert_package(&conn, &test_id("openssl"), Track::Binary, 0, true).unwrap();
        assert!(reverse_dependents(&conn, row_id, &[]).unwrap().is_empty());
    }

    #[test]
    fn reverse_dependents_filters_by_dependency_kind() {
        // curl links openssl; wget needs it at runtime (dlopen'd);
        // app needs openssl only at build time. Removal protection
        // (Link + Runtime) must see curl and wget but not app — a
        // build-time edge must not block `pkg remove openssl`.
        let conn = fresh_db();
        let curl_id = insert_package(&conn, &test_id("curl"), Track::Binary, 0, true).unwrap();
        let wget_id = insert_package(&conn, &test_id("wget"), Track::Binary, 0, true).unwrap();
        let app_id = insert_package(&conn, &test_id("app"), Track::Binary, 0, true).unwrap();
        let openssl_id = insert_package(&conn, &test_id("openssl"), Track::Binary, 0, false).unwrap();
        insert_dependency(&conn, curl_id, openssl_id, DependencyKind::Link).unwrap();
        insert_dependency(&conn, wget_id, openssl_id, DependencyKind::Runtime).unwrap();
        insert_dependency(&conn, app_id, openssl_id, DependencyKind::Build).unwrap();

        let blockers = removal_blockers(&conn, openssl_id).unwrap();
        let blocking: Vec<&str> = blockers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            blocking,
            vec!["curl", "wget"],
            "link + runtime edges block removal; build edges never do"
        );

        let build_deps = reverse_dependents(&conn, openssl_id, &[DependencyKind::Build]).unwrap();
        let build: Vec<&str> = build_deps.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(build, vec!["app"]);

        // A single-kind slice still works for callers slicing the graph
        // one kind at a time.
        let link_deps = reverse_dependents(&conn, openssl_id, &[DependencyKind::Link]).unwrap();
        let link_only: Vec<&str> = link_deps.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(link_only, vec!["curl"]);
    }

    #[test]
    fn forward_dependencies_walks_outgoing_edges_by_kind() {
        // python3 is a build dep of app; python3 itself links zlib and
        // needs openssl at runtime. The sandbox closure must find zlib
        // and openssl from python3, not from app.
        let conn = fresh_db();
        let app_id = insert_package(&conn, &test_id("app"), Track::Source, 0, true).unwrap();
        let py_id = insert_package(&conn, &test_id("python3"), Track::Source, 0, true).unwrap();
        let zlib_id = insert_package(&conn, &test_id("zlib"), Track::Source, 0, false).unwrap();
        let ssl_id = insert_package(&conn, &test_id("openssl"), Track::Source, 0, false).unwrap();
        insert_dependency(&conn, app_id, py_id, DependencyKind::Build).unwrap();
        insert_dependency(&conn, py_id, zlib_id, DependencyKind::Link).unwrap();
        insert_dependency(&conn, py_id, ssl_id, DependencyKind::Runtime).unwrap();

        let closure =
            forward_dependencies(&conn, py_id, REMOVAL_BLOCKING_KINDS).unwrap();
        let names: Vec<&str> = closure.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["openssl", "zlib"]);

        // The reverse direction sees app, not python3's deps.
        let rev = reverse_dependents(&conn, py_id, &[DependencyKind::Build]).unwrap();
        assert_eq!(rev.len(), 1);
        assert_eq!(rev[0].name, "app");

        assert!(forward_dependencies(&conn, py_id, &[]).unwrap().is_empty());
    }

    #[test]
    fn autoremove_excludes_explicit_and_still_depended_on() {
        let conn = fresh_db();
        let explicit_pkg = insert_package(&conn, &test_id("curl"), Track::Binary, 0, true).unwrap();
        insert_package(&conn, &test_id("old-lib"), Track::Binary, 0, false).unwrap(); // orphaned
        let still_needed =
            insert_package(&conn, &test_id("openssl"), Track::Binary, 0, false).unwrap();
        insert_dependency(&conn, explicit_pkg, still_needed, DependencyKind::Runtime).unwrap();

        let names: Vec<String> = autoremove_candidates(&conn)
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();

        assert_eq!(names, vec!["old-lib".to_string()]);
        assert!(!names.contains(&"curl".to_string()), "explicit installs are never candidates");
        assert!(!names.contains(&"openssl".to_string()), "still has a reverse dependent");
    }

    // ---- glibc_pending (v3) ----

    #[test]
    fn pending_glibc_is_none_before_set() {
        let conn = fresh_db();
        assert_eq!(pending_glibc(&conn).unwrap(), None);
    }

    #[test]
    fn set_and_read_pending_glibc_round_trips_all_fields() {
        let conn = fresh_db();
        let pending = test_pending();
        set_pending_glibc(&conn, &pending).unwrap();

        let read = pending_glibc(&conn).unwrap().unwrap();
        assert_eq!(read, pending);
    }

    #[test]
    fn set_pending_glibc_replaces_existing_row_latest_wins() {
        // A second major bump queued while one is pending REPLACES it —
        // no queue, no error.
        let conn = fresh_db();
        set_pending_glibc(&conn, &test_pending()).unwrap();

        let newer = PendingGlibc {
            target_version: "2.41".parse().unwrap(),
            created_at: 1_700_000_100,
            ..test_pending()
        };
        set_pending_glibc(&conn, &newer).unwrap();

        let read = pending_glibc(&conn).unwrap().unwrap();
        assert_eq!(read, newer, "latest staged bump must win");

        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM glibc_pending", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1, "still exactly one row — replace, not queue");
    }

    #[test]
    fn clear_pending_glibc_removes_row() {
        let conn = fresh_db();
        set_pending_glibc(&conn, &test_pending()).unwrap();
        assert!(pending_glibc(&conn).unwrap().is_some());

        clear_pending_glibc(&conn).unwrap();
        assert_eq!(pending_glibc(&conn).unwrap(), None);
    }

    #[test]
    fn clear_pending_glibc_is_noop_when_nothing_pending() {
        let conn = fresh_db();
        clear_pending_glibc(&conn).unwrap(); // must not error
    }

    #[test]
    fn check_glibc_gate_passes_when_nothing_pending() {
        let conn = fresh_db();
        check_glibc_gate(&conn, &"9.99".parse().unwrap()).unwrap();
    }

    #[test]
    fn check_glibc_gate_passes_below_pending_target() {
        let conn = fresh_db();
        set_pending_glibc(&conn, &test_pending()).unwrap(); // target 2.40
        check_glibc_gate(&conn, &"2.39".parse().unwrap()).unwrap();
    }

    #[test]
    fn check_glibc_gate_errors_at_or_above_pending_target() {
        let conn = fresh_db();
        set_pending_glibc(&conn, &test_pending()).unwrap(); // target 2.40

        // Same release core: needs exactly the pending glibc.
        let err = check_glibc_gate(&conn, &"2.40".parse().unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("requires glibc 2.40") && msg.contains("2.40 is staged pending reboot"),
            "gate message must name required and pending versions, got: {msg}"
        );
        // error-v3 exists now: the gate raises the real variant, not
        // the interim Resolution carrier this test asserted before.
        assert!(
            matches!(err, PkgError::GlibcPending { .. }),
            "gate must raise GlibcPending, got {err:?}"
        );

        // Above the pending target also needs (at least) the pending glibc.
        assert!(check_glibc_gate(&conn, &"2.41".parse().unwrap()).is_err());
    }

    #[test]
    fn pkg_clean_glibc_guard_passes_when_running_matches_active() {
        let conn = fresh_db();
        let glibc = PackageId {
            name: "glibc".to_string(),
            version: "2.39".parse().unwrap(),
            ..test_id("glibc")
        };
        insert_package(&conn, &glibc, Track::Binary, 0, false).unwrap();

        pkg_clean_glibc_guard(&conn, &"2.39".parse().unwrap()).unwrap();
    }

    #[test]
    fn pkg_clean_glibc_guard_refuses_on_version_mismatch() {
        // Running 2.38 but DB says 2.39 is active: rescue boot into the
        // old glibc — the staged recovery copy must survive `pkg clean`.
        let conn = fresh_db();
        let glibc = PackageId {
            name: "glibc".to_string(),
            version: "2.39".parse().unwrap(),
            ..test_id("glibc")
        };
        insert_package(&conn, &glibc, Track::Binary, 0, false).unwrap();

        let err = pkg_clean_glibc_guard(&conn, &"2.38".parse().unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("2.38") && msg.contains("2.39"),
            "message must name running and recorded versions, got: {msg}"
        );
    }

    #[test]
    fn pkg_clean_glibc_guard_refuses_when_no_glibc_recorded() {
        let conn = fresh_db();
        let err = pkg_clean_glibc_guard(&conn, &"2.39".parse().unwrap()).unwrap_err();
        assert!(
            err.to_string().contains("no glibc package recorded"),
            "got: {err}"
        );
    }

    #[test]
    fn glibc_pending_protocol_constant_is_current() {
        assert_eq!(GLIBC_PENDING_PROTOCOL, 1);
    }

    // ---- v4: opt_tag identity ----

    #[test]
    fn insert_package_with_opt_tag_allows_option_variants_to_coexist() {
        let conn = fresh_db();
        let id = test_id("ffmpeg");
        let default_id = insert_package(&conn, &id, Track::Source, 0, true).unwrap();
        let custom_id =
            insert_package_with_opt_tag(&conn, &id, "a1b2c3d4", Track::Source, 1, true).unwrap();

        let default_row = get_package_by_id(&conn, default_id).unwrap().unwrap();
        assert_eq!(default_row.opt_tag, "", "insert_package defaults to the all-default tag");
        let custom_row = get_package_by_id(&conn, custom_id).unwrap().unwrap();
        assert_eq!(custom_row.opt_tag, "a1b2c3d4");

        // Same tag again is a duplicate under the widened UNIQUE key.
        assert!(insert_package_with_opt_tag(&conn, &id, "a1b2c3d4", Track::Source, 2, true).is_err());
    }

    #[test]
    fn find_package_by_identity_opt_matches_full_v4_key() {
        let conn = fresh_db();
        let id = test_id("ffmpeg");
        insert_package_with_opt_tag(&conn, &id, "a1b2c3d4", Track::Source, 0, true).unwrap();

        assert!(find_package_by_identity_opt(&conn, &id, "a1b2c3d4").unwrap().is_some());
        assert_eq!(find_package_by_identity_opt(&conn, &id, "ffffffff").unwrap(), None);
        assert_eq!(find_package_by_identity_opt(&conn, &id, "").unwrap(), None);
    }

    // ---- v4: package_options ----

    #[test]
    fn package_options_round_trip_and_delete() {
        let conn = fresh_db();
        let pkg = insert_package(&conn, &test_id("ffmpeg"), Track::Source, 0, true).unwrap();

        insert_package_option(&conn, pkg, "x11", true, true).unwrap();
        insert_package_option(&conn, pkg, "pulseaudio", false, true).unwrap();

        let opts = package_options(&conn, pkg).unwrap();
        assert_eq!(opts.len(), 2);
        // Ordered by name for canonical (hash-order) reads.
        assert_eq!(opts[0].name, "pulseaudio");
        assert!(!opts[0].value);
        assert!(opts[0].abi_affecting);
        assert_eq!(opts[1].name, "x11");
        assert!(opts[1].value);

        delete_package_options(&conn, pkg).unwrap();
        assert!(package_options(&conn, pkg).unwrap().is_empty());
    }

    // ---- v4: generations ----

    #[test]
    fn record_generation_snapshots_installed_set() {
        let conn = fresh_db();
        let curl = insert_package(&conn, &test_id("curl"), Track::Source, 0, true).unwrap();
        insert_package_with_opt_tag(&conn, &test_id("ffmpeg"), "a1b2c3d4", Track::Source, 0, false)
            .unwrap();
        set_installed_explicitly(&conn, curl, true).unwrap();

        let gen = record_generation(&conn, "pkg install curl", GenerationKind::Install, None)
            .unwrap();
        assert!(gen > 0);

        let row = get_generation(&conn, gen).unwrap().unwrap();
        assert_eq!(row.command, "pkg install curl");
        assert_eq!(row.kind, GenerationKind::Install);
        assert_eq!(row.undoes, None);
        assert_eq!(row.schema_ver, SCHEMA_VERSION);
        assert_eq!(row.cascade_id, None);

        let set = generation_package_set(&conn, gen).unwrap();
        assert_eq!(set.len(), 2);
        let ffmpeg = set.iter().find(|p| p.name == "ffmpeg").unwrap();
        assert_eq!(ffmpeg.opt_tag, "a1b2c3d4");
        assert!(!ffmpeg.installed_explicitly);
        assert_eq!(ffmpeg.version, "1.0.0".parse().unwrap());
        let curl_row = set.iter().find(|p| p.name == "curl").unwrap();
        assert!(curl_row.installed_explicitly);

        // A later package-set change belongs to the NEXT generation,
        // not this one.
        insert_package(&conn, &test_id("zlib"), Track::Source, 1, false).unwrap();
        assert_eq!(generation_package_set(&conn, gen).unwrap().len(), 2);
    }

    #[test]
    fn record_undo_generation_links_the_generation_it_reverts() {
        let conn = fresh_db();
        insert_package(&conn, &test_id("curl"), Track::Source, 0, true).unwrap();
        let gen1 = record_generation(&conn, "pkg install curl", GenerationKind::Install, None)
            .unwrap();
        let gen2 = record_undo_generation(&conn, "pkg undo 1", gen1, None).unwrap();

        let undo = get_generation(&conn, gen2).unwrap().unwrap();
        assert_eq!(undo.kind, GenerationKind::Undo);
        assert_eq!(undo.undoes, Some(gen1));

        let gens = list_generations(&conn).unwrap();
        assert_eq!(gens.len(), 2);
        assert_eq!(gens[0].id, gen1);
        assert_eq!(gens[1].id, gen2);
    }

    #[test]
    fn record_generation_groups_cascade_id() {
        let conn = fresh_db();
        let g1 = record_generation(&conn, "pkg upgrade", GenerationKind::Upgrade, Some("cascade-1"))
            .unwrap();
        let g2 = record_generation(&conn, "pkg upgrade", GenerationKind::Upgrade, Some("cascade-1"))
            .unwrap();
        record_generation(&conn, "pkg install other", GenerationKind::Install, None).unwrap();

        let gens = generations_in_cascade(&conn, "cascade-1").unwrap();
        let ids: Vec<i64> = gens.iter().map(|g| g.id).collect();
        assert_eq!(ids, vec![g1, g2]);
    }

    #[test]
    fn restorable_checks_every_store_dir_exists() {
        let conn = fresh_db();
        insert_package(&conn, &test_id("curl"), Track::Source, 0, true).unwrap();
        insert_package_with_opt_tag(&conn, &test_id("ffmpeg"), "a1b2c3d4", Track::Source, 0, false)
            .unwrap();
        let gen = record_generation(&conn, "pkg install curl ffmpeg", GenerationKind::Install, None)
            .unwrap();

        let store = tempfile::tempdir().unwrap();
        // Nothing on disk yet: not restorable.
        assert!(!restorable(&conn, store.path(), gen).unwrap());

        // Store dir naming follows the v14 Identity::store_dir_name
        // contract: name-version+ab<abi>, plus +p<opt> for a non-empty
        // option tag — with the version in its normalized form ("1.0.0"
        // renders as "1").
        std::fs::create_dir(store.path().join("curl-1+ab1")).unwrap();
        assert!(
            !restorable(&conn, store.path(), gen).unwrap(),
            "every pinned store dir must exist, not just some"
        );

        std::fs::create_dir(store.path().join("ffmpeg-1+ab1+pa1b2c3d4")).unwrap();
        assert!(restorable(&conn, store.path(), gen).unwrap());
    }

    #[test]
    fn restorable_errors_for_unknown_generation() {
        let conn = fresh_db();
        let store = tempfile::tempdir().unwrap();
        let err = restorable(&conn, store.path(), 999).unwrap_err();
        assert!(matches!(err, PkgError::NotFound(_)), "got {err:?}");
    }

    // ---- v4: provenance ----

    fn test_provenance(package_id: i64) -> ProvenanceRow {
        ProvenanceRow {
            package_id,
            recipe_commit: "deadbeef".to_string(),
            source_hashes: r#"[{"url":"https://x/y.tar.gz","sha256":"abcd"}]"#.to_string(),
            index_version: 7,
            attestations: Some(r#"["fp1","fp2"]"#.to_string()),
            build_host: "builder1 linux 6.9".to_string(),
            builder_version: "pkg 0.14.0 profile 3".to_string(),
            started_at: 1_700_000_000,
            finished_at: 1_700_000_600,
            reproducibility: Some("self-match".to_string()),
        }
    }

    #[test]
    fn provenance_round_trips_all_fields() {
        let conn = fresh_db();
        let pkg = insert_package(&conn, &test_id("openssl"), Track::Source, 0, true).unwrap();

        assert_eq!(provenance_for_package(&conn, pkg).unwrap(), None);

        let row = test_provenance(pkg);
        insert_provenance(&conn, &row).unwrap();
        assert_eq!(provenance_for_package(&conn, pkg).unwrap(), Some(row.clone()));

        // Rebuild-in-place: a second insert for the same package
        // replaces the provenance row (a rebuilt package gets fresh
        // provenance, not a UNIQUE violation).
        let rebuilt = ProvenanceRow {
            recipe_commit: "cafebabe".to_string(),
            ..row.clone()
        };
        insert_provenance(&conn, &rebuilt).unwrap();
        assert_eq!(provenance_for_package(&conn, pkg).unwrap(), Some(rebuilt));
    }

    #[test]
    fn provenance_reproducibility_check_rejects_other_values() {
        let conn = fresh_db();
        let pkg = insert_package(&conn, &test_id("openssl"), Track::Source, 0, true).unwrap();
        let bad = ProvenanceRow {
            reproducibility: Some("maybe".to_string()),
            ..test_provenance(pkg)
        };
        assert!(insert_provenance(&conn, &bad).is_err());
    }

    // ---- v4: build_history / build_eta ----

    fn test_build(name: &str, duration_s: u64, success: bool, started_at: i64) -> BuildRecord {
        BuildRecord {
            name: name.to_string(),
            version: "1.0.0".parse().unwrap(),
            abi_tag: AbiTag(1),
            option_set_hash: "".to_string(),
            duration: Duration::from_secs(duration_s),
            success,
            started_at,
            cc_hit_rate: None,
            peak_rss: None,
            host_jobs: 8,
        }
    }

    #[test]
    fn record_build_and_history_round_trip() {
        let conn = fresh_db();
        let mut build = test_build("ffmpeg", 3600, true, 100);
        build.cc_hit_rate = Some(0.75);
        build.peak_rss = Some(4_194_304); // 4 GiB in KiB
        let id = record_build(&conn, &build).unwrap();
        assert!(id > 0);

        let history = build_history_for(&conn, "ffmpeg").unwrap();
        assert_eq!(history.len(), 1);
        let row = &history[0];
        assert_eq!(row.duration_s, 3600);
        assert!(row.success);
        assert_eq!(row.started_at, 100);
        assert_eq!(row.cc_hit_rate, Some(0.75));
        assert_eq!(row.peak_rss, Some(4_194_304));
        assert_eq!(row.host_jobs, 8);
    }

    #[test]
    fn record_build_peak_rss_defaults_to_none_when_unmeasured() {
        let conn = fresh_db();
        record_build(&conn, &test_build("zlib", 10, true, 1)).unwrap();
        let history = build_history_for(&conn, "zlib").unwrap();
        assert_eq!(history[0].peak_rss, None);
    }

    #[test]
    fn build_eta_is_none_without_successful_builds() {
        let conn = fresh_db();
        assert_eq!(build_eta(&conn, "ffmpeg").unwrap(), None);

        record_build(&conn, &test_build("ffmpeg", 3600, false, 100)).unwrap();
        assert_eq!(
            build_eta(&conn, "ffmpeg").unwrap(),
            None,
            "failed builds never feed the ETA"
        );
    }

    #[test]
    fn build_eta_is_median_of_last_5_successes() {
        let conn = fresh_db();
        // Six successes: the oldest must drop out of the 5-wide window.
        for (duration_s, started_at) in [
            (100u64, 1i64),
            (300, 2),
            (500, 3),
            (700, 4),
            (900, 5),
            (1100, 6),
        ] {
            record_build(&conn, &test_build("ffmpeg", duration_s, true, started_at)).unwrap();
        }
        // A later failure with an absurd duration must be ignored.
        record_build(&conn, &test_build("ffmpeg", 999_999, false, 7)).unwrap();
        // Another package's builds don't leak in.
        record_build(&conn, &test_build("zlib", 10, true, 8)).unwrap();

        assert_eq!(
            build_eta(&conn, "ffmpeg").unwrap(),
            Some(Duration::from_secs(700)),
            "median of the last 5 successes (300,500,700,900,1100)"
        );
    }

    #[test]
    fn build_eta_averages_middle_two_for_even_windows() {
        let conn = fresh_db();
        for (duration_s, started_at) in [(100u64, 1i64), (200, 2), (300, 3), (500, 4)] {
            record_build(&conn, &test_build("ffmpeg", duration_s, true, started_at)).unwrap();
        }
        assert_eq!(
            build_eta(&conn, "ffmpeg").unwrap(),
            Some(Duration::from_secs(250)),
            "median of (100,200,300,500) is the mean of 200 and 300"
        );
    }

    // ---- v4: link_interface ----

    fn test_link_interface(package_id: i64, soname: &str, hash: &str) -> LinkInterfaceRow {
        LinkInterfaceRow {
            package_id,
            soname: soname.to_string(),
            symbol_versions: "OPENSSL_3.0.0\nOPENSSL_3.0.13".to_string(),
            header_fingerprint: "hfp123".to_string(),
            link_interface_hash: hash.to_string(),
        }
    }

    #[test]
    fn packages_providing_soname_maps_back_to_package_names() {
        let conn = fresh_db();
        let z = insert_package(&conn, &test_id("zlib"), Track::Source, 0, true).unwrap();
        let ssl = insert_package(&conn, &test_id("openssl"), Track::Source, 0, true).unwrap();
        insert_link_interface(&conn, &test_link_interface(z, "libz.so.1", "hz")).unwrap();
        insert_link_interface(&conn, &test_link_interface(ssl, "libssl.so.3", "h1")).unwrap();

        assert_eq!(
            packages_providing_soname(&conn, "libz.so.1").unwrap(),
            vec!["zlib".to_string()]
        );
        assert!(packages_providing_soname(&conn, "libmystery.so.9")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn link_interface_insert_and_lookup_by_name_version() {
        let conn = fresh_db();
        let pkg = insert_package(&conn, &test_id("openssl"), Track::Source, 0, true).unwrap();
        insert_link_interface(&conn, &test_link_interface(pkg, "libssl.so.3", "h1")).unwrap();
        insert_link_interface(&conn, &test_link_interface(pkg, "libcrypto.so.3", "h1")).unwrap();

        let version: Version = "1.0.0".parse().unwrap();
        assert_eq!(
            link_interface_hash(&conn, "openssl", &version).unwrap(),
            Some("h1".to_string())
        );
        assert_eq!(link_interface_hash(&conn, "zlib", &version).unwrap(), None);

        let rows = link_interfaces_for_package(&conn, pkg).unwrap();
        let sonames: Vec<&str> = rows.iter().map(|r| r.soname.as_str()).collect();
        assert_eq!(sonames, vec!["libcrypto.so.3", "libssl.so.3"], "ordered by soname");
    }

    #[test]
    fn insert_link_interface_remeasured_soname_replaces() {
        let conn = fresh_db();
        let pkg = insert_package(&conn, &test_id("openssl"), Track::Source, 0, true).unwrap();
        insert_link_interface(&conn, &test_link_interface(pkg, "libssl.so.3", "h1")).unwrap();
        insert_link_interface(&conn, &test_link_interface(pkg, "libssl.so.3", "h2")).unwrap();

        let rows = link_interfaces_for_package(&conn, pkg).unwrap();
        assert_eq!(rows.len(), 1, "re-measure replaces, not duplicates");
        assert_eq!(rows[0].link_interface_hash, "h2");
    }

    // ---- v4: cascades ----

    #[test]
    fn packages_in_cascade_unions_generation_sets() {
        let conn = fresh_db();
        insert_package(&conn, &test_id("openssl"), Track::Source, 0, false).unwrap();
        let g1 =
            record_generation(&conn, "pkg upgrade openssl", GenerationKind::Upgrade, Some("c-7"))
                .unwrap();
        insert_package(&conn, &test_id("curl"), Track::Source, 0, true).unwrap();
        let g2 = record_generation(&conn, "pkg upgrade curl", GenerationKind::Upgrade, Some("c-7"))
            .unwrap();
        // An unrelated standalone generation must not leak in.
        record_generation(&conn, "pkg install vim", GenerationKind::Install, None).unwrap();

        let cascade = packages_in_cascade(&conn, "c-7").unwrap();
        let gens: Vec<i64> = cascade.iter().map(|p| p.generation_id).collect();
        // Each generation's row set is the full package set at that
        // point: g1 = {openssl}, g2 = {curl, openssl}.
        assert_eq!(gens, vec![g1, g2, g2], "union of both generations' snapshots, ordered");
        let names: Vec<&str> = cascade.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["openssl", "curl", "openssl"]);
    }
}
