//! Table definitions and schema versioning for the package database (§9.2).
//!
//! Every fallible `rusqlite` call here ends in `.map_err(|e|
//! PkgError::Db(e.to_string()))` rather than a bare `?`. That's not a
//! style choice — it's the orphan rule. `PkgError` belongs to `pkg-core`;
//! `rusqlite::Error` belongs to `rusqlite`. Neither is local to this
//! crate, so `pkg-db` isn't allowed to write `impl From<rusqlite::Error>
//! for PkgError` here — only a crate that owns one of the two types can
//! add a new trait impl connecting them, and `pkg-db` owns neither. This
//! is different from `error.rs`'s `impl From<io::Error> for PkgError`,
//! which compiles fine precisely because that impl lives inside
//! `pkg-core`, the crate where `PkgError` itself is defined — there,
//! `PkgError` is local, and the same rule that blocks the impl here is
//! what allows it there.
//!
//! Row types mirror the table shapes below using `pkg-core`'s proper
//! types (`Version`, `AbiTag`, `Arch`, `Track`) rather than raw SQL
//! column types — the same "raw wire shape vs. domain type" split
//! `manifest.rs` used for TOML. Converting between the two is
//! `queries.rs`'s job, not this file's; `schema.rs` only defines what a
//! row *means*.
//!
//! ## v2 changes
//!
//! - [MINOR] `schema_meta` now enforces a singleton row
//!   (`id INTEGER PRIMARY KEY CHECK (id = 1)`), and
//!   `current_schema_version` selects with `ORDER BY version DESC LIMIT 1`
//!   so it stays correct even if a future migration path briefly leaves
//!   multiple rows present.
//! - [MINOR] `dependencies.depends_on_id` already carried `NOT NULL`;
//!   kept and now paired with the `NOT EXISTS` rewrite in `queries.rs`
//!   (`autoremove_candidates`), which is NULL-safe regardless.
//! - [MINOR] Added `FromStr` for `DependencyKind` (inverse of its
//!   `Display`), with round-trip tests, and a `Track` Display/FromStr
//!   round-trip test pinned against the `CHECK(track IN ('binary',
//!   'source'))` constraint. The `'binary'` track value stays for schema
//!   forward-compat even though this deployment is source-only.
//! - [MINOR] Doc note: `files.path`'s global `UNIQUE` assumes file
//!   paths don't collide across arches, which holds because of the §6.1
//!   `lib32/pkg/` tree split; if that assumption changes, the constraint
//!   needs arch scope.
//! - [MINOR] Doc note: versions are stored as TEXT; SQL `ORDER BY` on
//!   version strings is lexicographic and wrong — version comparisons
//!   must happen in Rust via `pkg_core::Version`'s `Ord`.
//!
//! ## v3 changes
//!
//! - [MAJOR] `SCHEMA_VERSION` bumped from 1 to 2 (the first actual schema
//!   change under an existing installation — see the migration note on
//!   `SCHEMA_VERSION` below).
//! - [MAJOR] New singleton table `glibc_pending` (spec v13): presence of
//!   its row == a glibc major-bump swap is pending reboot. It mirrors the
//!   `/var/lib/pkg/glibc-pending` flag file, which remains the boot-time
//!   source of truth; the DB row exists so pre-build checks
//!   (`queries.rs::check_glibc_gate`) and cleanup guards
//!   (`queries.rs::pkg_clean_glibc_guard`) can fail fast without touching
//!   the filesystem.

use std::fmt;
use std::str::FromStr;

use pkg_core::{AbiTag, Arch, PkgError, PkgResult, Track, Version, VirtualName};
use rusqlite::Connection;

/// Current schema version.
///
/// Migration note (v3): this was bumped from 1 to 2 for the addition of
/// the `glibc_pending` singleton table. Because the change is purely
/// additive (a brand-new table no prior version referenced) and `DDL`
/// uses `CREATE TABLE IF NOT EXISTS`, running `init_schema` against an
/// existing v1 database is itself a sufficient migration: the new table
/// is created in place, existing tables are untouched, and `schema_meta`
/// is updated to record the new version (see the UPDATE in
/// `init_schema` below). The more general incremental-migration
/// machinery (checking `current_schema_version` and applying ordered
/// steps) is still worth building the first time a *non*-additive change
/// — a column alteration, a constraint tightening, a backfill — lands
/// on an existing installation.
pub const SCHEMA_VERSION: i64 = 2;

