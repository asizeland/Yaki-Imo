//! pkg-cli/src/cli.rs — clap-4 derive definitions for the full `pkg`
//! subcommand surface (SPEC "pkg-cli"; v11 §16.1; Issue 1 §5 `--opt`;
//! Issue 4.4 `--dry-run`/`--skip-rebuilds`/`--background`; Issue 5 §5.3
//! `undo`).

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "pkg", version, about = "Source-first package manager")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Flags shared by every transaction that can trigger builds/cascades
/// (install, upgrade; `build` takes the build-relevant subset).
#[derive(Debug, Clone, Args)]
pub struct TransactionFlags {
    /// Declared-option override for this transaction, `name=0|1`
    /// (Issue 1 §5: per-command precedence; not persisted). Repeatable.
    #[arg(long = "opt", value_name = "name=val")]
    pub opts: Vec<String>,

    /// Resolve and preview the full cascade, then stop (Issue 4.4.1).
    #[arg(long)]
    pub dry_run: bool,

    /// Activate direct updates and DEFER the rebuild cascade
    /// (Issue 4.4.2). Refuses to defer security-relevant cascades
    /// without --defer-security.
    #[arg(long)]
    pub skip_rebuilds: bool,

    /// Permit --skip-rebuilds to defer a cascade containing a security
    /// fix. Deliberately un-ergonomic (Issue 4.4.2): the value must be
    /// typed out in full.
    #[arg(long, value_name = "I-understand")]
    pub defer_security: Option<String>,

    /// Detach the build phase to the pkg-buildd supervisor
    /// (Issue 4.2.3); the resolver and cascade plan commit up front.
    #[arg(long)]
    pub background: bool,

    /// Assume yes to interactive confirmations (§16.3).
    #[arg(short = 'y', long)]
    pub assume_yes: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Install a package (binary-first n/a — source builds only; §16.1).
    Install {
        /// `name`, `name@version`, or `name=version` (recovery path).
        specs: Vec<String>,
        #[command(flatten)]
        flags: TransactionFlags,
    },

    /// Remove a package (§16.6 reverse-dependency protection).
    Remove {
        name: String,
        /// Also remove reverse dependents (shows the full set first).
        #[arg(long)]
        recursive: bool,
        /// Remove only the named package, ignoring dependents.
        #[arg(long)]
        force: bool,
        #[arg(short = 'y', long)]
        assume_yes: bool,
    },

    /// Upgrade all or named packages.
    Upgrade {
        specs: Vec<String>,
        /// Upgrade everything installed (default when no names given).
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        flags: TransactionFlags,
    },

    /// Fetch and verify the latest repository index (§10.6).
    Sync,

    /// Search the repository index.
    Search { query: String },

    /// Show installed version, ABI/option tags, deps, files (§16.1);
    /// option deltas from recipe default (Issue 1 §5).
    Info { name: String },

    /// Explain why a package is installed (dependency chain).
    Why { name: String },

    /// Show the PubGrub explanation for an unsatisfiable constraint.
    Explain { constraint: String },

    /// Pin a package to a version or ABI track (§5.6.1). No version:
    /// show the current pin.
    Pin {
        package: String,
        version: Option<String>,
        #[arg(long, value_name = "exact|abi-track")]
        mode: Option<String>,
    },

    /// Remove a pin.
    Unpin { package: String },

    /// Run scanner diagnostics (§8.6).
    Check {
        /// Package name or binary path ($PATH search for names).
        target: Option<String>,
        #[arg(long)]
        verbose: bool,
        #[arg(long, value_name = "minimal|standard|deep")]
        depth: Option<String>,
        #[arg(long)]
        nss: bool,
        #[arg(long)]
        suggest: bool,
    },

    /// Remove dependency-only packages no longer required (§16.6).
    Autoremove {
        #[arg(short = 'y', long)]
        assume_yes: bool,
    },

    /// Prune unreferenced store dirs / reclaim the artifact cache.
    Clean {
        /// Also prune dirs referenced only by old generations
        /// (Issue 5 §5.2: warns which generations become non-restorable).
        #[arg(long)]
        garbage: bool,
        /// Reclaim the personal artifact cache (Issue 4.5).
        #[arg(long)]
        cache: bool,
    },

    /// List transactions, or show one generation's package-set diff.
    History {
        /// Generation id; omit to list all.
        generation: Option<i64>,
        #[arg(long)]
        json: bool,
    },

