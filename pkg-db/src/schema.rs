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
//!
//! ## v4 changes
//!
//! - [MAJOR] `SCHEMA_VERSION` bumped from 2 to 3 (spec v14). Unlike the
//!   v1→v2 bump, this one is not *fully* additive: new tables are
//!   additive, but `packages` and `dependencies` change shape, so
//!   `init_schema` now carries a real (table-rebuild) migration for
//!   existing v1/v2 databases — see `migrate_v1v2_to_v3` below.
//! - [MAJOR] `packages` gains `opt_tag TEXT NOT NULL DEFAULT ''` (spec
//!   v14, Issue 1 — option selections): the 8-hex truncated SHA-256 of
//!   the canonical sorted `name=value,` list, or the empty string for an
//!   all-default selection. The UNIQUE constraint becomes
//!   `(name, version, abi_tag, opt_tag, arch)` — two builds of the same
//!   package differing only in option selection coexist.
//! - [MAJOR] `dependencies.kind` CHECK becomes
//!   `('link', 'runtime', 'build')`, matching manifest-v3's
//!   `DependencyKind`. Migration mapping for existing rows: old
//!   `'runtime'` and `'build'` map through unchanged (the v1/v2 schema
//!   never had any other value — `'binary'` was never a kind, n/a), and
//!   anything unrecognized defaults to `'link'`.
//! - [MAJOR] `DependencyKind` gains the `Link` variant (now
//!   `Link/Runtime/Build`); `Link` is the `Default`, matching
//!   manifest-v3's default for `[dependencies]` entries without an
//!   explicit `kind`.
//! - [MAJOR] New tables (spec v14): `package_options` (full option
//!   assignment per installed package, so `pkg db regenerate` can
//!   reconstruct it), `generations` + `generation_packages` (Issue 2 —
//!   transaction generations; `generations.kind` CHECK includes
//!   `'undo'`, `undoes` links an undo generation to the one it reverts,
//!   `schema_ver` records the schema version the generation was recorded
//!   under, `cascade_id` groups the per-package transactions of one
//!   rebuild cascade), `provenance` (§3.5 supply-chain log),
//!   `build_history` (§4.2.4 ETA input), and `link_interface` (§4.1.1
//!   measured link interfaces driving rebuild-cascade pruning).
//! - [MINOR] `build_history` gains `peak_rss INTEGER` (KiB;
//!   §7.9.3/§9.2.1 list peak RSS among the recorded fields). Added in
//!   place without a version bump: v3 is still pre-release, and the
//!   table is created additively by `DDL` on both fresh and
//!   v1/v2-migrated databases, so one definition covers every path.
//! - [MINOR] `provenance.package_id` and `link_interface.package_id`
//!   gain `ON DELETE CASCADE`: both describe the *installed* artifact,
//!   and pkg-build now actually writes these tables — without the
//!   cascade, the next upgrade's `DELETE FROM packages` of the
//!   superseded row would fail FK enforcement.

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
///
/// Migration note (v4): bumped from 2 to 3. This bump is the
/// *non*-additive change the note above anticipated: `packages` gains a
/// column and a wider UNIQUE constraint, and `dependencies` tightens its
/// `kind` CHECK to the manifest-v3 three-kind set — neither is
/// expressible as `CREATE TABLE IF NOT EXISTS`. `init_schema` therefore
/// now runs `migrate_v1v2_to_v3` (table rebuild + copy) against any
/// database stamped below 3 before re-stamping. Fresh databases get the
/// v3 shapes directly from `DDL` and skip the rebuild entirely.
pub const SCHEMA_VERSION: i64 = 3;

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
const DDL: &str = r#"
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
    -- Option-selection tag (spec v14, Issue 1): 8-hex truncated SHA-256
    -- of the canonical sorted "name=value," list, or '' for an
    -- all-default selection. Part of the identity: two builds of the
    -- same package differing only in option selection coexist.
    opt_tag TEXT NOT NULL DEFAULT '',
    arch TEXT NOT NULL DEFAULT 'amd64',
    track TEXT NOT NULL CHECK(track IN ('binary', 'source')),
    installed_at INTEGER NOT NULL,
    installed_explicitly INTEGER NOT NULL DEFAULT 0,
    UNIQUE(name, version, abi_tag, opt_tag, arch)
);

