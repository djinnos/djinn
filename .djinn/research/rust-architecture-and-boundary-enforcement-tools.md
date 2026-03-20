---
title: Rust Architecture and Boundary Enforcement Tools
type: research
tags: ["tools","rust","architecture","linting","module-boundaries"]
---

## Source
- Harvested: 2026-03-12
- Type: Tool survey

## Practical CI Stack (ordered by effort)

### Tier 1 — Zero config, immediate value
```
cargo check -D warnings                   # includes dead_code, unused_imports
#![warn(unreachable_pub)]                 # add to lib.rs — flags pub items that can't be reached externally
```

### Tier 2 — Install once, run in CI
| Tool | Purpose | Install | Nightly? |
|------|---------|---------|----------|
| **cargo-machete** | Fast unused dependency detection (ripgrep-based) | `cargo install cargo-machete` | No |
| **cargo-deny** | Dependency policy: licenses, bans, advisories, sources | `cargo install --locked cargo-deny` | No |
| **cargo-modules** | Module tree visualization, orphan detection, cycle detection | `cargo install cargo-modules` | No |

### Tier 3 — Deeper analysis
| Tool | Purpose | Install | Nightly? |
|------|---------|---------|----------|
| **cargo-public-api** | Track/diff public API surface between commits | `cargo install cargo-public-api` | Needs nightly installed |
| **cargo-udeps** | Authoritative unused dep detection (compiles project) | `cargo install cargo-udeps` | Yes |
| **cargo-depgraph** | External dependency graph visualization (Graphviz) | `cargo install cargo-depgraph` | No |

## Key Commands

```bash
# Module structure tree with visibility
cargo modules structure

# Internal module dependency graph (detect cycles)
cargo modules dependencies | dot -Tsvg > deps.svg

# Find orphaned source files not linked into module tree
cargo modules orphans

# Public API diff between commits
cargo public-api diff HEAD~1..HEAD

# Unused deps (fast, imprecise)
cargo machete

# Unused deps (slow, precise)
cargo +nightly udeps --all-targets
```

## Built-in Rustc/Clippy Lints for Boundaries

| Lint | What it catches | Enable |
|------|----------------|--------|
| `unreachable_pub` | `pub` items inside private modules (should be `pub(crate)`) | `#![warn(unreachable_pub)]` |
| `dead_code` | Unused items within crate | On by default |
| `clippy::wildcard_imports` | `use foo::*` glob imports | `#[warn(clippy::wildcard_imports)]` |

## What Does NOT Exist
- No Rust equivalent of Java's **ArchUnit** (executable "module A must not import module B" tests)
- No mature source-level code clone detection for Rust (use PMD CPD or SonarQube as stopgap)
- No tool enforcing hexagonal architecture boundaries declaratively

## Code Duplication
No dominant tool. Options: `cargo-dupes` (AST fingerprinting), `cargo-duplicated` (text scan) — both low activity. Language-agnostic tools (PMD CPD, simian) work on `.rs` files.

## Relations
- [[Deep Modules Pattern for AI Codebases]]
- [[Pit of Success Pattern for AI]]
