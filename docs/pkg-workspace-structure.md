# [Name TBD] Package Manager — Workspace Structure & Build Plan

This document maps the v11 specification onto a concrete Cargo workspace: which crate owns which spec section, what files live in each crate, and the dependency graph between them. It's a companion to `pkg-spec-v11.md`, not a replacement — section references below point back into that document.

We'll write these files one at a time, in the order suggested at the bottom, across sessions.

---

## 1. Crate Overview

| Crate | Purpose | Spec Sections |
|---|---|---|
| `pkg-core` | Shared types, manifest parsing. No business logic — the data model everything else builds on. | §4 |
| `pkg-resolver` | PubGrub-based dependency resolution, pinning, profiles, explanations. | §5 |
| `pkg-db` | SQLite schema, queries, corruption handling, locking. | §9 |
| `pkg-sandbox` | Bubblewrap wrapper and sandbox profile definitions, shared by build/hooks/DKMS. | §7.1, §7.2.1, §7.8.3 |
| `pkg-scanner` | Binary scanner and gate — both the build-time gate and the standalone `pkg check` diagnostic. | §8 |
| `pkg-repo` | Repository format, trust chain/key rotation, sync, retention, reproducibility tracking. | §10 |
| `pkg-build` | Build orchestrator: recipes, hooks, transaction staging/activation, glibc special case, DKMS triggers. | §7.3–§7.8 |
| `pkg-audit` | Security auditing: advisory matching, inode-based process scanning, enforcement mode. | §16.4, §16.5 |
| `pkg-init` | Standalone minimal static binary (`glibc-apply`) that runs pre-init inside the initramfs. **Not** a subcommand of `pkg` — a genuinely separate program with its own tiny dependency footprint. | §7.6 |
| `pkg-cli` | The actual `pkg` binary. Entry point, argument parsing, command dispatch. Assembles every other crate. This is what gets built musl-static per §17. | §16, §17, §18 |

---

## 2. Dependency Graph

```
pkg-core
  ├── pkg-resolver
  ├── pkg-db
  ├── pkg-sandbox
  ├── pkg-scanner
  └── pkg-repo
        │
        ▼
pkg-build   (core, sandbox, db, scanner, resolver)
pkg-audit   (core, db, repo)
pkg-init    (core only — deliberately minimal, no further deps if avoidable)
        │
        ▼
pkg-cli     (everything)
```

No cycles: `pkg-core` sits at the bottom, five crates depend only on it, `pkg-build`/`pkg-audit` sit one layer up, and `pkg-cli` is the single integration point at the top. `pkg-init` is intentionally off to the side — it needs to be small and robust enough to run before `/sbin/init` exists, so it doesn't pull in the same dependency tree as the rest of the workspace even though it conceptually belongs to the glibc-swap mechanism in `pkg-build`.

---

## 3. File Breakdown by Crate

### `pkg-core`
| File | Contents |
|---|---|
| `lib.rs` | Re-exports |
| `package.rs` | `PackageId`, `Version`, `AbiTag` (§7.7), `Track` enum |
| `manifest.rs` | `pkg-meta.toml` parsing and field validation (§4.1–§4.2), including `dkms-module`/`dkms-name`/`dkms-version` |
| `virtual.rs` | Virtual package / OR-group types (§5.2) |
| `error.rs` | Shared error types used across the workspace |

### `pkg-resolver`
| File | Contents |
|---|---|
| `lib.rs` | Re-exports |
| `pubgrub.rs` | Core solver integration |
| `graph.rs` | Virtuals, OR-groups, multilib co-selection (§5.2–§5.4) |
| `profile.rs` | Resolution profiles (§5.4, §10.3), profile-vs-pin precedence |
| `pin.rs` | Pinning logic — exact vs. `abi-track` modes (§5.6, §5.6.1) |
| `explain.rs` | `pkg why` / `pkg explain` machinery (§5.5); reused by `pkg remove`'s dependent-chain display (§16.6) |

### `pkg-db`
| File | Contents |
|---|---|
| `lib.rs` | Re-exports |
| `schema.rs` | Table definitions, migrations (§9.2), including `installed_explicitly` |
| `queries.rs` | CRUD operations |
| `integrity.rs` | Corruption detection, `pkg db check`/`regenerate` (§9.3) |
| `lock.rs` | `flock`-based coordination, shared/exclusive semantics (§9.4) |

### `pkg-sandbox`
| File | Contents |
|---|---|
| `lib.rs` | Re-exports |
| `bubblewrap.rs` | Low-level `bwrap` invocation wrapper |
| `profile.rs` | Sandbox profile definitions: build sandbox (§7.1), hook categories (§7.2.1), Kernel-Module profile (§7.8.3) |
| `network.rs` | DNS-allowlist proxy for `NETWORK_SOURCES` (§7.2.2) |

