---
title: Rust Compilation and Tooling Optimization Strategy
type: 
tags: ["compilation","performance","tooling","worktree","cargo"]
---


# Rust Compilation and Tooling Optimization Strategy

## Source
- Researched: 2026-03-12
- Type: Multi-source synthesis (corrode.dev, Rust perf book, cargo-hakari docs, nextest docs, cargo-llvm-cov, mutants.rs, JetBrains State of Rust 2025)

## Baseline Metrics (2026-03-12)

| Metric | Value |
|--------|-------|
| Source files | 138 |
| Lines of Rust | ~45,500 |
| Direct deps | 38 (+4 dev) |
| Transitive deps | ~835 |
| Tests | 457 |
| Test runtime | ~12s |
| target/ size | 60 GB (52 GB debug) |
| Debug binary | 305 MB |
| Linker | Default cc (not mold/lld) |
| Dev profile | No customization |
| Workspace | Single crate |
| Worktree build cache | None |

## Core Problem

Every agent worktree compiles from scratch. `src/commands.rs` runs commands with `current_dir(worktree_path)` and no `CARGO_TARGET_DIR` override. Fresh `cargo test` in a worktree recompiles 835+ transitive deps.

Duplicate `reqwest` (0.12 + 0.13 via rmcp) means two full HTTP+TLS stacks compile.

## Tier 1: Immediate Wins

### Dev Profile Tuning
```toml
[profile.dev]
debug = "line-tables-only"    # 20-40% faster
[profile.dev.build-override]
opt-level = 3                 # faster proc-macros
```

### Mold Linker
```toml
[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=mold"]
```
2-5x faster linking on incremental builds.

### cargo-nextest
1.5-3x faster tests, process-per-test isolation, flaky retry.

### cargo-llvm-cov (replace tarpaulin)
LLVM native instrumentation. Fixes known async coverage misses in agent/ and actors/slot/.

## Tier 2: Worktree Build Cache Sharing

### Hardlink Approach (Recommended)
Hardlink immutable dependency artifacts from main worktree into agent worktrees. Benchmarked: cold build ~2m19s → <1s with hardlinks, ~0 extra disk.

Implementation: seed `target/debug/deps/` and `target/debug/.fingerprint/` for registry deps before setup commands run.

### sccache Alternative
`SCCACHE_BASEDIRS` strips path prefix for cross-worktree sharing. But disables incremental compilation (mutually exclusive). Cannot cache linker invocations.

### Shared CARGO_TARGET_DIR
Cargo takes exclusive file lock — serializes parallel builds. Only for sequential workflows.

## Tier 3: Workspace Splitting

Natural boundaries from existing modules:
- djinn-models (types, serde, schemars)
- djinn-db (sqlx, repositories, migrations)
- djinn-agent (LLM provider, sessions, sandbox)
- djinn-mcp (rmcp, tool handlers)
- djinn-server (axum, SSE, binary entry point)

Facade work from er9m creates the seam. After pub(crate) sweep, extraction is mechanical.
Add cargo-hakari immediately after first split.

## Tier 4: Tool Recommendations

| Tool | Purpose | Priority |
|------|---------|----------|
| cargo-nextest | Faster test runner | High |
| cargo-llvm-cov | Accurate async coverage | High |
| cargo-insta | Snapshot testing for MCP | High |
| cargo-audit | CVE scanning | High |
| cargo-deny | License + dup checking | High |
| cargo-mutants | Mutation testing | Medium |
| cargo-machete | Unused dep detection | Medium |
| cargo-semver-checks | API breakage linting | Medium |
| cargo-public-api | Visibility validation | Medium |
| cargo-auditable | Dep manifest in binary | Medium |

## Tier 5: Experimental (Nightly)

- Cranelift codegen: ~75% faster incremental, but `ring` uses asm! — verify compatibility
- Parallel frontend (-Zthreads=8): up to 50% reduction on large crates

## Key Insights

- sccache and incremental compilation are mutually exclusive
- RUSTFLAGS divergence silently invalidates entire incremental cache
- LLD is default Linux linker since Rust 1.90, but mold is faster
- cargo-hakari eliminates feature-unification duplicates in workspaces
- Workspace splitting + incremental compilation are complementary (different granularity levels)

## Relations

- [[ADR-028 Module Visibility Enforcement and Deep Module Architecture]]
- [[Deep Modules Pattern for AI Codebases]]
- [[reference/cognitive-memory-scope|Cognitive Memory Scope]]
