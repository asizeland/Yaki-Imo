# Yaki Imo (pkg)

In summary: never quite found a linux distribution which felt natural, ironically 
FreeBSD achieved that for me, tooling feels coherent, intuitive and natural, but it also
amplifies the aspect of linux: Lack of support and incompatibilty. Regardless a beloved
OS and project of mine. Gentoo linux has a special place in my heart because to me, within
linux it replicates similar feelings and more than ever you can fine-tune your system
in some manner. NixOS is probably the single most pragmatic distribution one could
design, centralized control of a system from a file and all sorts, but the
language itself in things such has flakes, it feels a bitoverwhelming. Furthermore I get an 
error and half the time I am left squinting at my screen and wondering what the fuck
something means when I am trying to rebuild a system.

Anyways, so I am no programmer, but I had an idea and decided to do anything to attempt
my own PM. Largely AI slop at the moment, as my life allows me I will replace this with
100% human, organic, unadulterated slop. 

Yaki Imo is a source-build-only Linux package manager (see SPEC.md for the
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

The intended goal is a complete distribution. The current state is the
package manager, not yet the distribution. The machinery the spec
promises is implemented and exercised end to end — PubGrub resolution
with optional pseudo-dependencies, signed repositories and pinned sources,
sandboxed reproducible builds with provenance, atomic store-and-symlink
activation, generations and undo, dependency-directed provider hints for
packagers — and it has been proven on real upstream software from glibc to 
bash, plus repackaged binary blobs like Discord, Steam ... The first bootstrap 
stage is done: Yaki Imo builds real glibc 2.42 from source on 
top of kernel headers and more, installed correctly and working. 
I am currently progressing in this area, packaging more software 
to create a live system. 

Past the bootstrap, the roadmap is a set of deliberately deferred
designs. A **binary repository** is the big one: deferred indefinitely 
on paper, but the groundwork exists builds are content-addressed and 
reproducible with recorded provenance, so "the artifact your build would 
have produced, fetched instead of compiled" is an evolution 
of the store, not a new subsystem; what is missing is 
the index, the signatures, and the trust policy around them. 
**Reproducibility verification tiers** (multi-builder cross-checking) 
are deferred in favor of a local double-build of the critical set. 
**DKMS integration** for out-of-tree kernel modules is specified 
(a canonical `[dkms]` recipe table) but unimplemented. 

The number of packages should be fairly easy to increase and in the 
future maintain due to how Yaki Imo assists with with finding undeclared
dependencies in a recepie and how it helps maintain a recepie by
updating them to newer versions of a program. This should also mean 
offering and retaining multiple version binaries in repos is possible,
need to study this (however say foobarlib is installed but also foobarlib21
is installed), I need to see how this can be managed effectively. 
Similarly it should mean that with the additive only useflags policy,
enabling a useflag will not require compilation and instead simply
result in another package being added as a dependency for the program. 

## Why it might be worth your attention

The short version: Gentoo's flexibility with a modern resolver and a
real transaction model. USE-flag-style build options exist, but they
are declared booleans the resolver reasons about (conflicts fail at
solve time with an explanation), they are visible (`pkg options` shows 
every knob, its default, and where your setting came from), 
and flipping one is a normal, undoable transaction rather
than a flag-day rebuild you hope works. Every install is an atomic,
content-addressed store swap with a recorded generation, so `pkg undo`
is a first-class citizen. And the build pipeline is actively on the
packager's side: when your recipe links a library you did not declare,
the build fails and tells you which installed package provides it.

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

This is a prototype as it stands, ongoing work on creating more recepies
for packages. Furthermore, at the moment these are currently distrubuted 
as a ports tree of sorts. Not within an actual repository, this is something
I will be working on and will come alongisde binary redistributables. 

Obvsiouly it will lack the extensive testing any of the package managers which 
have existed for a long time will have, with this unexpected quirks or behaviour
when installing different software. Currently all package recepies which are 
created are tested on creation. I will keep doing this in the forseeable future,
so at the very least any software which is packaged should work. However
certain software might uncover bugs in the package manager.

Once again, I am also not a programer, I have used AI. I am actively learning
rust for this other project and perhaps a couple more things in the future.
I would be happy to welcome anyone with interest in the project into it
or to welcome any good ideas.

## The package manager, from a few angles

**Why source-based?** Because what you run should be what the recipe and
the upstream source produced — no trusting a build farm you can't see.
It also makes build options meaningful: a binary repo ships someone
else's choices; here, `foo[readline]` is built for *your* system. The
price is compile time, paid down with a compiler cache, parallelism
knobs, and learned build-time estimates so you at least know the price
before you pay it.

**How is it different from apt, dnf, or pacman?** What options was
this built with is the distributor's business. Yaki Imo builds on your
machine (and in the future, binaries will adapt to your configuration), 
so options can be customized for your purpose and every transaction
is an atomic store swap with a recorded generation, there is no
half-installed state to recover from: there is only the old generation
and the new one. Furthermore, it allows you to not have issues with old
libraries or packages, or libraries upgrading causing certain packages to fail.

**How is it different from Gentoo's Portage?** Same philosophy,
stricter mechanics. To provide the flexibility of useflags with
more and consistency predictability. In my experience, while I loved 
Gentoo it would often behave in ways that I could not explain. A lot of the
design decisions made with the idea in mind of creating consistent and 
predictable results when possible.

**How is it similar to Nix?** It borrows Nix's *build correctness* — sandboxed
builds with a closed input set, pinned sources, recorded provenance,
an immutable store, but, the system is a normal FHS
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

**What about packaging your own software?** This is not an
afterthought. A recipe is a small directory: a `pkg-meta.toml`
manifest, a `source.toml` pin, and plain `sh` phase scripts (at the moment
I feel like these add unecessary complexity to creating packages, I will
aim to simplify this system such that only the somepkg.toml is required,
shell scrips will not have to be created by user this is a simplification 
being used at the moment). 
The build pipeline helps you get it right: dependencies appear at stable paths,
options arrive as `PKG_OPT_*` variables, and if your binary links
something you didn't declare, the build fails and names the package
that provides it. The user guide's packaging chapter explains in detail.

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
"/var/lib/pkg/cache" = "/somedisk/pkg-cache"
```

For testing, `PKG_ROOT=/some/dir` prefixes every state path (including
`[paths]` overrides) so the whole toolchain can run against a
throwaway root.

## License

BSD 3-Clause.
