---
name: go-best-practices
description: Go style and best practices guide synthesized from Google, Uber, and official Go documentation. This skill should be used when writing, reviewing, or refactoring Go code. Triggers on tasks involving Go packages, error handling, concurrency, interfaces, or performance optimization.
allowed-tools: Read, Glob, Grep, Bash
---

# Go Best Practices

Style and best practices guide for Go applications. Contains 60+ rules across 10 categories, synthesized from authoritative sources.

**Core skill for:** [[Dev]] (implementing Go code) and [[Architect]] (designing Go architectures).

## Linting (MANDATORY)

**Always run golangci-lint before completing any Go work:**

```bash
# Run linter on all packages
golangci-lint run ./...

# Run with auto-fix for safe fixes
golangci-lint run --fix ./...
```

The project uses `.golangci.yml` which enables:
- **Default linters**: errcheck, gosimple, govet, ineffassign, staticcheck, unused
- **Bug detection**: bodyclose, nilerr, rowserrcheck, sqlclosecheck, noctx
- **Security**: gosec
- **Style**: gofumpt, revive, misspell, whitespace
- **Performance**: prealloc

**For reviews:** Run linter first. Any issues found should be tracked as tasks using `djinn_task_create()`.

## Core Principles

1. **Clarity over cleverness** - Code is read more than written
2. **Simplicity** - The best code is no code; the second best is simple code
3. **Consistency** - Follow established patterns in the codebase
4. **Explicit over implicit** - Make dependencies and behavior obvious
5. **Safety first** - Errors are caught at compile time or fail loudly at runtime
6. **Fail early, fail loud** - Return errors, never panic at runtime

## When to Apply

Reference these guidelines when:
- Writing new Go packages or functions
- Implementing error handling
- Working with concurrency (goroutines, channels)
- Designing interfaces
- Writing tests
- Reviewing Go code for style issues
- Refactoring existing Go code

## Auto-Detection

This skill auto-activates when working with:
- Files matching `*.go`, `*_test.go`
- Imports from standard library or common Go packages
- Patterns like `func`, `type`, `interface`, `go func`, `chan`, `select`
- Error handling patterns like `if err != nil`

## Rule Categories by Priority

| Priority | Category | Impact | Prefix |
|----------|----------|--------|--------|
| 1 | Error Handling | CRITICAL | `err-` |
| 2 | Domain Modeling | CRITICAL | `domain-` |
| 3 | Naming & Style | HIGH | `name-` |
| 4 | Interfaces | HIGH | `iface-` |
| 5 | Concurrency | HIGH | `conc-` |
| 6 | Nil Safety | HIGH | `nil-` |
| 7 | Database | HIGH | `db-` |
| 8 | Package Design | MEDIUM-HIGH | `pkg-` |
| 9 | Control Flow | MEDIUM-HIGH | `flow-` |
| 10 | Functions & Methods | MEDIUM | `func-` |
| 11 | Testing | MEDIUM | `test-` |
| 12 | Performance | MEDIUM | `perf-` |
| 13 | Documentation | LOW-MEDIUM | `doc-` |
| 14 | Project Structure | LOW | `struct-` |

## Quick Reference

### 1. Error Handling (CRITICAL)

- `err-handle` - Always handle errors, never use `_`
- `err-wrap` - Wrap errors with context using `fmt.Errorf("...: %w", err)`
- `err-types` - Use sentinel errors or custom types for expected errors
- `err-strings` - Error strings: lowercase, no punctuation
- `err-flow` - Indent error handling, keep happy path unindented
- `err-panic` - **Never panic for runtime conditions**; return errors instead
- `err-inband` - Avoid in-band errors; use multiple returns

### 2. Domain Modeling (CRITICAL)

- `domain-types` - Use distinct types for domain concepts, wrap primitives
- `domain-validation` - Validate at right layer: boundary, domain, service
- `domain-constructors` - Use constructors for types with invariants
- `domain-state` - Enforce state transitions through methods

### 3. Naming & Style (HIGH)

- `name-mixedcaps` - Use MixedCaps, not underscores
- `name-initialisms` - Keep initialisms uppercase: `URL`, `HTTP`, `ID`
- `name-short` - Short names for short scopes (`i`, `r`, `ctx`)
- `name-receivers` - Receiver names: 1-2 letters, consistent across methods
- `name-getters` - No `Get` prefix for getters (`Name()` not `GetName()`)
- `name-packages` - Package names: short, lowercase, singular
- `name-avoid` - Avoid util, common, misc, base package names

### 4. Interfaces (HIGH)

- `iface-consumer` - Define interfaces where used, not where implemented
- `iface-small` - Prefer small interfaces (1-3 methods)
- `iface-accept` - Accept interfaces, return concrete types
- `iface-verify` - Verify interface compliance at compile time
- `iface-no-mock` - Don't define interfaces just for mocking

