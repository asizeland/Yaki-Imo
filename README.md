# pkg

Source-build-only package manager workspace (see SPEC.md for the binding
interface contracts). Crates: pkg-core (types), pkg-resolver (pubgrub),
pkg-db (SQLite), pkg-sandbox (bwrap), pkg-scanner (ELF/heuristics),
pkg-repo (signed index/fetch), pkg-build (orchestration), pkg-audit
(security/supply-chain), pkg-init (musl-static glibc-apply), pkg-cli (clap).

Build the musl-static release binaries (pkg / pkg-init):

    cargo build --release --target x86_64-unknown-linux-musl

Run the test suite:

    cargo test --workspace
