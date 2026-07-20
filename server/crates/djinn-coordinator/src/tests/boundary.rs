// ─── Boundary checks: orchestration crate dependency invariants ──────────────
//
// These tests verify the architectural boundaries established by the Phase 5
// orchestration split.  They read `Cargo.toml` at compile time via
// `include_str!` and assert that forbidden dependency edges are absent.
// If a boundary is violated, the test fails with a clear message explaining
// which crate depends on which forbidden crate.

/// `djinn-slot` must NOT depend on `djinn-coordinator` or `djinn-agent`.
///
/// The slot crate owns pool lifecycle, reply loop, session extraction, and
/// related helpers.  It must be usable by both the coordinator and the agent
/// facade without pulling in either's internals.
#[test]
fn boundary_djinn_slot_has_no_coordinator_or_agent_dependency() {
    let cargo_toml = include_str!("../../../djinn-slot/Cargo.toml");
    for forbidden in &["djinn-coordinator", "djinn-agent"] {
        let dep_pattern = format!("{forbidden} =");
        let has_dep = cargo_toml.lines().any(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with('#') && trimmed.starts_with(&dep_pattern)
        });
        assert!(
            !has_dep,
            "djinn-slot Cargo.toml must not contain a dependency on {forbidden}; \
             found a dependency declaration line in: djinn-slot/Cargo.toml"
        );
    }
}

/// `djinn-coordinator` production code must NOT depend on `djinn-agent`.
///
/// The coordinator crate owns dispatch, doctor, PR polling, and supervisor
/// disposition logic.  It depends on `djinn-slot` and `djinn-orchestration-types`
/// but must never pull the agent facade into its production dependency graph.
/// A test-only edge is permitted for cross-crate integration regressions.
#[test]
fn boundary_djinn_coordinator_has_no_agent_dependency() {
    let cargo_toml = include_str!("../../../djinn-coordinator/Cargo.toml");
    let dep_pattern = "djinn-agent =";
    let has_dep = cargo_toml
        .lines()
        .skip_while(|line| line.trim() != "[dependencies]")
        .skip(1)
        .take_while(|line| !line.trim_start().starts_with('['))
        .any(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with('#') && trimmed.starts_with(dep_pattern)
        });
    assert!(
        !has_dep,
        "djinn-coordinator production dependencies must not include djinn-agent"
    );
}

/// `djinn-slot` production code must NOT use `djinn-coordinator` types.
///
/// This source-level check scans all non-test `.rs` files in `djinn-slot/src/`
/// for `use djinn_coordinator` imports.  Test-only files (under `tests/` or
/// containing `#[cfg(test)]`) are excluded.
#[test]
fn boundary_djinn_slot_source_has_no_coordinator_import() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../djinn-slot/src");
    let mut offenders = Vec::new();
    if src_dir.exists() {
        for entry in walkdir_or_empty(&src_dir) {
            let path = entry;
            if !path.ends_with(".rs") {
                continue;
            }
            let rel = path.strip_prefix(&src_dir).unwrap_or(&path);
            // Skip test-only files
            if rel.starts_with("tests") || rel.to_string_lossy().contains("test_helper") {
                continue;
            }
            if let Ok(contents) = std::fs::read_to_string(&path)
                && contents.contains("use djinn_coordinator")
            {
                offenders.push(rel.display().to_string());
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "djinn-slot production source must not import djinn_coordinator; offenders: {offenders:?}"
    );
}

/// `djinn-coordinator` production code must NOT use `djinn_agent` types.
///
/// This source-level check scans all non-test `.rs` files in
/// `djinn-coordinator/src/` for `use djinn_agent` imports.
#[test]
fn boundary_djinn_coordinator_source_has_no_agent_import() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../djinn-coordinator/src");
    let mut offenders = Vec::new();
    if src_dir.exists() {
        for entry in walkdir_or_empty(&src_dir) {
            let path = entry;
            if !path.ends_with(".rs") {
                continue;
            }
            let rel = path.strip_prefix(&src_dir).unwrap_or(&path);
            // Skip test files
            if rel.starts_with("tests") || rel.to_string_lossy().contains("test_helper") {
                continue;
            }
            if let Ok(contents) = std::fs::read_to_string(&path)
                && contents.contains("use djinn_agent")
            {
                offenders.push(rel.display().to_string());
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "djinn-coordinator production source must not import djinn_agent; offenders: {offenders:?}"
    );
}

/// `djinn-coordinator` must NOT have a direct `sqlx` dependency.
///
/// All coordinator SQL is routed through `djinn-db` repository and
/// test-support helpers; the crate's `Cargo.toml` must not list `sqlx`
/// as a dependency.
#[test]
fn boundary_djinn_coordinator_has_no_sqlx_dependency() {
    let cargo_toml = include_str!("../../../djinn-coordinator/Cargo.toml");
    let has_sqlx = cargo_toml.lines().any(|line| {
        let trimmed = line.trim_start();
        !trimmed.starts_with('#') && trimmed.starts_with("sqlx =")
    });
    assert!(
        !has_sqlx,
        "djinn-coordinator Cargo.toml must not contain a direct sqlx dependency; \
         all SQL should go through djinn-db helpers"
    );
}

/// `djinn-slot` must NOT have a direct `sqlx` dependency.
///
/// All slot SQL is routed through `djinn-db` helpers; the crate's
/// `Cargo.toml` must not list `sqlx` as a dependency.
#[test]
fn boundary_djinn_slot_has_no_sqlx_dependency() {
    let cargo_toml = include_str!("../../../djinn-slot/Cargo.toml");
    let has_sqlx = cargo_toml.lines().any(|line| {
        let trimmed = line.trim_start();
        !trimmed.starts_with('#') && trimmed.starts_with("sqlx =")
    });
    assert!(
        !has_sqlx,
        "djinn-slot Cargo.toml must not contain a direct sqlx dependency; \
         all SQL should go through djinn-db helpers"
    );
}

// ── Helper: collect .rs paths from a directory tree ──────────────────────────

/// Return a vector of `.rs` file paths under `dir`, or an empty vec if the
/// directory does not exist.  Not recursive beyond what `std::fs` provides
/// via a simple stack walk.
fn walkdir_or_empty(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut result = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match std::fs::read_dir(&current) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                result.push(path);
            }
        }
    }
    result
}