// NOTE on two deliberate storage choices in this DDL:
//
// * Versions are TEXT. SQLite would compare them lexicographically, so
//   any `ORDER BY version` (or `MAX(version)`, or a `WHERE version > ?`
//   predicate) is WRONG for version semantics — "10.0" sorts before
//   "2.0". Version ordering and comparison must always happen in Rust,
//   on `pkg_core::Version` values, after reading rows out. SQL may only
//   ever match versions by equality.
//
// * `files.path` is globally UNIQUE, with no arch in the key. That is
//   safe only because §6.1 splits multilib content into separate trees
//   (`.store/` for amd64 vs `lib32/pkg/` for i686), so two arches never
//   legitimately own the same installed path. If the layout assumption
//   ever changes (e.g. arches sharing a tree), this constraint must gain
//   arch scope or become UNIQUE(package_id, path).
const DDL: &str = "
CREATE TABLE IF NOT EXISTS schema_meta (
    -- Singleton table: exactly one row, ever. The CHECK pins the only
    -- legal id, and PRIMARY KEY makes a second insert impossible.
    id INTEGER PRIMARY KEY CHECK (id = 1),
    version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS packages (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    abi_tag TEXT NOT NULL,
    arch TEXT NOT NULL DEFAULT 'amd64',
    track TEXT NOT NULL CHECK(track IN ('binary', 'source')),
    installed_at INTEGER NOT NULL,
    installed_explicitly INTEGER NOT NULL DEFAULT 0,
    UNIQUE(name, version, abi_tag, arch)
);

CREATE TABLE IF NOT EXISTS dependencies (
    id INTEGER PRIMARY KEY,
    package_id INTEGER NOT NULL REFERENCES packages(id),
    depends_on_id INTEGER NOT NULL REFERENCES packages(id),
    kind TEXT NOT NULL CHECK(kind IN ('runtime', 'build')),
    UNIQUE(package_id, depends_on_id, kind)
);

CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    package_id INTEGER NOT NULL REFERENCES packages(id),
    path TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS virtual_providers (
    id INTEGER PRIMARY KEY,
    virtual_name TEXT NOT NULL,
    package_id INTEGER NOT NULL REFERENCES packages(id),
    UNIQUE(virtual_name, package_id)
);

CREATE TABLE IF NOT EXISTS alternative_prefs (
    virtual_name TEXT PRIMARY KEY,
    provider TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS conffile_state (
    path TEXT PRIMARY KEY,
    package_id INTEGER NOT NULL REFERENCES packages(id),
    original_hash TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS glibc_pending (
    -- Singleton table, same shape as schema_meta: presence of the row ==
    -- a glibc major-bump swap is pending reboot (spec v13). Mirrors the
    -- /var/lib/pkg/glibc-pending flag file, which remains the boot-time
    -- source of truth; this row exists so pre-build checks can fail fast
    -- from the DB alone.
    id INTEGER PRIMARY KEY CHECK (id = 1),
    protocol INTEGER NOT NULL,          -- pending-flag protocol version, currently 1
    target_version TEXT NOT NULL,       -- e.g. \"2.40\"
    staged_sha256 TEXT NOT NULL,        -- manifest hash of /usr/lib/pkg/.stage/glibc-active/
    glibc_apply_sha256 TEXT NOT NULL,   -- hash of the glibc-apply binary at queue time (drift detection)
    created_at INTEGER NOT NULL
);
";

/// Creates every table if it doesn't already exist, and stamps
/// `schema_meta` with `SCHEMA_VERSION`. Safe to call on every startup —
/// `CREATE TABLE IF NOT EXISTS` makes table creation idempotent.
///
/// On a fresh database the version stamp is inserted once (guarded by
/// `current_schema_version` returning `None`). On an existing v1
/// database, this call doubles as the v1→v2 migration: the new
/// `glibc_pending` table is created additively and the `schema_meta`
/// stamp is updated in place to `SCHEMA_VERSION`.
///
/// Also turns on `PRAGMA foreign_keys`. SQLite ships with foreign key
/// *enforcement* off by default, per connection, even though the syntax
/// to declare a foreign key (`REFERENCES packages(id)`, used throughout
/// the DDL above) is always accepted — a genuinely easy thing to miss,
/// since a schema with foreign keys declared but not enforced looks
/// identical to one that enforces them, right up until something
/// references a row that was never there.
pub fn init_schema(conn: &Connection) -> PkgResult<()> {
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(|e| PkgError::Db(e.to_string()))?;
    conn.execute_batch(DDL)
        .map_err(|e| PkgError::Db(e.to_string()))?;

    match current_schema_version(conn)? {
        None => {
            conn.execute(
                "INSERT INTO schema_meta (id, version) VALUES (1, ?1)",
                [SCHEMA_VERSION],
            )
            .map_err(|e| PkgError::Db(e.to_string()))?;
        }
        Some(v) if v < SCHEMA_VERSION => {
            // Additive migration path (v1 → v2): tables already created
            // by the DDL batch above; all that remains is re-stamping.
            conn.execute(
                "UPDATE schema_meta SET version = ?1 WHERE id = 1",
                [SCHEMA_VERSION],
            )
            .map_err(|e| PkgError::Db(e.to_string()))?;
        }
        Some(_) => {}
    }
    Ok(())
}

/// Reads the schema version stamped in `schema_meta`, or `None` if
/// `init_schema` has never fully run against this database.
///
/// "Never run" covers two genuinely different cases, both meaning
/// `None`: the table doesn't exist yet at all, or it exists but is still
/// empty (the brief window `init_schema` itself passes through, between
/// creating the table and inserting the version stamp into it). The
/// `sqlite_master` check below only catches the first — `query_row`
/// against an existing-but-empty table doesn't error with "no such
/// table," it errors with `QueryReturnedNoRows`, a distinct case that
/// needs its own arm rather than falling through to a generic failure.
///
/// `schema_meta` is a singleton by constraint (`CHECK (id = 1)`), so
/// there is only ever one row in practice — but the `ORDER BY version
/// DESC LIMIT 1` makes the query robust anyway: if a future migration
/// path ever leaves more than one row behind mid-step, "the current
/// version" unambiguously means the highest one rather than whichever
/// row SQLite happens to visit first.
pub fn current_schema_version(conn: &Connection) -> PkgResult<Option<i64>> {
    let table_exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_meta'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| PkgError::Db(e.to_string()))?;

    if table_exists == 0 {
        return Ok(None);
    }

    match conn.query_row(
        "SELECT version FROM schema_meta ORDER BY version DESC LIMIT 1",
        [],
        |row| row.get(0),
    ) {
        Ok(version) => Ok(Some(version)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(PkgError::Db(e.to_string())),
    }
}

// ---------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------

/// A row from `packages`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageRow {
    pub id: i64,
    pub name: String,
    pub version: Version,
    pub abi_tag: AbiTag,
    pub arch: Arch,
    pub track: Track,
    pub installed_at: i64,
    pub installed_explicitly: bool,
}

/// Which kind of edge a `dependencies` row represents — the installed,
/// resolved graph, not the declared one. `manifest.rs`'s `Dependency`
/// describes what a package *asked for* (possibly an OR-group or a
/// virtual); a `DependencyRow` records what was actually chosen to
/// satisfy that ask, as a concrete edge between two installed packages.
///
/// The distinction matters for removal: only `Runtime` edges make a
/// package required at runtime, so only they may block `pkg remove`
/// (see `queries.rs::reverse_dependents`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DependencyKind {
    Runtime,
    Build,
}

impl fmt::Display for DependencyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DependencyKind::Runtime => "runtime",
            DependencyKind::Build => "build",
        };
        write!(f, "{s}")
    }
}

