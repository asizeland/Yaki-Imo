# Review: [Name TBD] Package Manager — Practical Concerns & Next Steps

**Scope:** pkg-spec-v11 (+ v8 drift check), workspace-structure doc, and 15 Rust draft modules, reviewed under the constraint of **source compilation only — no binary package management**.
**Review method:** 4 parallel reviewers (spec, core/db modules, resolver modules, system modules), findings merged and deduped below.

---

## Part 1 — Critical issues (fix before writing more code)

### 1. `Version` violates the `Eq`/`Ord`/`Hash` contract *(code, package.rs)*
`#[derive(PartialEq, Eq)]` compares raw `release`/`patch`, but the hand-written `Ord` pads trailing zeros and treats `patch: None` as `0`. So `2.39` vs `2.39.0` compare `Equal` but are `!=`. This corrupts `BTreeSet`s and, worse, PubGrub's internal version-set logic. **Normalize on construction** (strip trailing zeros, canonicalize patch) and derive/handle all three traits on the normalized form.

### 2. Exclusive global lock held across multi-hour builds *(spec §9.4 + §7.5)*
The writer `flock` spans "the duration of the transaction (Phase A + B)," and Phase A includes building. Under source-only use, **every** install holds a system-wide exclusive lock for potentially hours, serializing all other `pkg` invocations — this is your common case, not an edge case. Decide now: build *outside* the lock and re-validate resolution at staging time, or accept full serialization and document it. This must be resolved before `lock.rs` and `pkg-build` are written.

### 3. Pins and profiles are never enforced by the solver *(code, pubgrub.rs)*
`resolve_dependencies(catalog, root_name, root_version)` takes no `PinSet`, no profile constraints, no installed-set. `pin.rs` concedes it "isn't wired up yet." Today an `AtLeast`-constrained, unpinned solve silently floats to newest — exactly what pins exist to prevent. §5.6's precedence rules have no enforcement site. **Fix by filtering candidates at `Catalog` registration or via a custom `DependencyProvider::prioritize`.**

### 4. `abi-track` pin logic is wrong in both directions *(code, pin.rs)*
- **Leaks major bumps:** the `same_release_core OR same abi_tag` check applies even to `-rN`-convention packages, so `glibc 2.40` with an unchanged abi_tag passes a `2.39.*` pin — the exact bump §5.6.1 promises to block.
- **Allows downgrades:** no `candidate >= pinned` check; an abi-track pin on `openssl 3.0.12` admits `3.0.11`.
- **Unimplementable as designed:** `abi_tag` is derived *at build time*, but pins filter *pre-build*. Either the repo index must publish per-version abi_tags, or the abi-tag fallback becomes a post-build verification step. Decide the contract.

### 5. DKMS: three contradictory field representations + an unimplementable sandbox profile *(spec M1/M2)*
- §4.1/§4.2: presence of a `[dkms]` table is the signal. §7.8.1/§7.8.3: "`dkms-module = true`". Workspace doc: flat keys `dkms-module`/`dkms-name`/`dkms-version` — the exact shape §4.1 argues against. **Pick the §4.1 form everywhere.**
- The **Kernel-Module sandbox profile** (read-write `/lib/modules/<kver>/extra/`, no `/etc` write) will fail on the first real nvidia run: real DKMS needs `/var/lib/dkms/`, `depmod` rewrites `modules.dep*` across the whole `/lib/modules/<kver>/`, and module signing/secure-boot is unaddressed. Specify empirically against real DKMS or mark TBD.
- **Resolver purity broken** (§7.8.1): implicit `linux-headers` edges derived from *currently installed kernels* make resolution depend on install state, and have no schema representation (nothing records "module X built for kernel Y"). Needs a data model + kernel-transaction ordering story.

---

## Part 2 — Major issues

| # | Where | Issue |
|---|---|---|
| 6 | integrity.rs | `scan_store` doesn't filter DB rows by `arch` despite its own doc saying it must — every multilib scan false-positives `MissingFromDisk` for the other arch, and cross-arch dir-name collisions can mask orphans. |
| 7 | queries.rs | `reverse_dependents` ignores dependency `kind` — in a source-only PM most edges are *build* deps, so `pkg remove` would constantly block on packages that merely once built against the target. Filter to runtime edges. |
| 8 | pubgrub.rs | pubgrub 0.3 result iteration likely doesn't compile (`SelectedDependencies` has `.iter()`, `IntoIterator` unconfirmed). Pin the version and verify. |
| 9 | virtual_pkg.rs | Module doc claims it lives in pkg-core but it was delivered with the resolver files; `Dependency`/`VersionConstraint` would be defined twice. Also: no serde impls, yet manifests are TOML. Also: no upper-bound/range constraint variant — profiles can't express `>= 24, < 25`, and abi-track can't be represented as a version set. |
| 10 | schema.rs | `files.path UNIQUE` may be too strong for multilib co-installation (verify against §6.1's `lib32/pkg/` split — if paths can't collide by construction, document that). `schema_meta` lacks a single-row guard. |
| 11 | spec §21 vs §7.6 | v11's decision table silently reverses v8's hard "no copy-and-reboot fallback" for glibc into an `--allow-unsafe-copy` opt-in — contradicting §7.6, which v11 says is unchanged. Looks accidental; restore v8 semantics or fully specify the fallback. |
| 12 | spec §10.5 | Trust-chain bootstrap origin unspecified (where does the first root manifest come from?), no rollback protection (version counters), and the online timestamp role is a single point of failure with no grace period or re-delegation path. |
| 13 | spec | `installed_explicitly` default 0 + no migration note → first `pkg autoremove` on a migrated DB proposes removing the entire system. |
| 14 | manifest.rs | OR-group parsing silently accepts empty alternatives (`"a||b"` → dependency on `""`). |
| 15 | lock.rs | `fs4` try-lock API is version-sensitive — pin the version. Stale holder PID metadata after writer release makes §9.4's "pid N" message sometimes wrong. |

