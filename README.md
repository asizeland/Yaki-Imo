# Yakimo (pkg)

Yakimo is a source-build-only Linux package manager (see SPEC.md for the
binding interface contracts; the user-facing distribution spec is
pkg-spec-v14-consolidated.md). The workspace crates and CLI keep their
`pkg-*` / `pkg` names. Crates: pkg-core (types), pkg-resolver (pubgrub),
pkg-db (SQLite), pkg-sandbox (bwrap), pkg-scanner (ELF/heuristics),
pkg-repo (signed index/fetch), pkg-build (orchestration), pkg-audit
(security/supply-chain), pkg-init (musl-static glibc-apply), pkg-cli (clap).

Build the musl-static release binaries (pkg / pkg-init):

    cargo build --release --target x86_64-unknown-linux-musl

Run the test suite:

    cargo test --workspace

Run the end-to-end harness (fabricated signed repo, sandboxed builds,
all state under a throwaway `/tmp/pkg-e2e` root):

    sh e2e/run.sh

## Where the project stands

The intended goal is a complete, self-hosting source-based distribution:
a signed ports tree that builds everything from a real glibc upward,
installed and maintained by Yakimo itself. The current state is the
package manager, not yet the distribution. The machinery the spec
promises is implemented and exercised end to end — PubGrub resolution
with option pseudo-dependencies, signed repositories and pinned sources,
sandboxed reproducible builds with provenance, atomic store-and-symlink
activation, generations and undo, dependency-directed provider hints for
packagers — and it has been proven on real upstream software (a
53-recipe ports tree from glibc to NetworkManager, plus repackaged binary blobs
like Discord). The first bootstrap stage is done: Yakimo builds real
glibc 2.42 from source on top of kernel headers and libxcrypt it also
built, and the result is a self-contained root — the chroot-equivalence
suite and a Yakimo-installed sysvinit running as PID 1 in a user
namespace pass with no host libraries bound in. What remains is the
rest of the bootstrap: the compiler and binutils are still host
wrappers (stage 2 is packaging them), and no live machine has been
installed or rebuilt itself with Yakimo yet. That live boot is the
next milestone, and the ports tree, the chrootable set, and the user
guide exist specifically to rehearse it.

Past the bootstrap, the roadmap is a set of deliberately deferred
designs rather than blank space. A **binary repository** is the big
one: deferred indefinitely on paper, but the groundwork is already
load-bearing — builds are content-addressed and reproducible with
recorded provenance, so "the artifact your build would have produced,
fetched instead of compiled" is an evolution of the store, not a new
subsystem; what is missing is the index, the signatures, and the trust
policy around them. **Reproducibility verification tiers**
(multi-builder cross-checking) are deferred in favor of a local
double-build of the critical set. **DKMS integration** for out-of-tree
kernel modules is specified (a canonical `[dkms]` recipe table) but
unimplemented. And the ports tree itself is the largest piece of
future work: from nineteen recipes toward a coverage set, with
`watch.toml` automation already in place to keep versions current once
the tree grows. None of these are promises with dates; they are the
shape of where the project goes once a machine boots itself with it.

## Why it might be worth your attention

The short version: Gentoo's flexibility with a modern resolver and a
real transaction model. USE-flag-style build options exist, but they
are declared booleans the resolver reasons about (conflicts fail at
solve time with an explanation, not mid-compile), they are visible
(`pkg options` shows every knob, its default, and where your setting
came from), and flipping one is a normal, undoable transaction rather
than a flag-day rebuild you hope works. Every install is an atomic,
content-addressed store swap with a recorded generation, so `pkg undo`
is a first-class citizen. And the build pipeline is actively on the
packager's side: when your recipe links a library you did not declare,
the build fails and tells you which installed package provides it.
Source-only does not have to mean opaque, fragile, or unverifiable —
that combination is the bet this project makes.

What that should feel like in daily use: `pkg install` answers with a
plan, not a leap of faith — what will be built, in what order, why each
package is there (`pkg why`), and how long it should take, with ETAs
that get *better* the longer you use the system because they are learned
from your own build history. Builds run sandboxed and unattended; you
watch progress, not scrollback. When something goes wrong, the failure
is meant to be a sentence you can act on — an undeclared dependency
naming its provider, a conflicting option naming both requesters — and
the recovery is `pkg undo`, not an afternoon of reinstalling. Curiosity
is a first-class workflow too: `pkg explain` walks you through the
resolver's reasoning, `pkg history` shows what the system has done, and
`pkg audit` maps installed packages against security advisories. Some
of the polish is deliberately deferred and you will notice the seams:
rebuild cascades can be postponed (`pkg upgrade --skip-rebuilds`, with
a deliberately un-ergonomic `--defer-security=I-understand` guard when
an advisory is open; the spec's config-side `[defer]` lists are
written down but not wired up yet), reproducibility assurance
is a local double-build of the critical set rather than full
multi-builder verification tiers, and DKMS for out-of-tree kernel
modules is designed but not implemented — so today, kernel module
building is on you.