impl FromStr for DependencyKind {
    type Err = PkgError;

    /// Inverse of `Display` — the two must stay in lockstep with the
    /// `CHECK(kind IN ('runtime', 'build'))` constraint in the DDL: every
    /// string the constraint accepts must parse, and every variant must
    /// display to a string the constraint accepts.
    fn from_str(s: &str) -> PkgResult<Self> {
        match s {
            "runtime" => Ok(DependencyKind::Runtime),
            "build" => Ok(DependencyKind::Build),
            other => Err(PkgError::Parse(format!(
                "unknown dependency kind `{other}` (expected `runtime` or `build`)"
            ))),
        }
    }
}

/// A row from `dependencies` — `package_id` requires `depends_on_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyRow {
    pub id: i64,
    pub package_id: i64,
    pub depends_on_id: i64,
    pub kind: DependencyKind,
}

/// A row from `files` — one file installed by `package_id`. Used by GC
/// (§12) and `pkg remove` to know what to clean up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub id: i64,
    pub package_id: i64,
    pub path: String,
}

/// A row from `virtual_providers` — `package_id` declares itself a
/// provider of `virtual_name` (§5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualProviderRow {
    pub id: i64,
    pub virtual_name: VirtualName,
    pub package_id: i64,
}

/// A row from `conffile_state`. Deliberately holds only `original_hash`
/// — the pristine as-shipped file's hash — rather than also storing a
/// separate `user_modified` flag. Whether the file has been modified is
/// something you can always recompute by rehashing the live file and
/// comparing; storing it a second time as its own field would just be
/// another instance of two things that are supposed to agree with
/// nothing forcing them to, the same category of hazard as the
/// redundant `dkms-module` boolean `manifest.rs` avoided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConffileRow {
    pub package_id: i64,
    pub path: String,
    pub original_hash: String,
}

