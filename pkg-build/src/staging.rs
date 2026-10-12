//! pkg-build/src/staging.rs — stage → activate (§7.5, v12 §3) and
//! transaction-level undo (design doc Issue 5).
//!
//! ## v1 changes
//!
//! - Initial implementation of the rename-based atomic core: stage a
//!   built `$PKGDIR` into `.stage/` under the store, verify its
//!   manifest, rename it into the store, then swap the FHS symlink tree
//!   link-by-link via `rename(2)` (create `<link>.pkg-new`, rename over
//!   the old link), and commit the DB **last**. Exactly one code path —
//!   [`activate_identities`] — serves normal transactions *and* undo
//!   (Issue 5 §5.2: "reusing §7.5's stage→activate machinery wholesale,
//!   with the generation table playing the role the resolver normally
//!   plays").
//! - `undo_to_generation` (Issue 5): derived symlink reconstruction
//!   (no separate symlink log — the package sets fully determine the
//!   links), strictly-sequential check (§5.3), pre-staging existence
//!   check that fails atomically listing missing store dirs (§5.5.3),
//!   glibc refusal pointing at `glibc-apply --rollback` (§5.1), schema
//!   version refusal (§5.8), and a post-undo pin re-check that warns
//!   without blocking (§5.6).
//!
//! ## v2
//!
//! - [FIX] `GenerationPackageRow`/`GenerationKind` live in
//!   `pkg_db::schema`; `pkg_db::queries` does not re-export them. The
//!   imports and the `GenerationKind::Upgrade` reference now use the
//!   schema path.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use pkg_core::{Arch, Identity, PkgError, PkgResult, Track};
use pkg_db::queries;
use pkg_db::schema::{GenerationKind, GenerationPackageRow, GenerationRow, SCHEMA_VERSION};
use rusqlite::{Connection, OptionalExtension};

use crate::cache;
use crate::orchestrator::PlanNode;

/// Store-rooted staging area: packages are assembled under
/// `<store_root>/.stage/` before the atomic rename into
/// `<store_root>/<dir_name>` (§7.5 Phase A).
pub const STAGE_DIR_NAME: &str = ".stage";

// ---------------------------------------------------------------------
// Symlink planning — the derived activation model (Issue 5 §5.2)
// ---------------------------------------------------------------------

/// One symlink swap in the FHS tree. Both the normal and the undo path
/// are sequences of these; the DB commit that follows them is always
/// last (§7.5 Phase B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymlinkOp {
    /// Absolute FHS path of the link, e.g. `/usr/lib/libfoo.so.1`.
    pub link_path: PathBuf,
    /// What the link pointed at before (`None` = link created).
    pub old_target: Option<PathBuf>,
    /// What it points at after (`None` = link removed).
    pub new_target: Option<PathBuf>,
}

/// Computes the symlink ops that make `identity`'s store dir live under
/// `fhs_root` (`/` for the system; an env root for `pkg env`). One
/// symlink per file/symlink in the store dir; directories become real
/// directories in the FHS tree.
pub fn plan_activation_ops(store_dir: &Path, fhs_root: &Path) -> PkgResult<Vec<SymlinkOp>> {
    let mut ops = Vec::new();
    let mut stack = vec![store_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).map_err(PkgError::Io)? {
            let entry = entry.map_err(PkgError::Io)?;
            let path = entry.path();
            let rel = path.strip_prefix(store_dir).map_err(|_| {
                PkgError::Build(format!(
                    "{} escapes store dir {}",
                    path.display(),
                    store_dir.display()
                ))
            })?;
            if rel.as_os_str() == cache::MANIFEST_NAME {
                continue; // store bookkeeping, not FHS content
            }
            let ftype = entry.file_type().map_err(PkgError::Io)?;
            if ftype.is_dir() {
                stack.push(path);
            } else {
                ops.push(SymlinkOp {
                    link_path: fhs_root.join(rel),
                    old_target: None, // filled by `with_old_targets`
                    new_target: Some(path),
                });
            }
        }
    }
    ops.sort_by(|a, b| a.link_path.cmp(&b.link_path));
    Ok(ops)
}