## The honest downsides

It is unproven where it matters most: nothing has booted or run a
live system, and the only host it has ever been tested on is the
developer's. The ports tree is a seed, not a distribution — dozens of
packages where thousands are needed, with known gaps (the compiler and
binutils are still host wrappers, no Python/meson/ninja chain). Source-only means every
install is a compile; the cache and parallelism knobs soften but do not
remove that. The safety machinery (signing, audit, undo) has been
exercised against harnesses and adversarial tests, not against years of
real-world misuse. And the bus factor is currently one developer plus
an AI assistant — the docs are fresh, the community is nonexistent, and
anyone adopting it today is signing up to help find the bugs the e2e
suite does not know about yet.

## The package manager, from a few angles

**What is it, in one breath?** A source-based package manager for a
conventional FHS Linux system. A signed recipe tree (ports-style)
describes software; Yakimo resolves a plan, fetches pinned sources,
builds each package in a network-less sandbox, and activates the result
by swapping symlinks into a content-addressed store. The command is
`pkg`.

**Why source-only?** Because what you run should be what the recipe and
the upstream source produced — no trusting a build farm you can't see.
It also makes build options meaningful: a binary repo ships someone
else's choices; here, `foo[readline]` is built for *your* system. The
price is compile time, paid down with a compiler cache, parallelism
knobs, and learned build-time estimates so you at least know the price
before you pay it.

**How is it different from apt, dnf, or pacman?** Those distribute
binaries; installation is download-and-unpack, and "what options was
this built with" is the distributor's business. Yakimo builds on your
machine, so options are *your* business — and because every transaction
is an atomic store swap with a recorded generation, there is no
half-installed state to recover from: there is only the old generation
and the new one.

**How is it different from Gentoo's Portage?** Same philosophy,
stricter mechanics. USE flags exist, but as declared boolean options the
resolver reasons about: a requirement like `foo[x11=1]` is a
pseudo-dependency, so two packages wanting incompatible variants fail at
solve time with both requesters named — not at hour three of a compile.
Options are visible (`pkg options`, `pkg info`), splittable across
config files, and flipping one is an ordinary undoable transaction.
Raw `CFLAGS` injection is deliberately forbidden: toolchain flags are
the distribution's ABI contract, not a user knob.

**Is it Nix?** No. It borrows Nix's *build correctness* — sandboxed
builds with a closed input set, pinned sources, recorded provenance,
an immutable store — but not its worldview. The system is a normal FHS
tree of symlinks into the store, not a store-path universe; identity is
`name-version+abi+opt_tag`, not an input hash; and immutability here is
a means to atomic transactions and undo, not an end in itself.

**What stops me from running garbage?** The chain is signed end to end:
the repository index is signed, sources are fetched only if the repo
pins their SHA-256, the sandbox has no network, and the build records
its provenance (recipe commit, source hashes, builder) into the
database. `pkg audit` then maps what you have installed against
security advisories. The development placeholder keys in the tree are
exactly that — a real deployment embeds a real root key set.

**What happens when something breaks?** You go back. Every transaction
records a generation; `pkg undo` restores a previous one. Integrity is
checkable (`pkg check`), the database is inspectable (`pkg db check`),
and when a build fails the error is designed to be a sentence you can
act on, not a log to excavate.

**Can I package my own software?** Yes — that is a design goal, not an
afterthought. A recipe is a small directory: a `pkg-meta.toml`
manifest, a `source.toml` pin, and plain `sh` phase scripts. The build
pipeline helps you get it right: dependencies appear at stable paths,
options arrive as `PKG_OPT_*` variables, and if your binary links
something you didn't declare, the build fails and names the package
that provides it. The user guide's packaging chapter walks the loop.

**Who is this for?** Someone building or running a hobbyist,
gaming-friendly source distribution who wants Gentoo-level control
without resolver opacity — and who is willing to trade install speed
for verifiability and to help shake down a young system. If you want
five-thousand packages and five-second installs today, this is not
that yet.

## Configuration

System config lives in `/etc/pkg/config.toml`. Supported sections:

```toml
# Compiler cache wrapper for builds ("ccache"/"sccache"/"off").
[cache]
compiler = "ccache"

# Build parallelism: total compile jobs across the machine (default:
# core count) and the per-package `make -j` cap (default: 4).
# `pkg install --jobs N` overrides `jobs` for one invocation.
[build]
jobs = 8
max_pkg_jobs = 4

# Relocate individual state paths from their spec (FHS-style)
# defaults. Keys are the default absolute paths; values must be
# absolute.
[paths]
"/var/lib/pkg/cache" = "/bigdisk/pkg-cache"
```

For testing, `PKG_ROOT=/some/dir` prefixes every state path (including
`[paths]` overrides) so the whole toolchain can run against a
throwaway root.

## License

BSD 3-Clause (see [LICENSE](LICENSE)).