CREATE TABLE IF NOT EXISTS dependencies (
    id INTEGER PRIMARY KEY,
    package_id INTEGER NOT NULL REFERENCES packages(id),
    depends_on_id INTEGER NOT NULL REFERENCES packages(id),
    -- manifest-v3 three-kind set. 'link' = linked against (drives
    -- abi_tag and removal protection), 'runtime' = needed at runtime
    -- but not linked (also blocks removal), 'build' = build-time only
    -- (never blocks removal). Migration from v1/v2: old 'runtime' and
    -- 'build' rows map through unchanged; anything else becomes 'link'
    -- ('binary' was never a kind value — n/a).
    kind TEXT NOT NULL CHECK(kind IN ('link', 'runtime', 'build')),
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

-- ---------------------------------------------------------------------
-- v4 tables (spec v14)
-- ---------------------------------------------------------------------

-- Full option assignment for an installed package (Issue 1): not just
-- the deltas from defaults, so `pkg db regenerate` can reconstruct the
-- row from the store manifest, which records the same assignment.
CREATE TABLE IF NOT EXISTS package_options (
    package_id INTEGER NOT NULL REFERENCES packages(id),
    name TEXT NOT NULL,
    value INTEGER NOT NULL CHECK (value IN (0, 1)),
    abi_affecting INTEGER NOT NULL,
    UNIQUE(package_id, name)
);

-- Transaction generations (Issue 2): one row per successful activation
-- of the §7.5 atomic core. The generation record and the package-set
-- change it describes are committed in the SAME transaction, so the two
-- are atomically consistent by construction.
CREATE TABLE IF NOT EXISTS generations (
    id INTEGER PRIMARY KEY,                 -- generation number, monotonic
    created_at INTEGER NOT NULL,            -- unix ts
    command TEXT NOT NULL,                  -- e.g. "pkg install ffmpeg --opt x11=0"
    kind TEXT NOT NULL CHECK (kind IN ('install', 'remove', 'upgrade', 'undo')),
    undoes INTEGER REFERENCES generations(id),  -- set only when kind = 'undo': the generation this one reverts
    schema_ver INTEGER NOT NULL,            -- SCHEMA_VERSION at record time
    cascade_id TEXT                         -- shared by the per-package transactions of one rebuild cascade (§4.2.5)
);

-- The post-transaction package set pinned by a generation. Membership
-- pins store dirs against `pkg clean` (Issue 2 §3).
CREATE TABLE IF NOT EXISTS generation_packages (
    generation_id INTEGER NOT NULL REFERENCES generations(id),
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    abi_tag TEXT NOT NULL,
    opt_tag TEXT NOT NULL DEFAULT '',
    arch TEXT NOT NULL DEFAULT 'amd64',
    installed_explicitly INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (generation_id, name, arch)
);

-- Append-only supply-chain provenance log (§3.5): who/what/when for
-- every installed artifact, so `pkg audit` can retrospectively answer
-- "which packages were built from a recipe commit later implicated in
-- an incident".
-- ON DELETE CASCADE: the row describes the *installed* artifact; when an
-- upgrade/removal deletes the packages row, its provenance goes with it
-- rather than blocking the delete under FK enforcement.
CREATE TABLE IF NOT EXISTS provenance (
    package_id INTEGER PRIMARY KEY REFERENCES packages(id) ON DELETE CASCADE,
    recipe_commit TEXT NOT NULL,
    source_hashes TEXT NOT NULL,        -- JSON: [{url, sha256}], exactly as verified at fetch
    index_version INTEGER NOT NULL,     -- which signed index authorized this build
    attestations TEXT,                  -- JSON: fingerprints present in the manifest entry
    build_host TEXT NOT NULL,           -- hostname + uname snapshot
    builder_version TEXT NOT NULL,      -- pkg version + sandbox profile version
    started_at INTEGER NOT NULL,
    finished_at INTEGER NOT NULL,
    reproducibility TEXT CHECK (reproducibility IN ('self-match', 'mismatch'))  -- NULL | §3.5a self-check result
);

-- Historical build durations (§4.2.4): ETA = median of the last 5
-- successful builds of the same name. Rows only ever feed display and
-- scheduling order, never correctness.
CREATE TABLE IF NOT EXISTS build_history (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    abi_tag TEXT NOT NULL,
    option_set_hash TEXT NOT NULL,
    duration_s INTEGER NOT NULL,
    success INTEGER NOT NULL,
    started_at INTEGER NOT NULL,        -- unix ts
    cc_hit_rate REAL,                   -- NULL if compiler cache disabled
    peak_rss INTEGER,                   -- peak RSS in KiB (getrusage ru_maxrss); NULL when unmeasurable
    host_jobs INTEGER NOT NULL
);

-- Measured link interface per installed package soname (§4.1.1): the
-- scanner measures sonames + symbol versions + public-header
-- fingerprints after every build; a dependency rebuild whose
-- link_interface_hash is unchanged prunes the rebuild cascade at that
-- node.
CREATE TABLE IF NOT EXISTS link_interface (
    -- ON DELETE CASCADE, same rationale as provenance: a measurement of
    -- the installed artifact, deleted together with its packages row.
    package_id INTEGER NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    soname TEXT NOT NULL,
    symbol_versions TEXT NOT NULL,
    header_fingerprint TEXT NOT NULL,
    link_interface_hash TEXT NOT NULL,
    UNIQUE(package_id, soname)
);
"#;

/// Creates every table if it doesn't already exist, and stamps
/// `schema_meta` with `SCHEMA_VERSION`. Safe to call on every startup —
/// `CREATE TABLE IF NOT EXISTS` makes table creation idempotent.
///
/// On a fresh database the version stamp is inserted once (guarded by
/// `current_schema_version` returning `None`). On an existing v1
/// database, this call doubles as the v1→v2 migration: the new
/// `glibc_pending` table is created additively and the `schema_meta`
/// stamp is updated in place to `SCHEMA_VERSION`. On an existing v1 or
/// v2 database it additionally runs the v3 migration
/// (`migrate_v1v2_to_v3`): the `packages`/`dependencies` table rebuild
/// that the new `opt_tag` column, wider UNIQUE constraint, and
/// three-kind CHECK require, all of which `CREATE TABLE IF NOT EXISTS`
/// alone cannot apply to tables that already exist.
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
            // v1 → v3 and v2 → v3: the v4 tables and `glibc_pending`
            // (for v1) were created additively by the DDL batch above,
            // but `packages` and `dependencies` need a rebuild to pick
            // up the new column/UNIQUE/CHECK shapes.
            if v < 3 {
                migrate_v1v2_to_v3(conn)?;
            }
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

/// Migrates a v1/v2 database to v3 by rebuilding the two tables whose
/// shapes changed.
///
/// Why a rebuild: SQLite can add a column (`ALTER TABLE ... ADD
/// COLUMN`), but it cannot alter a table's UNIQUE constraint or CHECK
/// in place, and v3 changes both — `packages.UNIQUE` gains `opt_tag`
/// and `dependencies.kind` gains `'link'`. The standard SQLite
/// procedure (rename old, create new, copy, drop) applies.
///
/// Kind mapping for the copy: old `'runtime'` and `'build'` rows map
/// through unchanged — the v1/v2 CHECK only ever allowed those two, and
/// `'binary'` was never a kind value (n/a). Anything unrecognized
/// defaults to `'link'`, the manifest-v3 default kind, rather than
/// failing the migration on data the old schema wouldn't have produced
/// anyway.
///
/// Foreign-key handling: `PRAGMA foreign_keys` is switched OFF for the
/// duration of the rebuild. That matters for two distinct reasons:
/// `ALTER TABLE ... RENAME TO` rewrites `REFERENCES` clauses in *other*
/// tables to point at the new name only when foreign keys are enabled
/// (with them off, `dependencies`' reference keeps saying `packages`,
/// which is what we want once the replacement table takes that name),
/// and the intermediate state — two `packages`-shaped tables, one of
/// them a rename target — never gets FK-checked. Afterwards,
/// `PRAGMA foreign_key_check` (run in Rust, where its result rows are
/// visible) verifies the copied data still satisfies every reference.
fn migrate_v1v2_to_v3(conn: &Connection) -> PkgResult<()> {
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF;
         BEGIN IMMEDIATE;

         ALTER TABLE packages RENAME TO packages_v2;
         CREATE TABLE packages (
             id INTEGER PRIMARY KEY,
             name TEXT NOT NULL,
             version TEXT NOT NULL,
             abi_tag TEXT NOT NULL,
             opt_tag TEXT NOT NULL DEFAULT '',
             arch TEXT NOT NULL DEFAULT 'amd64',
             track TEXT NOT NULL CHECK(track IN ('binary', 'source')),
             installed_at INTEGER NOT NULL,
             installed_explicitly INTEGER NOT NULL DEFAULT 0,
             UNIQUE(name, version, abi_tag, opt_tag, arch)
         );
         INSERT INTO packages
             (id, name, version, abi_tag, opt_tag, arch, track, installed_at, installed_explicitly)
         SELECT id, name, version, abi_tag, '', arch, track, installed_at, installed_explicitly
         FROM packages_v2;
         DROP TABLE packages_v2;

         ALTER TABLE dependencies RENAME TO dependencies_v2;
         CREATE TABLE dependencies (
             id INTEGER PRIMARY KEY,
             package_id INTEGER NOT NULL REFERENCES packages(id),
             depends_on_id INTEGER NOT NULL REFERENCES packages(id),
             kind TEXT NOT NULL CHECK(kind IN ('link', 'runtime', 'build')),
             UNIQUE(package_id, depends_on_id, kind)
         );
         INSERT INTO dependencies (id, package_id, depends_on_id, kind)
         SELECT id, package_id, depends_on_id,
                CASE kind
                    WHEN 'runtime' THEN 'runtime'
                    WHEN 'build' THEN 'build'
                    ELSE 'link'
                END
         FROM dependencies_v2;
         DROP TABLE dependencies_v2;

         COMMIT;
         PRAGMA foreign_keys = ON;",
    )
    .map_err(|e| PkgError::Db(e.to_string()))?;

    // `PRAGMA foreign_key_check` returns a row per violation; run it in
    // Rust (execute_batch would silently discard the rows). The copy
    // preserves ids verbatim, so any violation here means the source
    // database was already corrupt — say so rather than claiming a
    // clean migration.
    let violations: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .map_err(|e| PkgError::Db(e.to_string()))?;
    if violations > 0 {
        return Err(PkgError::Db(format!(
            "v3 migration left {violations} foreign-key violation(s); \
             the source database's references were already inconsistent"
        )));
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
///
/// `opt_tag` is a plain `String` here rather than `pkg_core::OptTag`
/// only because this crate predates the pkg-core wave that adds that
/// type (v14); the invariants are the same — the 8-hex truncated
/// SHA-256 of the canonical sorted `name=value,` list, or `""` for an
/// all-default selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageRow {
    pub id: i64,
    pub name: String,
    pub version: Version,
    pub abi_tag: AbiTag,
    pub opt_tag: String,
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
/// The distinction matters for removal: `Link` and `Runtime` edges
/// make a package required at runtime, so they block `pkg remove`,
/// while `Build` edges are consumed during the build and never block
/// removal (see `queries.rs::reverse_dependents` and
/// `queries.rs::removal_blockers`).
///
/// v4 (spec v14): three variants, matching manifest-v3's
/// `DependencyKind`; `Link` is the default kind for manifest
/// `[dependencies]` entries without an explicit `kind`, so it is the
/// `Default` here too. The v1/v2 schema's two-kind set had no `Link`:
/// migrated rows keep their old `'runtime'`/`'build'` values (see
/// `migrate_v1v2_to_v3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DependencyKind {
    /// Linked against: drives `abi_tag`, link-interface cascades, and
    /// removal protection.
    #[default]
    Link,
    /// Needed at runtime but not linked (dlopen'd plugins, data files,
    /// helper executables): blocks removal, but does not drive abi_tag.
    Runtime,
    /// Needed only to build: consumed during the build, never blocks
    /// removal of the thing it built.
    Build,
}

impl fmt::Display for DependencyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DependencyKind::Link => "link",
            DependencyKind::Runtime => "runtime",
            DependencyKind::Build => "build",
        };
        write!(f, "{s}")
    }
}

