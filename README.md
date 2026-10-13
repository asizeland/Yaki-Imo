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
19-recipe ports tree from zlib to bash, plus repackaged binary blobs
like Discord), up to booting a Yakimo-installed sysvinit as PID 1 in a
user namespace. What has not happened is the bootstrap: glibc and the
toolchain are still host wrappers, so no live system has been installed
or rebuilt itself with Yakimo yet. That is the next milestone, and the
ports tree, the chrootable set, and the user guide exist specifically
to rehearse it.

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

## The honest downsides

It is unproven where it matters most: nothing has bootstrapped or run a
live system, and the only host it has ever been tested on is the
developer's. The ports tree is a seed, not a distribution — dozens of
packages where thousands are needed, with known gaps (no libxcrypt, no
Python/meson/ninja chain, no real glibc build). Source-only means every
install is a compile; the cache and parallelism knobs soften but do not
remove that. The safety machinery (signing, audit, undo) has been
exercised against harnesses and adversarial tests, not against years of
real-world misuse. And the bus factor is currently one developer plus
an AI assistant — the docs are fresh, the community is nonexistent, and
anyone adopting it today is signing up to help find the bugs the e2e
suite does not know about yet.

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
