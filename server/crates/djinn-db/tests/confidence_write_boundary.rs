//! AC1 (proposal 9xih): the epistemic confidence write boundary, enforced as a
//! whole-workspace call-site allowlist.
//!
//! `notes.confidence` is an epistemic posterior AND an input to injection
//! eligibility, injection ordering, and archival lifecycle. Writing a
//! non-epistemic quantity into it does not merely reweight a ranking — it
//! changes what the system may inject and what it archives.
//!
//! Retrieval inclusion, retrieval frequency, reporting, cohorts, rollout
//! labels, and task/review/merge outcomes are therefore forbidden from calling
//! `NoteRepository::set_confidence` or `NoteRepository::update_confidence`.
//!
//! This test scans every production Rust source file under `server/` and fails
//! if a confidence writer appears anywhere outside the allowlist below. It is
//! the "cannot call" half of AC1; the behavioural half lives next to each
//! removed writer:
//!
//! * `djinn-agent/src/task_confidence.rs` —
//!   `task_completion_records_activity_without_touching_note_confidence`
//! * `djinn-coordinator/src/tests/status_and_stuck.rs` —
//!   `failed_closed_task_records_marker_without_penalising_note_confidence`
//! * `djinn-control-plane/src/tools/memory_tools/ops_tests.rs` —
//!   `memory_read_with_a_related_high_confidence_note_changes_no_confidence`

use std::path::{Path, PathBuf};

/// Call syntax rather than bare identifiers, so documentation that explains why
/// a writer was removed does not trip the guard.
const WRITER_CALL_SYNTAX: [&str; 2] = ["update_confidence(", "set_confidence("];

/// The only production files permitted to contain a confidence writer, each
/// with the epistemic justification that earns it the exemption.
///
/// Adding a file here is a deliberate act: it asserts that the new call site
/// concerns a note's TRUTH, not its usefulness, its retrieval frequency, or the
/// outcome of a task that referenced it.
const ALLOWED_WRITER_FILES: [(&str, &str); 5] = [
    (
        "crates/djinn-db/src/repositories/note/scoring.rs",
        "defines set_confidence / update_confidence themselves",
    ),
    (
        "crates/djinn-control-plane/src/tools/memory_tools/confirm.rs",
        "memory_confirm applies USER_CONFIRM — a human asserting the note is correct",
    ),
    (
        "crates/djinn-control-plane/src/tools/memory_tools/contradiction.rs",
        "applies CONTRADICTION / STALE_CITATION — direct evidence the note is wrong or stale",
    ),
    (
        "crates/djinn-db/src/repositories/note/lifecycle.rs",
        "stale-citation decay — the note's cited basis no longer holds",
    ),
    (
        "crates/djinn-slot/src/llm_extraction.rs",
        "sets the INITIAL confidence of a session-extracted note at creation time; \
         a starting prior, not an outcome-derived update",
    ),
];

fn server_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is server/crates/djinn-db.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// Test code is excluded: fixtures legitimately force a confidence value to set
/// up a scenario, and pinning those would make the guard noise.
fn is_test_path(relative: &str) -> bool {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    relative.split('/').any(|segment| segment == "tests")
        || name.ends_with("_tests.rs")
        || name == "tests.rs"
}

fn collect_rust_files(root: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if matches!(name.as_str(), "target" | "node_modules" | ".git") {
                continue;
            }
            collect_rust_files(&path, into);
        } else if name.ends_with(".rs") {
            into.push(path);
        }
    }
}

#[test]
fn only_epistemic_call_sites_may_write_note_confidence() {
    let root = server_root();
    let mut files = Vec::new();
    collect_rust_files(&root.join("crates"), &mut files);
    collect_rust_files(&root.join("src"), &mut files);
    assert!(
        files.len() > 100,
        "the scan found only {} files; it is not actually walking the workspace",
        files.len()
    );

    let mut offenders: Vec<String> = Vec::new();
    let mut seen_allowed: Vec<&str> = Vec::new();

    for path in &files {
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if is_test_path(&relative) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        // Only the production half of the file: an in-file `mod tests` may set
        // confidence to build a fixture.
        let production = text.split("#[cfg(test)]").next().unwrap_or("");
        if !WRITER_CALL_SYNTAX
            .iter()
            .any(|needle| production.contains(needle))
        {
            continue;
        }

        match ALLOWED_WRITER_FILES
            .iter()
            .find(|(allowed, _)| *allowed == relative)
        {
            Some((allowed, _)) => seen_allowed.push(allowed),
            None => offenders.push(relative),
        }
    }

    assert!(
        offenders.is_empty(),
        "these production files write notes.confidence but are not on the epistemic \
         allowlist in this test:\n  {}\n\n\
         Retrieval inclusion/frequency, reporting, cohorts, rollout labels, and \
         task/review/merge outcomes may NOT write confidence (proposal 9xih AC1). \
         If the new call site really is epistemic evidence about the note's truth, \
         add it to ALLOWED_WRITER_FILES with the reason.",
        offenders.join("\n  ")
    );

    // Negative control: if the scan silently stopped matching (a rename, a
    // changed call shape), `offenders` would also be empty and this test would
    // pass while proving nothing. Require the known-good sites to still be
    // found.
    for (allowed, reason) in ALLOWED_WRITER_FILES {
        assert!(
            seen_allowed.contains(&allowed),
            "expected `{allowed}` to still contain a confidence writer ({reason}); \
             it does not, so this guard is no longer detecting anything and must be updated"
        );
    }
}

/// The two removed writers, pinned by name at their former homes.
///
/// The allowlist test above proves no file *outside* the allowlist writes
/// confidence. This proves the specific production paths 9xih named are the
/// ones that stopped.
#[test]
fn removed_outcome_writers_are_absent_from_their_former_call_sites() {
    let root = server_root();
    let cases: [(&str, &str); 3] = [
        (
            "crates/djinn-agent/src/task_confidence.rs",
            "task completion (TASK_SUCCESS_SIGNAL)",
        ),
        (
            "crates/djinn-coordinator/src/actor.rs",
            "task failure after injection (TASK_OUTCOME_CONFIDENCE_SIGNAL)",
        ),
        (
            "crates/djinn-control-plane/src/server.rs",
            "co-access flush (CO_ACCESS_HIGH)",
        ),
    ];

    for (relative, what) in cases {
        let path = root.join(relative);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        let production = text.split("#[cfg(test)]").next().unwrap_or("");
        for needle in WRITER_CALL_SYNTAX {
            assert!(
                !production.contains(needle),
                "{relative} must not contain `{needle}`: the {what} confidence writer \
                 was removed by proposal 9xih"
            );
        }
    }
}

/// `CO_ACCESS_HIGH` and `TASK_SUCCESS` were deleted, not merely unused: an
/// available constant is an invitation to wire it back up.
#[test]
fn removed_outcome_signal_constants_no_longer_exist() {
    let scoring = std::fs::read_to_string(
        server_root().join("crates/djinn-db/src/repositories/note/scoring.rs"),
    )
    .expect("read scoring.rs");
    for constant in ["CO_ACCESS_HIGH", "TASK_SUCCESS"] {
        assert!(
            !scoring.contains(&format!("const {constant}")),
            "`{constant}` must stay deleted (proposal 9xih)"
        );
    }
    // Negative control: an epistemic constant that must still be defined, so a
    // typo'd `const` prefix cannot make the assertions above vacuous.
    assert!(
        scoring.contains("pub const USER_CONFIRM"),
        "USER_CONFIRM must still be defined; the assertions above match on `const <NAME>`"
    );
}
