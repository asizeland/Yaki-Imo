# ports/ — the Yakimo seed package tree

A small, deliberately diverse recipe tree (ports-style) used to test
Yakimo on a live system before bootstrap. Selection criteria: cover
different build systems and program kinds. Since the stage-1 bootstrap
work, the tree includes the C library itself: `glibc` is built from
source against the packaged `linux-headers`. What is still borrowed
from the host is the *compiler* — recipes build with the host's gcc
and binutils; packaging the toolchain (binutils, gcc) and rebuilding
it against the packaged glibc is the stage-2 bootstrap.

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
| sys-kernel/linux-headers | kernel UAPI headers | `make headers` + copy | prerequisite for glibc |
| sys-libs/glibc | C library + loader | autotools (separate build dir) | **the real thing** — `slibdir=/usr/lib` makes /usr/lib the loader's compiled-in default search path; `/lib64/ld-linux` is a symlink |
| sys-libs/libxcrypt | C library | autotools | libcrypt; re-enables sysvinit's `sulogin` |
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
| sys-apps/sysvinit | init system | plain Makefile | links libxcrypt (`sulogin`); boots as PID 1 |
| dev-util/pkgconf | pkg-config implementation | autotools | `pkg-config` symlink; meson/autotools dep discovery |
| dev-libs/openssl | TLS/crypto library | custom `Configure` (perl) | `--libdir=lib` — the linux-x86_64 default is lib64 |
| dev-libs/expat | XML parser | autotools | |
| app-misc/ca-certificates | data only | none (install-only) | Mozilla CA bundle at /etc/ssl/certs |
| net-misc/curl | HTTP client + libcurl | autotools | openssl + zlib; CA bundle path pinned at configure |
| dev-vcs/git | VCS | plain Makefile | `NO_PERL=1` (perl unpackaged); https remote via packaged curl — proven by a real TLS fetch; phase flags must match (GIT-CFLAGS rebuild trap) |
| sys-apps/util-linux | system tool kit | autotools | mount/blkid/fdisk + libuuid/libblkid/libmount; `--libdir=/usr/lib` (its default /lib is off the loader path); two tools scripts need a shebang repoint |
| sys-apps/iproute2 | network tools | custom configure + Makefile | ip/ss/tc; tc needs bison+flex |
| sys-apps/procps-ng | process tools | autotools | ps/top/free; `CC='gcc -std=gnu99'` is forced by its configure, so stdbool.h is force-included |
| sys-fs/e2fsprogs | ext4 tools | autotools (separate build dir) | mkfs.ext4/e2fsck; uses util-linux's libuuid/libblkid |
| net-misc/openssh | SSH client+server | autotools | no PAM; sshd runtime setup is a live-system step |
| dev-lang/python3 | interpreter | autotools | ssl/ctypes/zlib/expat modules link the packaged libs |
| dev-libs/libffi | C library | autotools | python3 ctypes + glib |
| dev-util/meson | build system | install-only | python3 payload + relocatable wrapper (works live and in the sandbox) |
| dev-util/ninja | build tool | bootstrap script (C++) | `-static-libstdc++` — no packaged libstdc++ until stage 2 |
| dev-libs/pcre2 | regex library | autotools | glib's regex backend |
| dev-libs/glib | C utility library | meson | the first meson consumer in the tree |
| sys-apps/dbus | message bus | meson | system bus; NM talks to wpa_supplicant over it |
| sys-apps/kmod | kernel module tools | meson | modprobe/insmod in /usr/sbin; PKCS#7 via openssl |
| sys-fs/eudev | device manager | autotools | libudev + udevadm |
| dev-libs/gmp | bignum library | autotools | `CC="cc -std=gnu17"` — GCC 15's C23 default rejects its K&R configure probes |
| dev-libs/nettle | crypto primitives | autotools | gnutls backend |
| dev-libs/libtasn1 | ASN.1 library | autotools | ships asn1Parser (p11-kit's build needs it on PATH) |
| dev-libs/p11-kit | PKCS#11 glue | meson | trust paths pinned at the packaged CA bundle |
| dev-libs/libunistring | Unicode library | autotools | gnutls dep |
| net-libs/gnutls | TLS library | autotools | `--disable-cxx` (libtool's C++ hardcode probing misfires in the sandbox); trust store pinned at /etc/ssl/certs |
| dev-libs/libnl | netlink library | autotools | wpa_supplicant's nl80211 driver |
| dev-libs/jansson | JSON library | autotools | NM config parsing |
| net-libs/libndp | IPv6 ND library | hand-compiled | upstream ships git checkouts without configure; single-TU library built directly (soname from its version-info) |
| net-wireless/wpa_supplicant | Wi-Fi auth daemon | .config + Makefile | defconfig + `CONFIG_TLS=openssl`; D-Bus activation file installed |
| net-misc/networkmanager | network daemon | meson | sysvinit/eudev profile: no systemd/polkit, keyfile plugin, internal DHCP, gnutls crypto |

The bottom eight (bash through xz) are the **chrootable set**: with
them installed, the root can be chrooted into (`arch-chroot`-style,
after mounting /proc /sys /dev) and testing continues inside —
`e2e/ports.sh` proves this with a bwrap chroot-equivalence run, and
goes one step further: with `sysvinit` installed it boots the tree's
`/sbin/init` as PID 1 inside a user namespace and verifies the inittab
sysinit action runs.

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
