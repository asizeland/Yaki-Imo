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

use pkg_core::{AbiTag, Arch, PackageId, PkgError, PkgResult, Track, Version};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::{DependencyKind, PackageRow};

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
        arch: arch.0,
        track: track.0,
        installed_at: row.get("installed_at")?,
        installed_explicitly: installed_explicitly != 0,
    })
}

const PACKAGE_COLUMNS: &str =
    "id, name, version, abi_tag, arch, track, installed_at, installed_explicitly";

// ---------------------------------------------------------------------
// packages
// ---------------------------------------------------------------------

/// Inserts a package and returns its new row id.
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
    conn.execute(
        "INSERT INTO packages (name, version, abi_tag, arch, track, installed_at, installed_explicitly)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id.name,
            SqlVersion(id.version.clone()),
            SqlAbiTag(id.abi_tag),
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
/// tag, and architecture together, matching the `packages` table's
/// `UNIQUE` constraint. Used to check whether an exact build is already
/// installed before starting a transaction.
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

/// Packages that depend on `package_id` via an edge of the given
/// `kind` — the set `pkg remove` (§16.6) checks before allowing a
/// removal, and what `pkg why`'s explanation (§5.5) walks to show a
/// dependency chain.
///
/// The kind filter is not optional decoration: it decides semantics.
/// `pkg remove` must query with `DependencyKind::Runtime`, because a
/// package that is only needed at *build* time (a `Build` edge) must not
/// block removing the thing it built — build dependencies are consumed
/// during the build, not linked to the artifact's runtime lifetime.
/// Conversely, tooling that wants the full picture (e.g. `pkg why`
/// explaining "needed to build X") calls this with `DependencyKind::Build`
/// — or once per kind and merges.
pub fn reverse_dependents(
    conn: &Connection,
    package_id: i64,
    kind: DependencyKind,
) -> PkgResult<Vec<PackageRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT p.id, p.name, p.version, p.abi_tag, p.arch, p.track, p.installed_at, p.installed_explicitly
             FROM packages p
             JOIN dependencies d ON d.package_id = p.id
             WHERE d.depends_on_id = ?1 AND d.kind = ?2
             ORDER BY p.name",
        )
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let rows = stmt
        .query_map(params![package_id, kind.to_string()], map_package_row)
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

        let dependents = reverse_dependents(&conn, openssl_id, DependencyKind::Runtime).unwrap();
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].name, "curl");
    }

    #[test]
    fn reverse_dependents_empty_when_nothing_depends_on_it() {
        let conn = fresh_db();
        let row_id = insert_package(&conn, &test_id("openssl"), Track::Binary, 0, true).unwrap();
        assert!(reverse_dependents(&conn, row_id, DependencyKind::Runtime).unwrap().is_empty());
    }

    #[test]
    fn reverse_dependents_filters_by_dependency_kind() {
        // curl needs openssl at runtime; app needs openssl only at build
        // time. Removal protection (Runtime) must see curl but not app —
        // a build-time edge must not block `pkg remove openssl`.
        let conn = fresh_db();
        let curl_id = insert_package(&conn, &test_id("curl"), Track::Binary, 0, true).unwrap();
        let app_id = insert_package(&conn, &test_id("app"), Track::Binary, 0, true).unwrap();
        let openssl_id = insert_package(&conn, &test_id("openssl"), Track::Binary, 0, false).unwrap();
        insert_dependency(&conn, curl_id, openssl_id, DependencyKind::Runtime).unwrap();
        insert_dependency(&conn, app_id, openssl_id, DependencyKind::Build).unwrap();

        let runtime: Vec<&str> = reverse_dependents(&conn, openssl_id, DependencyKind::Runtime)
            .unwrap()
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(runtime, vec!["curl"], "build-time edges must not block removal");

        let build: Vec<&str> = reverse_dependents(&conn, openssl_id, DependencyKind::Build)
            .unwrap()
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(build, vec!["app"]);
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
        // TODO(error.rs v3): once PkgError::GlibcPending exists, assert
        // matches!(err, PkgError::GlibcPending { .. }) instead of the
        // interim Resolution variant.
        assert!(matches!(err, PkgError::Resolution(_)), "interim variant, got {err:?}");

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
}