// Note: `alternative_prefs` has no dedicated row type here. Its shape —
// a virtual name paired with a preferred provider — is exactly
// `pkg_core::AlternativePreference` (`virtual_pkg.rs`), so `queries.rs`
// can use that type directly rather than this crate inventing a
// duplicate with the same two fields.
//
// `glibc_pending` likewise has no row type here: its row type is
// `queries.rs::PendingGlibc`, which lives next to the queries that read
// and write it (its `target_version` is a `pkg_core::Version`, so the
// SqlVersion wrapper machinery lives there too).

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_db() -> Connection {
        Connection::open_in_memory().expect("in-memory sqlite connection")
    }

    #[test]
    fn init_schema_creates_all_tables() {
        let conn = fresh_db();
        init_schema(&conn).unwrap();

        let expected = [
            "schema_meta",
            "packages",
            "dependencies",
            "files",
            "virtual_providers",
            "alternative_prefs",
            "conffile_state",
            "glibc_pending",
        ];
        for table in expected {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "expected table `{table}` to exist");
        }
    }

    #[test]
    fn schema_version_is_none_before_init() {
        let conn = fresh_db();
        assert_eq!(current_schema_version(&conn).unwrap(), None);
    }

    #[test]
    fn schema_version_is_set_after_init() {
        let conn = fresh_db();
        init_schema(&conn).unwrap();
        assert_eq!(current_schema_version(&conn).unwrap(), Some(SCHEMA_VERSION));
    }

    #[test]
    fn schema_version_is_none_when_table_exists_but_is_empty() {
        // The exact gap that broke this function initially: table
        // created, nothing inserted yet. Reproduced directly, without
        // going through init_schema, so this stays pinned even if
        // init_schema's own ordering changes later.
        let conn = fresh_db();
        conn.execute_batch(
            "CREATE TABLE schema_meta (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                version INTEGER NOT NULL
            );",
        )
        .unwrap();
        assert_eq!(current_schema_version(&conn).unwrap(), None);
    }

    #[test]
    fn schema_meta_enforces_singleton() {
        let conn = fresh_db();
        init_schema(&conn).unwrap();

        // A second row — even with a different version — must be
        // rejected: the singleton CHECK pins id = 1 and the PRIMARY KEY
        // forbids a duplicate id.
        let second = conn.execute(
            "INSERT INTO schema_meta (id, version) VALUES (2, ?1)",
            [SCHEMA_VERSION + 1],
        );
        assert!(second.is_err(), "schema_meta must be a singleton table");

        let dup = conn.execute(
            "INSERT INTO schema_meta (id, version) VALUES (1, ?1)",
            [SCHEMA_VERSION + 1],
        );
        assert!(dup.is_err(), "duplicate singleton id must be rejected");
    }

    #[test]
    fn glibc_pending_enforces_singleton() {
        let conn = fresh_db();
        init_schema(&conn).unwrap();

        let insert = "INSERT INTO glibc_pending
                      (id, protocol, target_version, staged_sha256, glibc_apply_sha256, created_at)
                      VALUES (?1, 1, '2.40', 'aabb', 'ccdd', 0)";
        conn.execute(insert, [1]).unwrap();

        // Any other id violates the CHECK (id = 1) singleton pin.
        let other_id = conn.execute(insert, [2]);
        assert!(other_id.is_err(), "glibc_pending must be a singleton table");

        // The same id again violates the PRIMARY KEY — a plain INSERT
        // must never overwrite the pending row (queries.rs uses INSERT
        // OR REPLACE deliberately, as an explicit latest-staged-wins
        // decision, not something the schema permits by accident).
        let dup = conn.execute(insert, [1]);
        assert!(dup.is_err(), "duplicate singleton id must be rejected");
    }

    #[test]
    fn glibc_pending_columns_are_not_null() {
        let conn = fresh_db();
        init_schema(&conn).unwrap();

        // Every column except the pinned id is NOT NULL — a pending
        // swap with no recorded target or hashes would be worse than
        // no row, since the gate reads these to decide.
        let result = conn.execute(
            "INSERT INTO glibc_pending (id, protocol, target_version, staged_sha256, glibc_apply_sha256, created_at)
             VALUES (1, 1, NULL, 'aabb', 'ccdd', 0)",
            [],
        );
        assert!(result.is_err(), "NULL target_version must be rejected");
    }

    #[test]
    fn init_schema_migrates_v1_database_additively() {
        // Simulate an existing v1 installation: schema_meta stamped 1,
        // no glibc_pending table (v1 DDL didn't have it).
        let conn = fresh_db();
        conn.execute_batch(
            "CREATE TABLE schema_meta (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                version INTEGER NOT NULL
            );
            INSERT INTO schema_meta (id, version) VALUES (1, 1);",
        )
        .unwrap();

        init_schema(&conn).unwrap();

        assert_eq!(
            current_schema_version(&conn).unwrap(),
            Some(SCHEMA_VERSION),
            "v1 stamp must be updated to the current version"
        );
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'glibc_pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "migration must create glibc_pending");
    }

    #[test]
    fn current_schema_version_takes_highest_when_multiple_rows_exist() {
        // Defensive robustness: if a future migration ever leaves
        // several rows behind mid-step, "current version" is the max.
        // Recreated without the singleton constraint to simulate that
        // intermediate state.
        let conn = fresh_db();
        conn.execute_batch(
            "CREATE TABLE schema_meta (version INTEGER NOT NULL);
             INSERT INTO schema_meta (version) VALUES (1), (3), (2);",
        )
        .unwrap();
        assert_eq!(current_schema_version(&conn).unwrap(), Some(3));
    }

    #[test]
    fn init_schema_is_idempotent() {
        let conn = fresh_db();
        init_schema(&conn).unwrap();
        init_schema(&conn).unwrap(); // must not error, must not duplicate the stamp

        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_meta", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn foreign_keys_pragma_is_actually_on() {
        let conn = fresh_db();
        init_schema(&conn).unwrap();
        let enabled: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(enabled, 1);
    }

    #[test]
    fn packages_unique_constraint_is_enforced() {
        let conn = fresh_db();
        init_schema(&conn).unwrap();

        let insert = "INSERT INTO packages (name, version, abi_tag, arch, track, installed_at)
                      VALUES ('openssl', '3.0.13', 'ab1', 'amd64', 'binary', 0)";
        conn.execute(insert, []).unwrap();

        // Same (name, version, abi_tag, arch) again — should be rejected.
        let result = conn.execute(insert, []);
        assert!(result.is_err());
    }

    #[test]
    fn dependencies_foreign_key_is_enforced() {
        let conn = fresh_db();
        init_schema(&conn).unwrap();

        // No package with id 999 exists — this must fail now that
        // foreign_keys is actually on, proving the pragma isn't just
        // set but doing something.
        let result = conn.execute(
            "INSERT INTO dependencies (package_id, depends_on_id, kind)
             VALUES (1, 999, 'runtime')",
            [],
        );
        assert!(result.is_err());
    }

    #[test]
    fn dependencies_depends_on_id_is_not_null() {
        let conn = fresh_db();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO packages (name, version, abi_tag, arch, track, installed_at)
             VALUES ('openssl', '3.0.13', 'ab1', 'amd64', 'binary', 0)",
            [],
        )
        .unwrap();

        let result = conn.execute(
            "INSERT INTO dependencies (package_id, depends_on_id, kind)
             VALUES (1, NULL, 'runtime')",
            [],
        );
        assert!(result.is_err(), "NULL depends_on_id must be rejected");
    }

    #[test]
    fn dependency_kind_displays_lowercase() {
        assert_eq!(DependencyKind::Runtime.to_string(), "runtime");
        assert_eq!(DependencyKind::Build.to_string(), "build");
    }

    #[test]
    fn dependency_kind_display_fromstr_round_trip() {
        for kind in [DependencyKind::Runtime, DependencyKind::Build] {
            let parsed: DependencyKind = kind.to_string().parse().unwrap();
            assert_eq!(parsed, kind);
        }
        // And every string the SQL CHECK constraint accepts must parse.
        for s in ["runtime", "build"] {
            assert!(s.parse::<DependencyKind>().is_ok());
        }
    }

    #[test]
    fn dependency_kind_fromstr_rejects_unknown() {
        let err = "dev".parse::<DependencyKind>().unwrap_err();
        assert!(matches!(err, PkgError::Parse(_)));
    }

    #[test]
    fn track_display_fromstr_round_trip_matches_check_constraint() {
        // The DDL constraint is CHECK(track IN ('binary', 'source')).
        // Every Track variant must display to one of those strings and
        // parse back to itself. 'binary' stays even though this
        // deployment is source-only: it's schema forward-compat, and the
        // constraint already has to name it.
        for track in [Track::Binary, Track::Source] {
            let s = track.to_string();
            assert!(
                s == "binary" || s == "source",
                "Track display `{s}` not accepted by the CHECK constraint"
            );
            let parsed: Track = s.parse().unwrap();
            assert_eq!(parsed, track);
        }
    }
}