### 5. Concurrency (HIGH)

- `conc-goroutine-lifetime` - Document when/whether goroutines exit
- `conc-channel-ownership` - Establish clear channel ownership
- `conc-prefer-sync` - Prefer synchronous functions over async
- `conc-mutex-embed` - Embed mutex, don't expose it
- `conc-context-first` - Context as first parameter
- `conc-no-context-struct` - Don't store context in structs

### 6. Nil Safety (HIGH)

- `nil-pointer-discipline` - Use pointers only when needed, check at reception

### 7. Database (HIGH)

- `db-raw-sql` - Prefer raw SQL over ORM magic
- `db-transactions` - Make transaction boundaries explicit

### 8. Package Design (MEDIUM-HIGH)

- `pkg-single-purpose` - One package, one purpose
- `pkg-internal` - Use internal/ for private packages
- `pkg-avoid-init` - Avoid init() when possible
- `pkg-import-blank` - Side-effect imports only in main
- `pkg-circular` - Avoid circular dependencies

### 9. Control Flow (MEDIUM-HIGH)

- `flow-bounded-loops` - All loops must have explicit termination
- `flow-exhaustive-switch` - Handle all cases + default with error

### 10. Functions & Methods (MEDIUM)

- `func-receiver-type` - Use pointer receiver for mutations, large structs
- `func-named-returns` - Use named returns only for documentation
- `func-naked-returns` - Avoid naked returns except in tiny functions
- `func-options` - Use functional options for complex configuration
- `func-pass-values` - Don't pass pointers to small immutable types

### 11. Testing (MEDIUM)

- `test-table-driven` - Use table-driven tests
- `test-useful-failures` - Test failures should say: got X, want Y
- `test-layers` - Testing strategy by layer (unit, integration, e2e)
- `test-e2e` - E2E tests for critical behaviors (idempotency, races)
- `test-examples` - Write testable Example functions
- `test-parallel` - Use t.Parallel() for independent tests
- `test-helpers` - Use t.Helper() in test helper functions
- `test-subtests` - Use t.Run() for subtests

### 12. Performance (MEDIUM)

- `perf-preallocate` - Preallocate slices when size is known
- `perf-strings-builder` - Use strings.Builder for concatenation
- `perf-sync-pool` - Use sync.Pool for frequently allocated objects
- `perf-avoid-reflect` - Avoid reflection in hot paths
- `perf-benchmark` - Benchmark before optimizing

### 13. Documentation (LOW-MEDIUM)

- `doc-package` - Every package needs a package comment
- `doc-exported` - Document all exported names
- `doc-sentences` - Comments are full sentences, start with name
- `doc-examples` - Include runnable examples in docs

### 14. Project Structure (LOW)

- `struct-cmd` - Main packages in cmd/
- `struct-internal` - Private code in internal/
- `struct-file-size` - Keep files under 400 lines when practical
- `struct-pkg-flat` - Prefer flat package structure over deep nesting

## How to Use

**Quick lookup:** Reference the rule name from the Quick Reference above, then read the detailed rule:

```
rules/{rule-name}.md
```

Example: For `err-wrap`, read [rules/err-wrap.md](./rules/err-wrap.md)

Each rule file contains:
- Brief explanation of why it matters
- Incorrect code example with explanation
- Correct code example with explanation
- References to official documentation

**Full document:** For the complete guide with all rules expanded: [AGENTS.md](./AGENTS.md)

## Integration with Orchestrators

### Implementation (auto-dev)
When implementing Go tasks:
1. Check applicable rules before writing code
2. Apply CRITICAL rules (err-*, domain-*) by default
3. Use domain-* rules for business logic and validation
4. Reference specific rules when reviewing implementations

### Architect (Archie)
When designing Go architectures:
1. Use pkg-* and iface-* rules for package design
2. Use domain-* rules for modeling business concepts
3. Reference struct-* rules for project layout
4. Include rule citations in ADRs

### Testing Strategy
When writing tests:
1. Use test-layers to determine test type
2. Use test-e2e for critical financial/transactional operations
3. Mock at service layer, use real DB at repository layer

## Key Sources

- [Effective Go](https://go.dev/doc/effective_go) - Official Go style guide
- [Go Code Review Comments](https://go.dev/wiki/CodeReviewComments) - Community code review checklist
- [Google Go Style Guide](https://google.github.io/styleguide/go/) - Google's internal style guide
- [Uber Go Style Guide](https://github.com/uber-go/guide) - Uber's Go conventions
- [Standard Go Project Layout](https://github.com/golang-standards/project-layout) - Community project structure