*(Minor findings — double-solve for explanations, duplicate pin/profile entries silently overwriting, NULL-fragile `NOT IN` autoremove query, non-transactional `regenerate_orphans`, unversioned-libc-symbol floor check, cc-dependent tests — are in the backlog section of each reviewer's report; ask if you want the full annexes.)*

---

## Part 3 — Source-only impact: what to cut and what to keep

**Dead weight you can skip (defers crates/files):**
- §10.2 binary repository, §10.7 retention policy, §10.8 reproducibility tiers, the `'binary'` track value, §15's binary pipeline stage, §19 Phase 4's "binary install faster than source" milestone.
- Workspace impact: in `pkg-repo`, drop `retention.rs`, `reproducibility.rs`, most of `format.rs`. `pkg-repo` shrinks ~60%.

**Keep, contrary to a naive read:**
- §10.5/§10.6 trust chain + sync — the bootstrap glibc tarball (§18.2) is built by the binary CI pipeline you won't run, so it's a permanently imported artifact that still needs signature verification; `security.json`/advisories also ride this channel.

**The gap cutting creates — your biggest source-only rework item:**
Nothing specifies integrity for **fetched recipes and sources**, which you *will* execute as root (sandboxed). The trust chain authenticates an index of binaries you'll never download while the recipe git repo has no stated trust path. Decide: signed commits, recipe hashes in the signed index, or a signed recipe-manifest. Also: `pkg install foo=<version>` under source-only is *always* available (recipes live in git forever), which is why §10.7's entire apparatus becomes moot.

**Reframing, not deletion:** the ELF glibc floor/ceiling check stops being an install *gate* (you built against your own glibc — floor ≤ ceiling by construction) but stays valuable as post-build verification: catching recipes that linked against the build-host libc instead of the sysroot, and `pkg check` diagnostics. DT_NEEDED extraction becomes more important, not less — it's your ground truth for recorded runtime dependency edges.

---

## Part 4 — Recommended next steps (roadmap to a functional MVP)

The workspace doc's write order is sound. These adjustments are driven by the findings above.

**Phase 0 — Unblock the foundations (spec decisions, ~a day of thinking, not code):**
1. Decide lock scope across builds (#2). Everything in `pkg-build` depends on the answer.
2. Fix the `[dkms]` representation everywhere (#5) or defer DKMS entirely (recommend deferral — see below).
3. Resolve the glibc-fallback contradiction (#11).
4. Decide recipe/source integrity (#G2) — even a minimal "signed recipe index" decision now saves rework.
5. Formally drop §10.2/§10.7/§10.8; note the §18.2 bootstrap-tarball provenance question.

**Phase 1 — Make pkg-core compile and correct:**
6. Land the workspace `Cargo.toml`s with pinned deps (`pubgrub`, `fs4`, `rusqlite` with `bundled` for the musl-static goal, `goblin`, `serde`, `toml`).
7. Fix `Version` normalization (#1) + property tests (parse → cmp → hash round-trips).
8. Unify `Dependency`/`VersionConstraint`/`VirtualName` in pkg-core (#9), add serde impls, add a `Range` constraint variant.
9. Fix the empty-OR-alternative parse (#14); reconcile workspace doc field names with the code.

**Phase 2 — Database with correct semantics:**
10. `schema.rs` + `queries.rs` together, fixing: arch-filtered store scans (#6), runtime-kind reverse deps (#7), `NOT EXISTS` autoremove + the iterative closure driver, transactional `regenerate_orphans`, affected-rows checks, `FromStr` for `DependencyKind`, schema_meta singleton.
11. `lock.rs` last in this phase, implementing whatever Phase-0 decided (#2, #15).

**Phase 3 — Resolver, actually wired:**
12. Candidate filtering at `Catalog` registration with pins + profiles + installed-set inputs (#3).
13. Fix abi-track semantics and the abi_tag-availability contract (#4).
14. Represent per-kernel DKMS edges in the data model, or cut DKMS from v1 (#5-C2).
15. Single-solve explain path (return the `NoSolution` tree from the resolver instead of re-solving).

**Phase 4 — Build orchestration (the actual product):** sandbox profile, recipe execution, staging/activation, post-build ELF verification, DB commit. This is where the project becomes *usable* — everything before it is plumbing. Consider deferring: glibc swap (`glibc-apply`/initramfs), DKMS, multilib, security enforcement mode. A v1 that does *resolve → fetch → sandboxed build → verify → stage → activate → record* for ordinary packages proves the architecture; the exotic paths each deserve their own design pass afterward.

**Testing posture:** the current tests lean on a live `cc` and single-arch assumptions — add multilib fixtures, malformed-ELF truncation tests, and property tests around `Version` and pin semantics (the two places subtle ordering bugs live).

**One cross-cutting note:** scope is the project's biggest risk. PubGrub + explanations + multilib + DKMS + glibc hot-swap + TUF-lite + sandboxing is several person-years if built all at once. The spec's phased plan already knows this — the review strongly suggests moving DKMS, glibc major-bump swap, and enforcement-mode audit out of the critical path for "functional."
