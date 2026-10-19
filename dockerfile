# syntax=docker/dockerfile:1
#
# dockerfile — Debian/glibc test container for the Yakimo package
# manager workspace. This is the image that can run the full e2e suite:
# the scripts detect the host *glibc* and wrap it as a package (the
# build gate requires a declared libc.so.6 provider), which only works
# on a glibc distro. Debian is also FHS-normal (/usr/bin toolset, /lib
# present), so unlike the NixOS dev host no toolchain-farm tricks are
# needed — the e2e bwrap shims pass straight through.
#
# The base is the official `rust` image (Debian trixie underneath),
# NOT bare Debian + apt rustc: the locked dependencies need a current
# stable toolchain (some require rustc ≥ 1.88 / edition2024 cargo),
# which is newer than any Debian release packages.
#
# (For the musl side — musl-static release binaries per SPEC §17 — use
# dockerfile.alpine instead; the e2e suites cannot run on musl.)
#
# Build:  docker build -f dockerfile -t yakimo-test .
# Shell:  docker run --rm -it --privileged yakimo-test
#         --privileged is required for the e2e suite: Yakimo's builds
#         run in bwrap sandboxes, and bwrap inside a container needs
#         to create nested user/mount namespaces and mount proc —
#         seccomp=unconfined and SYS_ADMIN alone are not enough. Only
#         use it on this disposable test image.
#
# Inside, from /src:
#   sh e2e/run.sh           # smoke: repo, signatures, install, gate
#   sh e2e/lifecycle.sh     # remove/autoremove, upgrade, undo, mirrors
#   sh e2e/options.sh       # build options, store GC, undo interplay
#   sh e2e/real-package.sh  # a real autotools package from the internet
#   sh e2e/real-binary.sh   # repackaged binary blob (needs network)
#   sh e2e/ports.sh         # the full 66-recipe tree from scratch —
#                           # long (hours), the pre-bootstrap dress rehearsal

# ---------- stage 1: build & unit-test ----------
FROM rust:1-trixie AS build

WORKDIR /src
COPY . .

RUN cargo test --locked --workspace
RUN cargo build --locked --release -p pkg-cli -p pkg-init

# ---------- stage 2: interactive e2e environment ----------
FROM rust:1-trixie

# The e2e scripts' tool list, mapped to Debian package names:
#   gcc/perl/bubblewrap/python3/coreutils/tar/gzip/make/sqlite/binutils/
#   sed/grep/gawk/diffutils — plus xz (ports tree tarballs), curl and
#   file for general poking. git + openssh-client: authoring and
#   pushing recipes from inside the container.
# bison/flex/gperf/gettext(msgfmt) are part of the build sandbox's
# host-tool contract (the e2e toolchain farm, e2e/ports.sh): glibc's
# configure needs bison, and several tree recipes need the rest. On the
# NixOS dev host these come from the scripts' `nix shell` line.
# (cargo/rustc/build-essential come with the rust base image.)
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential perl \
    bash bubblewrap python3 sqlite3 \
    coreutils tar xz-utils gzip binutils \
    sed grep gawk diffutils \
    bison flex gperf gettext \
    curl file ca-certificates \
    git openssh-client \
    && rm -rf /var/lib/apt/lists/*

# The built workspace (including target/) and the rustup/cargo homes
# (toolchain + registry cache), so the scripts' `cargo build --offline`
# just works.
COPY --from=build /src /src
COPY --from=build /usr/local/cargo /usr/local/cargo
COPY --from=build /usr/local/rustup /usr/local/rustup

# Binaries on PATH under their spec names.
RUN ln -sf /src/target/release/pkg-cli /usr/local/bin/pkg \
    && ln -sf /src/target/release/pkg-init /usr/local/bin/pkg-init

# A placeholder git identity so `git commit` works out of the box —
# override with your own (git config --global user.name/user.email),
# or mount your real ~/.gitconfig and ~/.ssh at run time when you want
# to push. safe.directory=* lets root work in bind-mounted repos owned
# by your host uid.
RUN git config --global user.name "Yakimo Tester" \
    && git config --global user.email "yakimo@localhost" \
    && git config --global init.defaultBranch main \
    && git config --global --add safe.directory "*"

# e2e scripts re-exec into `nix shell` unless this is set; there is no
# nix in this image by design.
ENV E2E_INNER=1

WORKDIR /src
CMD ["/bin/bash"]