impl FromStr for DependencyKind {
    type Err = PkgError;

    /// Inverse of `Display` — the two must stay in lockstep with the
    /// `CHECK(kind IN ('link', 'runtime', 'build'))` constraint in the
    /// DDL: every string the constraint accepts must parse, and every
    /// variant must display to a string the constraint accepts.
    fn from_str(s: &str) -> PkgResult<Self> {
        match s {
            "link" => Ok(DependencyKind::Link),
            "runtime" => Ok(DependencyKind::Runtime),
            "build" => Ok(DependencyKind::Build),
            other => Err(PkgError::Parse(format!(
                "unknown dependency kind `{other}` (expected `link`, `runtime` or `build`)"
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

/// A row from `package_options` (spec v14, Issue 1): one option's
/// assigned value for an installed package. The full assignment is
/// recorded — not just deltas from defaults — so `pkg db regenerate`
/// can reconstruct it from the store manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageOptionRow {
    pub package_id: i64,
    pub name: String,
    pub value: bool,
    pub abi_affecting: bool,
}

/// The `generations.kind` discriminator (spec v14, Issue 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GenerationKind {
    Install,
    Remove,
    Upgrade,
    Undo,
}

impl fmt::Display for GenerationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            GenerationKind::Install => "install",
            GenerationKind::Remove => "remove",
            GenerationKind::Upgrade => "upgrade",
            GenerationKind::Undo => "undo",
        };
        write!(f, "{s}")
    }
}

impl FromStr for GenerationKind {
    type Err = PkgError;

