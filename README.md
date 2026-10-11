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

## Configuration

System config lives in `/etc/pkg/config.toml`. Supported sections:

```toml
# Compiler cache wrapper for builds ("ccache"/"sccache"/"off").
[cache]
compiler = "ccache"

# Relocate individual state paths from their spec (FHS-style)
# defaults. Keys are the default absolute paths; values must be
# absolute.
[paths]
"/var/lib/pkg/cache" = "/bigdisk/pkg-cache"
```

For testing, `PKG_ROOT=/some/dir` prefixes every state path (including
`[paths]` overrides) so the whole toolchain can run against a
throwaway root.
