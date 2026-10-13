# ports/ — the Yakimo seed package tree

A small, deliberately diverse recipe tree (ports-style) used to test
Yakimo on a live system before bootstrap. Selection criteria: cover
different build systems and program kinds while keeping the dependency
graph closed over `glibc` (the bootstrap boundary — the toolchain
itself is not packaged yet; recipes build with the host's compiler and
tools).

## Layout

```
<category>/<name>/
  pkg-meta.toml    # manifest: name, version, deps, [options], [build]
  source.toml      # upstream source pins: [[source]] url + sha256
  build            # build phase script (required)
  install          # install phase script (required, fills $PKGDIR)
  prepare / check  # optional phases
```

## The set

| Package | Kind | Build style | Notes |
|---|---|---|---|
| sys-libs/zlib | C library | custom `configure` | foundational dep |
| sys-libs/bzip2 | C library + tools | plain Makefile | separate shared-lib target |
| sys-libs/ncurses | C library + data | full autotools | compiles terminfo via its own `tic` |
| sys-libs/readline | C library | autotools | links ncurses (`SHLIB_LIBS`) |
| dev-db/sqlite | library + CLI | autoconf | `[options.fts5]` — `abi_affecting = false` demo |
| dev-lang/lua | interpreter | plain Makefile | `[options.readline]` — `add_deps` demo |
| sys-apps/less | TUI program | autotools | links ncurses |
| sys-apps/man-pages | data only | none (install-only) | exercises the minimal-phase path |
| games-misc/tty-solitaire | game | plain Makefile | ncurses; the distro is meant for gaming after all |
| net-im/discord | binary blob | repackaging | `[build] binary = true`; **version pin expires** — bump `pkg-meta.toml` + `source.toml` together |
| app-shells/bash | shell | autotools | the chrootable set's `/bin/sh` equivalent |
| sys-apps/coreutils | core tools | autotools | large configure+compile |
| sys-apps/grep | text tool | autotools | `--disable-perl-regexp` keeps pcre2 out of the closure |
| sys-apps/sed | text tool | autotools | |
| sys-apps/gawk | text tool | autotools | |
| sys-apps/tar | archiver | autotools | |
| sys-apps/gzip | compressor | autotools | |
| sys-apps/xz | compressor + liblzma | autotools | watched via git tags |

The bottom eight are the **chrootable set**: with them installed, the
root can be chrooted into (`arch-chroot`-style, after mounting
/proc /sys /dev) and testing continues inside — `e2e/ports.sh` proves
this with a bwrap chroot-equivalence run.

## Turning the tree into a repository

```sh
# 1. Download the pinned upstream sources.
sh ports/fetch-distfiles.sh /path/to/distfiles

# 2. Build a signed local repo (also stages recipes/ with commits).
cargo run --offline -p pkg-repo --example ports2repo -- \
    /path/to/repo http://127.0.0.1:8778 --distfiles /path/to/distfiles ports

# 3. Serve it.
python3 -m http.server 8778 --directory /path/to/repo &

# 4. Install recipes into a root and sync.
sh ports/install.sh /path/to/repo            # honors $PKG_ROOT
pkg key trust-root /path/to/repo/root.json
pkg sync
pkg install lua less tty-solitaire
```

> The generated repo uses deterministic **test keys** — it is for
> localhost and pre-bootstrap testing, not distribution.

Adding a package: copy the closest existing recipe, set the manifest,
pin the source in `source.toml` (`sha256sum` the tarball), re-run the
four steps above. Recipe rules to keep in mind: dependencies are
additive-only, options are declared booleans exported as
`PKG_OPT_<NAME>`, sources are fetched only if pinned in the repo
manifest, and the scanner cross-checks soname linkage against declared
`link` deps.

## Keeping versions current (watch.toml)

Recipes always pin an exact version plus a distfile SHA-256 — that pin
is what makes builds reproducible and what the attestation chain signs,
so there is deliberately no "latest" version grammar a client could
follow. What *is* automated is the bump itself: a recipe may carry a
`watch.toml` describing how to poll its upstream, and

```sh
ports/watch.sh           # report which recipes are behind upstream
ports/watch.sh --apply   # bump pkg-meta.toml + source.toml in place
```

walks the tree and reports (or applies) available updates — the same
role Debian's `uscan` plays for `debian/watch`. `--apply` fetches the
new distfile to compute its hash, so a bump is a complete, reviewable
tree change that flows through the normal attestation path.

`watch.toml` is flat TOML:

```toml
type = "git-tags"                     # or "release-regex"
url = "https://github.com/madler/zlib.git"
tag_prefix = "v"                      # git-tags: stripped from tag names
regex = 'lua-([0-9.]+)\.tar\.gz'      # release-regex: group 1 = version
stable_regex = '^[0-9]+\.[0-9]+(\.[0-9]+)?$'   # optional filter
url_template = "https://zlib.net/fossils/zlib-{version}.tar.gz"
filename_template = "..."             # optional staged-distfile name
```

Two recipes intentionally have **no** watch.toml: `sqlite` (its archive
URL uses a concatenated version form — 3.45.3 → 3450300 — that a regex
poll can't map back without a transform hook) and `discord` (the vendor
deletes old tarballs and publishes no version listing; the pin there is
expiry-prone by nature and must be bumped by hand).