    /// Inverse of `Display` — in lockstep with the
    /// `CHECK(kind IN ('install', 'remove', 'upgrade', 'undo'))`
    /// constraint, same discipline as `DependencyKind`.
    fn from_str(s: &str) -> PkgResult<Self> {
        match s {
            "install" => Ok(GenerationKind::Install),
            "remove" => Ok(GenerationKind::Remove),
            "upgrade" => Ok(GenerationKind::Upgrade),
            "undo" => Ok(GenerationKind::Undo),
            other => Err(PkgError::Parse(format!(
                "unknown generation kind `{other}` (expected `install`, `remove`, `upgrade` or `undo`)"
            ))),
        }
    }
}

/// A row from `generations` (spec v14, Issue 2): one successful
/// activation of the §7.5 atomic core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationRow {
    pub id: i64,
    pub created_at: i64,
    pub command: String,
    pub kind: GenerationKind,
    /// Set only when `kind == GenerationKind::Undo`: the generation
    /// this one reverts.
    pub undoes: Option<i64>,
    /// `SCHEMA_VERSION` at record time.
    pub schema_ver: i64,
    /// Shared across the per-package transactions of one rebuild
    /// cascade (§4.2.5), so `pkg history` can group them and `pkg undo`
    /// can roll a cascade back as a unit.
    pub cascade_id: Option<String>,
}