    /// Restore the pre-transaction state of transaction N (default:
    /// the most recent restorable). STRICTLY SEQUENTIAL (Issue 5 §5.3).
    Undo {
        /// Transaction (generation) to undo.
        n: Option<i64>,
        /// NOT SUPPORTED — reserved so the refusal is discoverable
        /// (Issue 5 §5.4). Use `pkg install <pkg>=<old-version>` +
        /// `pkg pin` for single-package downgrades.
        #[arg(long, value_name = "name")]
        package: Option<String>,
        /// Rebuild pruned store dirs from their pinned recipe commits
        /// instead of failing (Issue 5 §5.5.5 — clearly-labeled
        /// fallback; converts instant undo into a source-build session).
        #[arg(long)]
        rebuild_missing: bool,
        #[arg(short = 'y', long)]
        assume_yes: bool,
    },

    /// Named, exportable environments (Issue 2 §4).
    Env {
        #[command(subcommand)]
        action: EnvAction,
    },

    /// Print the generated build-time environment (v11 §16.1's
    /// original `pkg env`, renamed per Issue 2 §4's note).
    BuildEnv {
        name: String,
        /// Resolve a hypothetical version instead of the installed one.
        #[arg(long)]
        resolve: bool,
    },

    /// Security advisories, deleted-inode processes, supply-chain
    /// provenance (v11 §16.1, v13 §7, Issue 3.6).
    Audit {
        #[arg(long)]
        json: bool,
        /// List PIDs/services holding deleted glibc inodes (v13 §7).
        #[arg(long)]
        glibc: bool,
        /// Supply-chain report: attestations, provenance, repro (Issue 3.6).
        #[arg(long)]
        supply_chain: bool,
        /// Restrict --supply-chain to the critical set.
        #[arg(long)]
        critical: bool,
        /// Acknowledge a --supply-chain hard finding for a package
        /// (unblocks install/upgrade in enforcement mode, Issue 3.6).
        #[arg(long, value_name = "package")]
        acknowledge: Option<String>,
    },

    /// Run a binary in a temporary mount namespace (§16.1).
    Isolate {
        path: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Fetch + verify a package's declared sources (v14 T7).
    Fetch { name: String },

    /// Force a source build of a package (§16.1).
    Build {
        name: String,
        #[arg(long = "opt", value_name = "name=val")]
        opts: Vec<String>,
        /// Bypass the personal artifact cache (Issue 4.5: "exists for
        /// paranoia").
        #[arg(long)]
        no_cache: bool,
    },

    /// Trust-chain management (§10.5.4).
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },

    /// Database maintenance (§9.3).
    Db {
        #[command(subcommand)]
        action: DbAction,
    },

    /// First-boot bootstrap checks (v13 §9 exposes --verify-layout).
    Bootstrap {
        /// Re-verify the glibc anchor/stage same-filesystem invariant.
        #[arg(long)]
        verify_layout: bool,
    },

    /// Restart services holding deleted inodes, in OpenRC dependency
    /// order (v13 §7).
    ApplyPendingRestarts {
        #[arg(short = 'y', long)]
        assume_yes: bool,
    },

    /// Inspect or schedule deferred rebuild cascades (Issue 4.3.3).
    Cascade {
        /// Schedule the pending cascade (e.g. `--at 03:00`).
        #[arg(long, value_name = "HH:MM")]
        at: Option<String>,
        /// Run the pending cascade now (what the nightly timer calls).
        #[arg(long)]
        execute_pending: bool,
        /// Override the idle-window power/idle guards (Issue 4.3.3).
        #[arg(long)]
        ignore_power: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum EnvAction {
    /// Resolve + materialize a named environment.
    Create {
        name: String,
        #[arg(long, value_name = "manifest.toml")]
        from: Option<String>,
    },
    /// List environments.
    List,
    /// Show resolved identities + option deltas.
    Show { name: String },
    /// Print shell exports (`eval "$(pkg env activate <name>)"`).
    Activate {
        name: String,
        #[arg(long, value_name = "posix|fish")]
        shell: Option<String>,
    },
    /// Re-resolve & rebuild within the manifest's constraints.
    Update { name: String },
    /// Export manifest + resolved identity lock for sharing/CI.
    Export { name: String },
    /// Delete an environment, releasing its store pins.
    Delete { name: String },
}

#[derive(Debug, Subcommand)]
pub enum KeyAction {
    /// List keys in the trust chain.
    List,
    /// Show the delegation chain root → repository → timestamp.
    ShowChain,
    /// Install a new root trust anchor (§10.5.2).
    TrustRoot { file: String },
}

#[derive(Debug, Subcommand)]
pub enum DbAction {
    /// Verify database integrity (§9.3).
    Check,
    /// Rebuild the database from the installed store (§16.1).
    Regenerate,
}
