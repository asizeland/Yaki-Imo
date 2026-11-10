# Yaki Imo (pkg)

In summary: never quite found a linux distribution which felt natural, ironically 
FreeBSD achieved that for me, tooling feels coherent, intuitive and natural, but it also
amplifies the aspect of linux: Lack of support and incompatibilty. 

I am no programmer, but I had an idea and decided to attempt my own PM. 
Largely AI slop at the moment, as my life allows me I will replace this with
100% human, organic, unadulterated slop. All the ideas, mechanics and features
are of my own slop.

Yaki Imo is a source-build-only Linux package manager which tries to solve the issues
I have experienced in other distributions. 

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
to create a live system. Works correctly in containers. 

Past the bootstrap, the roadmap is a set of deliberately deferred
designs. A **binary repository** is the big one: deferred indefinitely 
on paper, but the groundwork exists builds are content-addressed and 
reproducible with recorded provenance, so "the artifact your build would 
have produced, fetched instead of compiled" is an evolution 
of the store, not a new subsystem; what is missing is 
the index, the signatures, and the trust policy around them. 
**Reproducibility verification tiers** (multi-builder cross-checking) 
are deferred in favor of a local double-build of the critical set (optional). 
**DKMS integration** for out-of-tree kernel modules is specified 
(a canonical `[dkms]` recipe table) but unimplemented. 
**Binary packages** will eventually be available, for standardized builds
or for longer builds, cutting down compilation/installation times.

The number of packages should be fairly easy to increase and in the 
future maintain due to Yaki-Imo's packaging mechanics.

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
learned from your own build history. 

Builds run sandboxed and unattended. When something goes wrong, the failure
is meant to be a sentence you can act on — an undeclared dependency
naming its provider, a conflicting option naming both requesters — and
the recovery is `pkg undo`, not an afternoon of reinstalling. 

Curiosity is answered too: `pkg explain` walks you through the
resolver's reasoning, `pkg history` shows what the system has done, and
`pkg audit` maps installed packages against security advisories. 

Some of the polish is deliberately deferred and you will notice the seams:
rebuild cascades can be postponed (`pkg upgrade --skip-rebuilds`, with
a deliberately un-ergonomic `--defer-security=I-understand` guard when
an advisory is open; the spec's config-side `[defer]` lists are
written down but not wired up yet), reproducibility assurance
is a local double-build of the critical set rather than full
multi-builder verification tiers, and DKMS for out-of-tree kernel
modules is designed but not implemented.

## The honest downsides

This is a prototype as it stands, ongoing work on creating more recepies
for packages. Furthermore, at the moment these are currently distrubuted 
as a ports tree of sorts. Not within an actual repository, this is something
I will be working on and will come alongisde binary redistributables. 

Once again, I am not a programer, I have used AI. I am actively learning
rust for this project and perhaps a couple more things in the future.
I would be happy to welcome anyone with interest in the project into it
or to welcome any good ideas.

## The package manager, from a few angles

**Why source-based?** Because what you run should be what the recipe and
the upstream source produced — no trusting a build farm you can't see.
It also makes build options meaningful: a binary repo ships someone
else's choices; here, `foo[readline]` is built for *your* system. The
price is compile time, reduced with a compiler cache, the ability to
control threads being used, and learned build-time estimates to not leave 
you guessing.

**How is it different from Gentoo's Portage?** Same philosophy,
stricter mechanics. To provide some of the flexibility of useflags with
more and consistency predictability.

**How is it similar to Nix?** It borrows Nix's build correctness, sandboxed
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
act on.

**What about packaging your own software?** This is not an
afterthought. A common source recipe needs only `pkg-meta.toml`: it holds
package metadata, SHA-256-pinned `[[source]]` entries, and a managed
`[build] system` for Autotools, Meson, CMake, or Make. Optional inline
`[scripts]` handle custom steps; older `source.toml` files and external
shell phase files remain supported.

The build pipeline helps you get it right: dependencies appear at stable paths,
options arrive as `PKG_OPT_*` variables, and if your binary links
something you didn't declare, the build fails and names the package
that provides it.