/// Fills `old_target` from the live filesystem so the swap list doubles
/// as an inspectable "what changes" report (and so undo's diff view is
/// data, not re-derivation).
pub fn with_old_targets(ops: &mut [SymlinkOp]) {
    for op in ops.iter_mut() {
        op.old_target = fs::read_link(&op.link_path).ok();
    }
}

/// Computes the deactivation ops for one store dir: every FHS link that
/// currently points *into* `store_dir` gets removed. Only links this
/// store dir could own are considered — we walk the store dir's own
/// file list, so a foreign package's link at the same path is left
/// alone (a same-path collision would have been rejected at install).
pub fn plan_deactivation_ops(store_dir: &Path, fhs_root: &Path) -> PkgResult<Vec<SymlinkOp>> {
    let planned = plan_activation_ops(store_dir, fhs_root)?;
    let mut ops = Vec::new();
    for mut op in planned {
        let current = fs::read_link(&op.link_path).ok();
        if current.as_deref() == op.new_target.as_deref() {
            op.old_target = op.new_target.take(); // becomes a removal
            ops.push(op);
        }
    }
    Ok(ops)
}

/// Applies symlink ops: for creations/updates, the new link is created
/// at `<path>.pkg-new` and `rename(2)`d over the destination — the swap
/// itself is atomic per link; for removals the link is unlinked. Parent
/// directories are created as real directories.
///
/// Interruption leaves a *consistent* (if stale-link) tree: either the
/// old link or the new one, never a half-written one. Recovery is the
/// next transaction's staging, exactly like §7.5.
pub fn apply_symlink_ops(ops: &[SymlinkOp]) -> PkgResult<()> {
    for op in ops {
        match &op.new_target {
            Some(target) => {
                if let Some(parent) = op.link_path.parent() {
                    fs::create_dir_all(parent).map_err(PkgError::Io)?;
                }
                let tmp = op
                    .link_path
                    .with_extension("pkg-new");
                let _ = fs::remove_file(&tmp);
                std::os::unix::fs::symlink(target, &tmp).map_err(PkgError::Io)?;
                fs::rename(&tmp, &op.link_path).map_err(PkgError::Io)?;
            }
            None => {
                // Removal: only remove if it still points where we
                // expect — never unlink something a later transaction
                // already re-pointed.
                if fs::read_link(&op.link_path).ok().as_deref()
                    == op.old_target.as_deref()
                {
                    let _ = fs::remove_file(&op.link_path);
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Store staging
// ---------------------------------------------------------------------

/// The store root for `arch` among `store_roots`: amd64 takes the
/// root *containing* `.store` directly; i686 takes a `lib32` tree when
/// one was provided (§6.1). With a single root (the common caller) it
/// serves both arches.
pub fn store_root_for(arch: Arch, store_roots: &[PathBuf]) -> PkgResult<PathBuf> {
    match store_roots {
        [] => Err(PkgError::Build("no store roots configured".into())),
        [only] => Ok(only.clone()),
        [primary, lib32, ..] => Ok(match arch {
            Arch::Amd64 => primary.clone(),
            Arch::I686 => lib32.clone(),
        }),
    }
}

/// Stages `pkgdir` into the store as `identity` and verifies the result:
/// assemble under `<store_root>/.stage/<dir_name>`, (re)write and verify
/// the store manifest, then rename into place. The rename is the
/// commit point for the *filesystem* half of the transaction; the DB
/// commit that follows is last (§7.5).
///
/// Returns the final store dir.
pub fn stage_into_store(pkgdir: &Path, identity: &Identity, store_root: &Path) -> PkgResult<PathBuf> {
    let dir_name = identity.store_dir_name();
    let final_dir = store_root.join(&dir_name);
    let stage_root = store_root.join(STAGE_DIR_NAME);
    fs::create_dir_all(&stage_root).map_err(PkgError::Io)?;
    let staged = stage_root.join(&dir_name);

    if staged.exists() {
        fs::remove_dir_all(&staged).map_err(PkgError::Io)?;
    }
    // Same-filesystem rename when possible (§7.5/§7.6's same-fs
    // discipline); copy+remove as the cross-fs fallback for test
    // environments with a tmpfs build root.
    if fs::rename(pkgdir, &staged).is_err() {
        copy_dir_recursive(pkgdir, &staged)?;
        fs::remove_dir_all(pkgdir).map_err(PkgError::Io)?;
    }

    // Verify before the rename into the store proper: a tree that
    // fails its own manifest never becomes addressable under its
    // canonical name.
    cache::write_manifest(&staged)?;
    cache::verify_manifest(&staged)?;

    if final_dir.exists() {
        fs::remove_dir_all(&final_dir).map_err(PkgError::Io)?;
    }
    fs::rename(&staged, &final_dir).map_err(PkgError::Io)?;
    Ok(final_dir)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> PkgResult<()> {
    fs::create_dir_all(dst).map_err(PkgError::Io)?;
    for entry in fs::read_dir(src).map_err(PkgError::Io)? {
        let entry = entry.map_err(PkgError::Io)?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        let ftype = entry.file_type().map_err(PkgError::Io)?;
        if ftype.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else if ftype.is_symlink() {
            let link = fs::read_link(&path).map_err(PkgError::Io)?;
            let _ = fs::remove_file(&target);
            std::os::unix::fs::symlink(link, &target).map_err(PkgError::Io)?;
        } else {
            fs::copy(&path, &target).map_err(PkgError::Io)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// The ONE activation path (normal transactions AND undo)
// ---------------------------------------------------------------------

/// A package set expressed as identities + explicitness, the common
/// currency of normal transactions (from the resolver/plan) and undo
/// (from the generation table).
#[derive(Debug, Clone)]
pub struct ActivationSet {
    /// name+arch → (identity, installed_explicitly)
    pub members: BTreeMap<(String, Arch), (Identity, bool)>,
}

impl ActivationSet {
    /// The currently installed set, from the DB.
    pub fn current(conn: &Connection) -> PkgResult<Self> {
        let mut members = BTreeMap::new();
        for row in queries::list_installed_packages(conn)? {
            let identity = Identity {
                name: row.name.clone(),
                version: row.version.clone(),
                abi_tag: row.abi_tag,
                opt_tag: row.opt_tag.parse().map_err(|e| {
                    PkgError::Parse(format!("bad opt_tag in packages row: {e}"))
                })?,
                arch: row.arch,
            };
            members.insert(
                (row.name, row.arch),
                (identity, row.installed_explicitly),
            );
        }
        Ok(ActivationSet { members })
    }

    /// A generation's pinned set (undo's target).
    pub fn from_generation(rows: Vec<GenerationPackageRow>) -> PkgResult<Self> {
        let mut members = BTreeMap::new();
        for row in rows {
            let identity = Identity {
                name: row.name.clone(),
                version: row.version,
                abi_tag: row.abi_tag,
                opt_tag: row.opt_tag.parse().map_err(|e| {
                    PkgError::Parse(format!("bad opt_tag in generation row: {e}"))
                })?,
                arch: row.arch,
            };
            members.insert((row.name, row.arch), (identity, row.installed_explicitly));
        }
        Ok(ActivationSet { members })
    }

    /// The symmetric difference vs. `current`: identities to activate
    /// (in self, not current — or at a different identity) and to
    /// deactivate (in current, not self — or superseded).
    pub fn diff(&self, current: &ActivationSet) -> (Vec<Identity>, Vec<(Identity, bool)>) {
        let mut activate = Vec::new();
        let mut deactivate = Vec::new();
        for (key, (identity, _)) in &self.members {
            match current.members.get(key) {
                Some((cur, _)) if cur == identity => {}
                _ => activate.push(identity.clone()),
            }
        }
        for (key, (identity, explicit)) in &current.members {
            match self.members.get(key) {
                Some((target, _)) if target == identity => {}
                _ => deactivate.push((identity.clone(), *explicit)),
            }
        }
        (activate, deactivate)
    }

    /// Every store dir this set references, checked for existence
    /// against `store_roots`. Returns the missing dirs (empty = all
    /// present). This is the *pre-staging* existence check (Issue 5
    /// §5.5.3): it runs before any filesystem mutation, so a
    /// non-restorable target fails atomically with the full missing
    /// list.
    pub fn missing_store_dirs(&self, store_roots: &[PathBuf]) -> Vec<PathBuf> {
        let mut missing = Vec::new();
        for (_, (identity, _)) in &self.members {
            let dir_name = identity.store_dir_name();
            let exists = store_roots
                .iter()
                .any(|root| root.join(&dir_name).is_dir());
            if !exists {
                missing.push(PathBuf::from(dir_name));
            }
        }
        missing.sort();
        missing
    }
}

/// The one stage→activate→commit path. `target` is the complete
/// post-transaction set. Caller supplies the generation-recording
/// closure so the same path serves `record_generation` (normal) and
/// `record_undo_generation` (undo) without a second code path.
///
/// Order of operations, per §7.5:
/// 1. verify every to-activate store dir exists (atomic pre-fail),
/// 2. apply the symlink ops (rename-based swaps),
/// 3. DB transaction LAST: package rows + generation record, one commit.
pub fn activate_identities<F>(
    conn: &Connection,
    target: &ActivationSet,
    store_roots: &[PathBuf],
    record_generation: F,
) -> PkgResult<ActivationReport>
where
    F: FnOnce(&Connection) -> PkgResult<i64>,
{
    let current = ActivationSet::current(conn)?;
    let (activate, deactivate) = target.diff(&current);

    // (1) Existence check before ANY mutation (Issue 5 §5.5.3's
    // atomicity guarantee applies to normal transactions too).
    let mut missing = Vec::new();
    for identity in &activate {
        let dir_name = identity.store_dir_name();
        if !store_roots.iter().any(|r| r.join(&dir_name).is_dir()) {
            missing.push(PathBuf::from(dir_name));
        }
    }
    if !missing.is_empty() {
        return Err(PkgError::NotFound(format!(
            "cannot activate: store dirs missing (pruned?):\n  {}",
            missing
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n  ")
        )));
    }

    // (2) Symlink swaps.
    let mut ops = Vec::new();
    for identity in &activate {
        let root = store_root_for(identity.arch, store_roots)?;
        let store_dir = root.join(identity.store_dir_name());
        ops.extend(plan_activation_ops(&store_dir, pkg_core::paths::root())?);
    }
    for (identity, _) in &deactivate {
        let root = store_root_for(identity.arch, store_roots)?;
        let store_dir = root.join(identity.store_dir_name());
        if store_dir.is_dir() {
            ops.extend(plan_deactivation_ops(&store_dir, pkg_core::paths::root())?);
        }
    }
    with_old_targets(&mut ops);
    apply_symlink_ops(&ops)?;

    // (3) DB commit LAST: the whole package-set change and its
    // generation record commit atomically together (Issue 2 §2 /
    // Issue 5 §5.8).
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(|e| PkgError::Db(e.to_string()))?;
    let tx_result = (|| -> PkgResult<i64> {
        for (identity, _) in &deactivate {
            // `dependencies` references package ids through plain
            // (non-cascading) foreign keys, so both edge directions
            // must go before the row itself — otherwise the FK check
            // rejects the delete.
            let dep_id: Option<i64> = conn
                .query_row(
                    "SELECT id FROM packages
                     WHERE name = ?1 AND version = ?2 AND abi_tag = ?3 AND opt_tag = ?4 AND arch = ?5",
                    rusqlite::params![
                        identity.name,
                        identity.version.to_string(),
                        identity.abi_tag.to_string(),
                        identity.opt_tag.to_string(),
                        identity.arch.to_string(),
                    ],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| PkgError::Db(e.to_string()))?;
            if let Some(id) = dep_id {
                conn.execute(
                    "DELETE FROM dependencies WHERE package_id = ?1 OR depends_on_id = ?1",
                    rusqlite::params![id],
                )
                .map_err(|e| PkgError::Db(e.to_string()))?;
            }
            conn.execute(
                "DELETE FROM packages
                 WHERE name = ?1 AND version = ?2 AND abi_tag = ?3 AND opt_tag = ?4 AND arch = ?5",
                rusqlite::params![
                    identity.name,
                    identity.version.to_string(),
                    identity.abi_tag.to_string(),
                    identity.opt_tag.to_string(),
                    identity.arch.to_string(),
                ],
            )
            .map_err(|e| PkgError::Db(e.to_string()))?;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        for identity in &activate {
            let explicit = target
                .members
                .get(&(identity.name.clone(), identity.arch))
                .map(|(_, e)| *e)
                .unwrap_or(false);
            let pid = pkg_core::PackageId {
                name: identity.name.clone(),
                version: identity.version.clone(),
                abi_tag: identity.abi_tag,
                arch: identity.arch,
            };
            queries::insert_package_with_opt_tag(
                conn,
                &pid,
                &identity.opt_tag.to_string(),
                Track::Source,
                now,
                explicit,
            )?;
        }
        record_generation(conn)
    })();
    match tx_result {
        Ok(gen_id) => {
            conn.execute_batch("COMMIT")
                .map_err(|e| PkgError::Db(e.to_string()))?;
            Ok(ActivationReport {
                activated: activate.iter().map(|i| i.store_dir_name()).collect(),
                deactivated: deactivate
                    .iter()
                    .map(|(i, _)| i.store_dir_name())
                    .collect(),
                symlink_ops: ops.len(),
                generation_id: gen_id,
            })
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActivationReport {
    pub activated: Vec<String>,
    pub deactivated: Vec<String>,
    pub symlink_ops: usize,
    pub generation_id: i64,
}

/// Per-package activation used by the orchestrator for a freshly built
/// node (Issue 4.2.5: a cascade is N of these transactions, one per
/// package, sharing a `cascade_id`).
pub fn activate_built_package(
    conn: &Connection,
    pkgdir: &Path,
    node: &PlanNode,
    store_roots: &[PathBuf],
    command: &str,
    cascade_id: Option<&str>,
) -> PkgResult<()> {
    let identity = Identity {
        name: node.name.clone(),
        version: node.version.clone(),
        abi_tag: node.abi_tag,
        opt_tag: node.opt_tag.clone(),
        arch: node.arch,
    };
    let root = store_root_for(node.arch, store_roots)?;
    stage_into_store(pkgdir, &identity, &root)?;

    // The target set = current set with this identity swapped in.
    // Explicitness: an existing row's flag is preserved; a first
    // install takes the node's `requested` flag (cascade rebuilds of
    // dependencies stay non-explicit, §16.6).
    let mut target = ActivationSet::current(conn)?;
    let explicit = target
        .members
        .get(&(identity.name.clone(), identity.arch))
        .map(|(_, e)| *e)
        .unwrap_or(node.requested);
    target
        .members
        .insert((identity.name.clone(), identity.arch), (identity, explicit));
    let command = command.to_string();
    let cascade = cascade_id.map(str::to_string);
    let command_for_gen = command.clone();
    // Dependency edges (§9) are part of the same commit as the package
    // row: removal protection, autoremove, and `pkg why` all read them.
    let edges = node.recipe.manifest.dependency_edges();
    let name_for_edges = node.name.clone();
    let arch_for_edges = node.arch;
    activate_identities(conn, &target, store_roots, move |c| {
        let gen = queries::record_generation(
            c,
            &command_for_gen,
            GenerationKind::Upgrade,
            cascade.as_deref(),
        )?;
        queries::record_dependency_edges(c, &name_for_edges, arch_for_edges, &edges)?;
        Ok(gen)
    })?;
    Ok(())
}

// ---------------------------------------------------------------------
// Undo (Issue 5)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct UndoReport {
    pub reactivated: Vec<String>,
    pub deactivated: Vec<String>,
    pub generation_id: i64,
    /// Pin re-check warnings (§5.6: warn, never block).
    pub pin_warnings: Vec<String>,
    /// Stated-every-time limitations (§5.1: /etc is not reverted).
    pub notes: Vec<String>,
}

/// The generation id whose package set is currently live: for a normal
/// latest generation, itself; after an undo, the pre-transaction state
/// the undo restored. Undo rows must NOT count as the live transaction
/// themselves — otherwise a second `pkg undo` could only ever re-apply
/// what the first one reverted (toggle), never step further back the
/// way §5.3's sequential chain requires.
pub fn live_state_gen(gens: &[GenerationRow]) -> PkgResult<i64> {
    let latest = gens.last().ok_or_else(|| {
        PkgError::NotFound("no generations recorded; nothing to undo".into())
    })?;
    Ok(match (latest.kind, latest.undoes) {
        (GenerationKind::Undo, Some(undone)) => pre_transaction_gen(gens, undone),
        _ => latest.id,
    })
}

/// First generation of the cascade transaction `t` belongs to — `t`
/// itself when it has no `cascade_id`. A rebuild cascade is N
/// per-package transactions sharing one `cascade_id` (§4.2.5); undo
/// treats them as a single unit.
fn cascade_first(gens: &[GenerationRow], t: i64) -> i64 {
    gens.iter()
        .find(|g| g.id == t)
        .and_then(|g| g.cascade_id.clone())
        .map(|c| {
            gens.iter()
                .filter(|g| g.cascade_id.as_deref() == Some(c.as_str()))
                .map(|g| g.id)
                .min()
                .unwrap_or(t)
        })
        .unwrap_or(t)
}

/// The generation whose package set was live before transaction `t`
/// ran — counting `t`'s whole cascade as one transaction. `pkg undo n`
/// restores exactly this state.
pub fn pre_transaction_gen(gens: &[GenerationRow], t: i64) -> i64 {
    cascade_first(gens, t) - 1
}

/// Restores the package set of generation `gen_id` (`pkg undo`,
/// Issue 5).
///
/// Hard rules enforced here, in order:
///
/// 1. **Sequential-only (§5.3):** `gen_id` must be the state
///    immediately preceding the live one (the live state is computed
///    through [`live_state_gen`], so undo rows don't toggle). Anything
///    older is refused with an explanation and the sequential path
///    (`pkg undo` × k) named.
/// 2. **Schema version (§5.8):** a generation recorded under a
///    different DB schema version is non-restorable, forever.
/// 3. **glibc (§5.1):** if the target's glibc identity differs from
///    the installed one, refuse — glibc anchor swaps belong to
///    `glibc-apply --rollback`, never to symlink undo.
/// 4. **Existence (§5.5.3):** every store dir the target needs must
///    exist *before staging*; any missing → atomic failure listing all
///    of them.
///
/// Pins are re-checked after the swap and violations are *warnings*
/// (§5.6: undo is not a resolution).
pub fn undo_to_generation(
    conn: &Connection,
    gen_id: i64,
    store_roots: &[PathBuf],
) -> PkgResult<UndoReport> {
    let gens = queries::list_generations(conn)?;
    let latest = gens.last().ok_or_else(|| {
        PkgError::NotFound("no generations recorded; nothing to undo".into())
    })?;
    let target = queries::get_generation(conn, gen_id)?.ok_or_else(|| {
        PkgError::NotFound(format!("no generation with id {gen_id}"))
    })?;

    // (1) Sequential-only.
    let live = live_state_gen(&gens)?;
    let expected = pre_transaction_gen(&gens, live);
    if gen_id != expected {
        return Err(PkgError::Resolution(format!(
            "undo is strictly sequential: generation {gen_id} is not the state \
             immediately preceding the current one (generation {live}).\n\
             Out-of-order undo would require re-solving against historical \
             constraints the system no longer has; sequential undo steps \
             through states the system provably occupied.\n\
             To go back further: run `pkg undo` repeatedly."
        )));
    }

    // (2) Schema version.
    if target.schema_ver != SCHEMA_VERSION {
        return Err(PkgError::Db(format!(
            "generation {gen_id} was recorded under schema v{} (current: \
             v{SCHEMA_VERSION}); undo across schema migrations is forbidden \
             because the restored set would be read with different column \
             semantics",
            target.schema_ver
        )));
    }

    // (3) glibc: undo never rolls the libc anchor.
    let target_set = ActivationSet::from_generation(queries::generation_package_set(conn, gen_id)?)?;
    let current_set = ActivationSet::current(conn)?;
    let glibc_of = |set: &ActivationSet| {
        set.members
            .iter()
            .find(|((name, _), _)| name == "glibc")
            .map(|(_, (id, _))| id.store_dir_name())
    };
    match (glibc_of(&target_set), glibc_of(&current_set)) {
        (Some(t), Some(c)) if t != c => {
            return Err(PkgError::Gate(format!(
                "generation {gen_id} carries a different glibc ({t} vs current {c}).\n\
                 A symlink-tree undo does not roll back the glibc anchor swap; \
                 use `glibc-apply --rollback` (the v13 rescue path) instead."
            )));
        }
        _ => {}
    }

    // (4) Pre-staging existence check — atomic failure, full list.
    let missing = target_set.missing_store_dirs(store_roots);
    if !missing.is_empty() {
        return Err(PkgError::NotFound(format!(
            "generation {gen_id} is no longer restorable: required store dirs \
             were pruned (check `pkg history`; a past `pkg clean` released \
             them):\n  {}",
            missing
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n  ")
        )));
    }

    // The swap itself: the SAME stage→activate machinery as any
    // transaction, with the generation table playing the resolver's
    // role (Issue 5 §5.2).
    let command = format!("pkg undo {gen_id}");
    let undone_id = latest.id;
    let command_for_gen = command.clone();
    let report = activate_identities(conn, &target_set, store_roots, move |c| {
        queries::record_undo_generation(c, &command_for_gen, undone_id, None)
    })?;

    // Post-undo pin re-check (§5.6): warn, never block.
    let pin_warnings = recheck_pins(conn);

    let notes = vec![
        "undo restores the package set and symlink tree only; /etc config \
         files are NOT reverted (conffile state may have been migrated \
         forward by the newer software — reconcile via the package's hooks)"
            .to_string(),
        "processes started since the undone transaction may still hold \
         deleted inodes of the newer library versions; run `pkg audit` and \
         `pkg apply-pending-restarts` to restart affected services"
            .to_string(),
    ];

    Ok(UndoReport {
        reactivated: report.activated,
        deactivated: report.deactivated,
        generation_id: report.generation_id,
        pin_warnings,
        notes,
    })
}

/// Warns when the restored set violates a pin (Issue 5 §5.6). Pins are
/// deliberately NOT modified — they constrain future resolutions, and
/// undo is not a resolution.
fn recheck_pins(conn: &Connection) -> Vec<String> {
    let mut warnings = Vec::new();
    let pins_path = pkg_core::paths::at("/var/lib/pkg/config/pins.toml");
    let Ok(text) = fs::read_to_string(&pins_path) else {
        return warnings; // no pins file → nothing to check
    };
    let Ok(pins) = pkg_resolver::PinSet::from_toml_str(&text) else {
        return warnings; // unparseable pins are install-time's problem
    };
    let Ok(installed) = queries::list_installed_packages(conn) else {
        return warnings;
    };
    for row in installed {
        if let Some(pin) = pins.get(&row.name) {
            if !pin.allows(&row.version, Some(&row.abi_tag)) {
                warnings.push(format!(
                    "warning: restored {} {} violates an existing pin; \
                     consider `pkg unpin {0}` or re-upgrading (undo does not \
                     modify pins — they constrain future resolutions only)",
                    row.name, row.version
                ));
            }
        }
    }
    warnings
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pkg_core::{AbiTag, OptTag};

    fn identity(name: &str, version: &str, abi: u32) -> Identity {
        Identity {
            name: name.to_string(),
            version: version.parse().unwrap(),
            abi_tag: AbiTag(abi),
            opt_tag: OptTag::none(),
            arch: Arch::Amd64,
        }
    }

    fn set(pairs: Vec<(Identity, bool)>) -> ActivationSet {
        ActivationSet {
            members: pairs
                .into_iter()
                .map(|(id, e)| ((id.name.clone(), id.arch), (id, e)))
                .collect(),
        }
    }

    #[test]
    fn diff_reports_both_directions() {
        let current = set(vec![
            (identity("openssl", "3.0.13", 1), false),
            (identity("curl", "8.0", 2), true),
        ]);
        let target = set(vec![
            (identity("openssl", "3.0.14", 1), false), // version changed
            (identity("git", "2.44", 1), true),        // new
        ]);
        let (activate, deactivate) = target.diff(&current);
        let act: Vec<&str> = activate.iter().map(|i| i.name.as_str()).collect();
        let deact: Vec<&str> = deactivate.iter().map(|(i, _)| i.name.as_str()).collect();
        assert!(act.contains(&"openssl") && act.contains(&"git"));
        assert!(deact.contains(&"openssl") && deact.contains(&"curl"));
    }

    #[test]
    fn diff_noop_when_identical() {
        let a = set(vec![(identity("zlib", "1.3", 4), true)]);
        let (act, deact) = a.diff(&a.clone());
        assert!(act.is_empty() && deact.is_empty());
    }

    #[test]
    fn missing_dirs_lists_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        // Only zlib's store dir exists.
        fs::create_dir_all(root.join(identity("zlib", "1.3", 4).store_dir_name())).unwrap();
        let target = set(vec![
            (identity("zlib", "1.3", 4), true),
            (identity("openssl", "3.0.14", 1), false),
            (identity("curl", "8.0", 2), true),
        ]);
        let missing = target.missing_store_dirs(&[root]);
        assert_eq!(missing.len(), 2);
        assert!(missing.iter().any(|p| p.to_string_lossy().contains("openssl")));
        assert!(missing.iter().any(|p| p.to_string_lossy().contains("curl")));
    }

    #[test]
    fn symlink_swap_roundtrip() {
        // Store dir with a lib; activate into a fake FHS root.
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store/foo-1.0+ab1");
        fs::create_dir_all(store.join("usr/lib")).unwrap();
        fs::write(store.join("usr/lib/libfoo.so.1"), b"elf").unwrap();
        let fhs = tmp.path().join("fhs");
        fs::create_dir_all(&fhs).unwrap();

        let mut ops = plan_activation_ops(&store, &fhs).unwrap();
        with_old_targets(&mut ops);
        apply_symlink_ops(&ops).unwrap();
        let link = fhs.join("usr/lib/libfoo.so.1");
        assert_eq!(fs::read_link(&link).unwrap(), store.join("usr/lib/libfoo.so.1"));

        // Re-point the same link at a new store dir: rename swap, and
        // old_target recorded.
        let store2 = tmp.path().join("store/foo-1.1+ab1");
        fs::create_dir_all(store2.join("usr/lib")).unwrap();
        fs::write(store2.join("usr/lib/libfoo.so.1"), b"elf2").unwrap();
        let mut ops2 = plan_activation_ops(&store2, &fhs).unwrap();
        with_old_targets(&mut ops2);
        assert_eq!(ops2[0].old_target, Some(store.join("usr/lib/libfoo.so.1")));
        apply_symlink_ops(&ops2).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), store2.join("usr/lib/libfoo.so.1"));

        // Deactivation removes only links still pointing into store2.
        let rm = plan_deactivation_ops(&store2, &fhs).unwrap();
        assert_eq!(rm.len(), 1);
        assert!(rm[0].new_target.is_none());
        apply_symlink_ops(&rm).unwrap();
        assert!(!link.exists() && !link.is_symlink());
    }
}