### `pkg-scanner`
| File | Contents |
|---|---|
| `lib.rs` | Re-exports |
| `elf.rs` | `DT_NEEDED`/soname extraction, GLIBC floor/ceiling check |
| `heuristic.rs` | `dlopen` string-grep, plugin-config parsing |
| `gate.rs` | Unified pass/fail rules used by the build pipeline |
| `check.rs` | `pkg check` diagnostics — works on arbitrary binaries, not just build output |

### `pkg-repo`
| File | Contents |
|---|---|
| `lib.rs` | Re-exports |
| `format.rs` | Flat index parsing, `.pkg.tar.zst` handling (§10.2) |
| `trust.rs` | Root/repository/timestamp role verification, key rotation (§10.5) |
| `keys.rs` | `pkg key` backing logic (§10.5.4) |
| `sync.rs` | `pkg sync`, mirror fallback, cache TTL (§10.6) |
| `retention.rs` | Version retention policy, source-build fallback (§10.7) |
| `reproducibility.rs` | Tier 1/Tier 2 tracking, `reproducibility.json` (§10.8) |
| `advisory.rs` | `security.json` parsing (§10.4) |

### `pkg-build`
| File | Contents |
|---|---|
| `lib.rs` | Re-exports |
| `recipe.rs` | Bash recipe parsing/execution (§7.1) |
| `hooks.rs` | Hook execution within category sandboxes (§7.2.1) |
| `env.rs` | Generated `dep-env.sh`, per-job scoped dependency tree (§7.3–§7.4) |
| `stage.rs` | Transaction staging (§7.5, Phase A) |
| `activate.rs` | Atomic core + best-effort split (§7.5, Phase B): symlink swaps, DB commit ordering, hooks/DKMS/initramfs as best-effort |
| `glibc.rs` | Glibc special case: anchor design, staging, category-based activation (§7.6, §3.7) |
| `dkms.rs` | DKMS trigger points, per-kernel `linux-headers` tracking, Kernel-Module sandbox invocation (§7.8) |
| `abi.rs` | ABI tag derivation (§7.7) |

### `pkg-audit`
| File | Contents |
|---|---|
| `lib.rs` | Re-exports |
| `advisory_match.rs` | Cross-reference installed packages against `security.json` |
| `process_scan.rs` | Inode-based `/proc` scanning for vulnerable running processes (§16.5) |
| `enforce.rs` | Enforcement mode gating, index-freshness check (§16.4, tied to §10.6) |

### `pkg-init`
| File | Contents |
|---|---|
| `main.rs` | Checks the pending flag, performs the single `renameat2` swap, rollback support (§7.6) |

Deliberately a single file. Correctness here comes from minimalism, not code reuse — this binary runs in an environment with almost nothing else available, and pulling in the rest of the workspace's dependency tree would work against the reason it exists as a separate crate at all.

### `pkg-cli`
| File | Contents |
|---|---|
| `main.rs` | Entry point |
| `cli.rs` | Argument parsing |
| `commands/install.rs` | install / build / remove / autoremove / upgrade |
| `commands/query.rs` | search / info / why / explain |
| `commands/alternatives.rs` | alternatives list / set, rebuild --affected |
| `commands/check.rs` | check / isolate |
| `commands/audit.rs` | audit |
| `commands/clean.rs` | clean |
| `commands/profile.rs` | profile list / apply |
| `commands/pin.rs` | pin / unpin |
| `commands/key.rs` | key list / show-chain / trust-root |
| `commands/db.rs` | db check / regenerate |
| `commands/sync.rs` | sync |
| `commands/bootstrap.rs` | bootstrap (§18) |

---

## 4. Suggested Write Order

Bottom of the dependency graph first, so each file compiles against things that already exist rather than stubs:

1. `pkg-core` — `package.rs`, `manifest.rs`, `virtual.rs`, `error.rs`
2. `pkg-db` — self-contained, easy to sanity-check independently
3. `pkg-resolver` — the most algorithmically interesting piece, worth dedicated attention
4. `pkg-sandbox` — small, needed before `pkg-build` can proceed
5. `pkg-scanner` — self-contained, no dependency on build machinery
6. `pkg-repo` — trust/sync/retention
7. `pkg-build` — pulls the above together, including glibc and DKMS special cases
8. `pkg-audit`
9. `pkg-init` — small, but conceptually easiest to place once `pkg-build/glibc.rs` exists as its counterpart
10. `pkg-cli` — final integration

Want to start with `pkg-core/package.rs`, or somewhere else?

---

*Companion to `pkg-spec-v11.md`. Section references throughout point back into that document.*