/// A row from `generation_packages`: one member of a generation's
/// post-transaction package set. Membership pins the store dir against
/// `pkg clean` (Issue 2 §3). `opt_tag` is a plain `String` for the same
/// reason as `PackageRow::opt_tag`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationPackageRow {
    pub generation_id: i64,
    pub name: String,
    pub version: Version,
    pub abi_tag: AbiTag,
    pub opt_tag: String,
    pub arch: Arch,
    pub installed_explicitly: bool,
}

/// A row from `provenance` (§3.5): the append-only supply-chain record
/// for one installed artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenanceRow {
    pub package_id: i64,
    /// Recipe repo commit the artifact was built from.
    pub recipe_commit: String,
    /// JSON `[{url, sha256}]`, exactly as verified at fetch time.
    pub source_hashes: String,
    /// Which signed index authorized this build.
    pub index_version: i64,
    /// JSON: attestation fingerprints present in the manifest entry.
    pub attestations: Option<String>,
    /// Hostname + uname snapshot of the build host.
    pub build_host: String,
    /// pkg version + sandbox profile version.
    pub builder_version: String,
    pub started_at: i64,
    pub finished_at: i64,
    /// §3.5a reproducibility self-check: `None`, `"self-match"`, or
    /// `"mismatch"`.
    pub reproducibility: Option<String>,
}

/// A row from `build_history` (§4.2.4). Durations are stored as whole
/// seconds (`duration_s INTEGER`); the public query API converts to/from
/// `std::time::Duration`.
#[derive(Debug, Clone, PartialEq)]
pub struct BuildHistoryRow {
    pub id: i64,
    pub name: String,
    pub version: Version,
    pub abi_tag: AbiTag,
    pub option_set_hash: String,
    pub duration_s: i64,
    pub success: bool,
    pub started_at: i64,
    /// Compiler-cache hit rate; `None` when the cache is disabled.
    pub cc_hit_rate: Option<f64>,
    /// Peak RSS of the build's child processes, in KiB
    /// (`getrusage(RUSAGE_CHILDREN)` `ru_maxrss`); `None` when the
    /// measurement was unavailable. §7.9.3 lists it as display/ETA input
    /// only.
    pub peak_rss: Option<i64>,
    pub host_jobs: u32,
}

/// A row from `link_interface` (§4.1.1): one exported soname of an
/// installed package, with the measured interface data the rebuild
/// cascade uses to prune itself when a dependency rebuild leaves the
/// consumed interface byte-identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkInterfaceRow {
    pub package_id: i64,
    pub soname: String,
    pub symbol_versions: String,
    pub header_fingerprint: String,
    pub link_interface_hash: String,
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
            // v4 tables (spec v14)
            "package_options",
            "generations",
            "generation_packages",
            "provenance",
            "build_history",
            "link_interface",
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

        // Same (name, version, abi_tag, opt_tag='', arch) again —
        // should be rejected.
        let result = conn.execute(insert, []);
        assert!(result.is_err());
    }

    #[test]
    fn packages_opt_tag_defaults_to_empty_and_widens_unique() {
        let conn = fresh_db();
        init_schema(&conn).unwrap();

        conn.execute(
            "INSERT INTO packages (name, version, abi_tag, arch, track, installed_at)
             VALUES ('ffmpeg', '7.0', 'ab1', 'amd64', 'source', 0)",
            [],
        )
        .unwrap();

        // opt_tag defaults to '' when not given.
        let opt_tag: String = conn
            .query_row(
                "SELECT opt_tag FROM packages WHERE name = 'ffmpeg'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(opt_tag, "");

        // v4 (spec v14): the UNIQUE key is (name, version, abi_tag,
        // opt_tag, arch), so a second build of the same package with a
        // different option selection coexists.
        conn.execute(
            "INSERT INTO packages (name, version, abi_tag, opt_tag, arch, track, installed_at)
             VALUES ('ffmpeg', '7.0', 'ab1', 'a1b2c3d4', 'amd64', 'source', 1)",
            [],
        )
        .unwrap();

        // ...but the same option selection again is a duplicate.
        let dup = conn.execute(
            "INSERT INTO packages (name, version, abi_tag, opt_tag, arch, track, installed_at)
             VALUES ('ffmpeg', '7.0', 'ab1', 'a1b2c3d4', 'amd64', 'source', 2)",
            [],
        );
        assert!(dup.is_err(), "duplicate (name, version, abi_tag, opt_tag, arch) must be rejected");
    }

    #[test]
    fn dependencies_kind_check_accepts_three_kinds() {
        let conn = fresh_db();
        init_schema(&conn).unwrap();
        for (name, version) in [("a", "1.0"), ("b", "1.0")] {
            conn.execute(
                "INSERT INTO packages (name, version, abi_tag, arch, track, installed_at)
                 VALUES (?1, ?2, 'ab1', 'amd64', 'source', 0)",
                rusqlite::params![name, version],
            )
            .unwrap();
        }
        for kind in ["link", "runtime", "build"] {
            conn.execute(
                "INSERT INTO dependencies (package_id, depends_on_id, kind)
                 VALUES (1, 2, ?1)",
                [kind],
            )
            .unwrap_or_else(|e| panic!("kind `{kind}` must be accepted: {e}"));
        }
        let rejected = conn.execute(
            "INSERT INTO dependencies (package_id, depends_on_id, kind)
             VALUES (1, 2, 'dev')",
            [],
        );
        assert!(rejected.is_err(), "kind 'dev' must be rejected by the CHECK");
    }

    #[test]
    fn generations_kind_check_includes_undo() {
        let conn = fresh_db();
        init_schema(&conn).unwrap();

        for kind in ["install", "remove", "upgrade", "undo"] {
            conn.execute(
                "INSERT INTO generations (created_at, command, kind, schema_ver)
                 VALUES (0, 'pkg test', ?1, ?2)",
                rusqlite::params![kind, SCHEMA_VERSION],
            )
            .unwrap_or_else(|e| panic!("generation kind `{kind}` must be accepted: {e}"));
        }

        let rejected = conn.execute(
            "INSERT INTO generations (created_at, command, kind, schema_ver)
             VALUES (0, 'pkg test', 'rollback', ?1)",
            [SCHEMA_VERSION],
        );
        assert!(rejected.is_err(), "kind 'rollback' must be rejected by the CHECK");
    }

    #[test]
    fn generations_undoes_and_cascade_id_are_nullable() {
        let conn = fresh_db();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO generations (created_at, command, kind, schema_ver)
             VALUES (0, 'pkg install a', 'install', ?1)",
            [SCHEMA_VERSION],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO generations (created_at, command, kind, undoes, schema_ver)
             VALUES (1, 'pkg undo', 'undo', 1, ?1)",
            [SCHEMA_VERSION],
        )
        .unwrap();

        let undoes: Option<i64> = conn
            .query_row("SELECT undoes FROM generations WHERE id = 2", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(undoes, Some(1));
    }

    #[test]
    fn generation_packages_pk_is_generation_name_arch() {
        let conn = fresh_db();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO generations (created_at, command, kind, schema_ver)
             VALUES (0, 'pkg install a', 'install', ?1)",
            [SCHEMA_VERSION],
        )
        .unwrap();

        let insert = "INSERT INTO generation_packages (generation_id, name, version, abi_tag)
                      VALUES (1, 'openssl', '3.0.13', 'ab1')";
        conn.execute(insert, []).unwrap();

        // opt_tag defaults to '' (same contract as packages.opt_tag).
        let opt_tag: String = conn
            .query_row(
                "SELECT opt_tag FROM generation_packages WHERE generation_id = 1 AND name = 'openssl'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(opt_tag, "");

        // Same (generation_id, name, arch) again — PK violation, even
        // with a different version or opt_tag: one entry per name+arch
        // per generation.
        let dup = conn.execute(insert, []);
        assert!(dup.is_err());

        // Same name on a different arch is a different PK — multilib
        // side-by-side install must fit.
        conn.execute(
            "INSERT INTO generation_packages (generation_id, name, version, abi_tag, arch)
             VALUES (1, 'openssl', '3.0.13', 'ab1', 'i686')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn init_schema_migrates_v2_database_with_data() {
        // Simulate a real v2 installation: old packages/dependencies
        // shapes (no opt_tag; two-kind CHECK), some data, stamp 2.
        let conn = fresh_db();
        conn.execute_batch(
            "CREATE TABLE schema_meta (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 version INTEGER NOT NULL
             );
             INSERT INTO schema_meta (id, version) VALUES (1, 2);
             CREATE TABLE packages (
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
             CREATE TABLE dependencies (
                 id INTEGER PRIMARY KEY,
                 package_id INTEGER NOT NULL REFERENCES packages(id),
                 depends_on_id INTEGER NOT NULL REFERENCES packages(id),
                 kind TEXT NOT NULL CHECK(kind IN ('runtime', 'build')),
                 UNIQUE(package_id, depends_on_id, kind)
             );
             INSERT INTO packages (id, name, version, abi_tag, arch, track, installed_at, installed_explicitly)
             VALUES (1, 'curl', '8.6.0', 'ab1', 'amd64', 'source', 42, 1),
                    (2, 'openssl', '3.0.13', 'ab1', 'amd64', 'source', 42, 0);
             INSERT INTO dependencies (id, package_id, depends_on_id, kind)
             VALUES (1, 1, 2, 'runtime');",
        )
        .unwrap();

        init_schema(&conn).unwrap();

        assert_eq!(current_schema_version(&conn).unwrap(), Some(SCHEMA_VERSION));

        // Data survived the rebuild; opt_tag backfilled to ''.
        let (name, opt_tag): (String, String) = conn
            .query_row("SELECT name, opt_tag FROM packages WHERE id = 1", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!((name.as_str(), opt_tag.as_str()), ("curl", ""));

        // Old kind values map through unchanged ...
        let kind: String = conn
            .query_row("SELECT kind FROM dependencies WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(kind, "runtime");

        // ... and the new three-kind CHECK is live on the rebuilt table.
        conn.execute(
            "INSERT INTO dependencies (package_id, depends_on_id, kind)
             VALUES (1, 2, 'link')",
            [],
        )
        .expect("'link' must be accepted after migration");

        // The widened UNIQUE (with opt_tag) is live too.
        conn.execute(
            "INSERT INTO packages (name, version, abi_tag, opt_tag, arch, track, installed_at)
             VALUES ('openssl', '3.0.13', 'ab1', 'a1b2c3d4', 'amd64', 'source', 43)",
            [],
        )
        .expect("opt_tag must widen the UNIQUE key after migration");

        // New v4 tables exist.
        for table in ["package_options", "generations", "generation_packages", "provenance", "build_history", "link_interface"] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "migration must create `{table}`");
        }
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
        assert_eq!(DependencyKind::Link.to_string(), "link");
        assert_eq!(DependencyKind::Runtime.to_string(), "runtime");
        assert_eq!(DependencyKind::Build.to_string(), "build");
    }

    #[test]
    fn dependency_kind_default_is_link() {
        // Matches manifest-v3: a [dependencies] entry without an
        // explicit `kind` is a link dependency.
        assert_eq!(DependencyKind::default(), DependencyKind::Link);
    }

    #[test]
    fn dependency_kind_display_fromstr_round_trip() {
        for kind in [DependencyKind::Link, DependencyKind::Runtime, DependencyKind::Build] {
            let parsed: DependencyKind = kind.to_string().parse().unwrap();
            assert_eq!(parsed, kind);
        }
        // And every string the SQL CHECK constraint accepts must parse.
        for s in ["link", "runtime", "build"] {
            assert!(s.parse::<DependencyKind>().is_ok());
        }
    }

    #[test]
    fn generation_kind_display_fromstr_round_trip() {
        for kind in [
            GenerationKind::Install,
            GenerationKind::Remove,
            GenerationKind::Upgrade,
            GenerationKind::Undo,
        ] {
            let parsed: GenerationKind = kind.to_string().parse().unwrap();
            assert_eq!(parsed, kind);
        }
        // Every string the SQL CHECK constraint accepts must parse.
        for s in ["install", "remove", "upgrade", "undo"] {
            assert!(s.parse::<GenerationKind>().is_ok());
        }
        assert!("rollback".parse::<GenerationKind>().is_err());
    }

    #[test]
    fn dependency_kind_fromstr_rejects_unknown() {
        let err = "dev".parse::<DependencyKind>().unwrap_err();
        assert!(matches!(err, PkgError::Parse(_)));
    }

    #[test]
    fn build_history_has_peak_rss_column() {
        let conn = fresh_db();
        init_schema(&conn).unwrap();
        let has_peak_rss: bool = conn
            .prepare("SELECT peak_rss FROM build_history LIMIT 0")
            .is_ok();
        assert!(has_peak_rss, "build_history must carry peak_rss (§7.9.3)");
        // NULL is legal — the measurement is best-effort.
        conn.execute(
            "INSERT INTO build_history
                 (name, version, abi_tag, option_set_hash, duration_s, success, started_at, host_jobs)
             VALUES ('a', '1.0', 'ab1', '', 1, 1, 0, 1)",
            [],
        )
        .unwrap();
        let peak: Option<i64> = conn
            .query_row("SELECT peak_rss FROM build_history WHERE name = 'a'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(peak, None);
    }

    #[test]
    fn provenance_and_link_interface_cascade_with_the_package_row() {
        // Upgrades delete the superseded packages row; the artifact's
        // provenance and measured interface must go with it, not block
        // the delete under FK enforcement.
        let conn = fresh_db();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO packages (name, version, abi_tag, arch, track, installed_at)
             VALUES ('openssl', '3.0.13', 'ab1', 'amd64', 'source', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provenance
                 (package_id, recipe_commit, source_hashes, index_version, build_host, builder_version, started_at, finished_at)
             VALUES (1, 'c0ffee', '[]', 1, 'host', 'pkg', 0, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO link_interface (package_id, soname, symbol_versions, header_fingerprint, link_interface_hash)
             VALUES (1, 'libssl.so.3', '', 'hfp', 'h1')",
            [],
        )
        .unwrap();

        conn.execute("DELETE FROM packages WHERE id = 1", []).unwrap();
        for table in ["provenance", "link_interface"] {
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table} rows must cascade-delete with the package");
        }
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
