use super::super::fuzzy::{MatchMetadata, MatchOutcome, UnicodeSpliceStatus};

/// Denial message for `cargo check`/`cargo build` by worker/reviewer roles.
/// These cold-build the whole workspace (~12min); clippy reuses the warm cache.
pub(crate) const CARGO_CHECK_DENIED_MSG: &str = "cargo check / cargo build (for type-checking) is disabled for the worker and reviewer roles: it produces different artifacts than the warm cargo cache and cold-builds the workspace. Use `cargo clippy -p <crate>` instead (it reuses the warm cache and also lints). `cargo test`/`cargo nextest`, `cargo tree`, `cargo metadata`, and `cargo fmt` are allowed.";

/// Does this shell command, run by a code-executing role (worker or reviewer),
/// invoke a `cargo check` or `cargo build` whose only purpose is type-checking?
/// Returns the denial message when so. We allow clippy/test/nextest/tree/metadata/fmt
/// and all non-cargo commands.
///
/// Robust to: `bash -lc "cargo check"`, leading paths
/// (`/usr/local/bin/cargo build`), `cargo +nightly check`, and `&&`/`;`/`|`
/// command chains. Conservative: only the exact denied subcommands trip it, so a
/// `cargo clippy` containing the word "check" in a path is unaffected.
pub(crate) fn cargo_check_denied(command: &str) -> Option<&'static str> {
    for segment in command.split(['\n', ';', '&', '|']) {
        if segment_is_denied_cargo(segment) {
            return Some(CARGO_CHECK_DENIED_MSG);
        }
    }
    None
}

/// True when a single shell segment is a `cargo check`/`cargo build` invocation.
fn segment_is_denied_cargo(segment: &str) -> bool {
    let mut tokens = segment.split_whitespace().peekable();

    while let Some(&tok) = tokens.peek() {
        let bare = tok.trim_matches(['"', '\'']);
        let base = bare.rsplit('/').next().unwrap_or(bare);
        if base == "cargo" {
            break;
        }
        if bare.contains('=') && !bare.starts_with('-') {
            tokens.next();
            continue;
        }
        tokens.next();
    }

    match tokens.next() {
        Some(tok) => {
            let bare = tok.trim_matches(['"', '\'']);
            let base = bare.rsplit('/').next().unwrap_or(bare);
            if base != "cargo" {
                return false;
            }
        }
        None => return false,
    }

    if tokens.peek().is_some_and(|next| next.starts_with('+')) {
        tokens.next();
    }

    match tokens.next() {
        Some(sub) => {
            let sub = sub.trim_matches(['"', '\'']);
            sub == "check" || sub == "build"
        }
        None => false,
    }
}

/// Emit bounded-cardinality telemetry for an edit match outcome.
///
/// Emits `edit_match_outcome` for all outcomes, and additionally
/// `edit_match_strategy` for successful matches. Uses structured `tracing`
/// fields; never logs full file paths or file content.
///
/// Telemetry failures are swallowed — this must never make an edit call fail.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_edit_match_telemetry(
    metadata: &MatchMetadata,
    task_id: Option<&str>,
    session_id: Option<&str>,
    agent_role: Option<&str>,
    path_ext: &str,
    old_bytes: usize,
    new_bytes: usize,
    matched_bytes: Option<usize>,
) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let strategy = metadata.strategy.as_str();
        let outcome = match metadata.outcome {
            MatchOutcome::Success => "success",
            MatchOutcome::Ambiguous => "ambiguous",
            MatchOutcome::NoMatch => "no_match",
            MatchOutcome::GuardRejected => "guard_rejected",
        };
        let guard = metadata.guard_rejected_reason.unwrap_or("");
        let score = metadata.nearest_miss;
        let unicode_splice = metadata.unicode_splice.map(|s| match s {
            UnicodeSpliceStatus::Clean => "clean",
            UnicodeSpliceStatus::Adjusted => "adjusted",
        });

        tracing::info!(
            event_name = "edit_match_outcome",
            task_id = task_id.unwrap_or(""),
            session_id = session_id.unwrap_or(""),
            agent_role = agent_role.unwrap_or(""),
            tool_name = "edit",
            path_ext,
            strategy,
            outcome,
            guard,
            candidate_count = metadata.candidate_count,
            score,
            old_bytes,
            new_bytes,
            matched_bytes,
            reindented = metadata.reindented,
            unicode_splice,
            "edit match outcome telemetry"
        );

        if metadata.outcome == MatchOutcome::Success {
            tracing::info!(
                event_name = "edit_match_strategy",
                task_id = task_id.unwrap_or(""),
                session_id = session_id.unwrap_or(""),
                agent_role = agent_role.unwrap_or(""),
                tool_name = "edit",
                path_ext,
                strategy,
                outcome,
                guard,
                candidate_count = metadata.candidate_count,
                score,
                old_bytes,
                new_bytes,
                matched_bytes,
                reindented = metadata.reindented,
                unicode_splice,
                "edit match strategy success telemetry"
            );
        }
    }));
}

#[cfg(test)]
mod cargo_guard_tests {
    use super::cargo_check_denied;

    #[test]
    fn rejects_cargo_check_and_build() {
        assert!(cargo_check_denied("cargo check -p djinn-db").is_some());
        assert!(cargo_check_denied("cargo build -p x").is_some());
        // bash -lc wrapper.
        assert!(cargo_check_denied(r#"bash -lc "cargo check""#).is_some());
        assert!(cargo_check_denied(r#"bash -lc 'cargo check'"#).is_some());
        // leading path + toolchain selector.
        assert!(cargo_check_denied("/usr/local/bin/cargo check").is_some());
        assert!(cargo_check_denied("cargo +nightly check -p x").is_some());
        // env-assignment prefix.
        assert!(cargo_check_denied("FOO=bar cargo check").is_some());
        // chain — the denied segment trips it even after an allowed one.
        assert!(cargo_check_denied("cargo clippy -p x && cargo check -p x").is_some());
        assert!(cargo_check_denied("cd server; cargo build").is_some());
    }

    #[test]
    fn allows_clippy_test_and_inspection() {
        assert!(cargo_check_denied("cargo clippy -p x").is_none());
        assert!(cargo_check_denied("cargo clippy --all-targets -- -D warnings").is_none());
        assert!(cargo_check_denied("cargo nextest run -p x").is_none());
        assert!(cargo_check_denied("cargo test -p x").is_none());
        assert!(cargo_check_denied("cargo tree -p x").is_none());
        assert!(cargo_check_denied("cargo metadata --format-version 1").is_none());
        assert!(cargo_check_denied("cargo fmt").is_none());
        assert!(cargo_check_denied("git diff").is_none());
        assert!(cargo_check_denied("ls -la").is_none());
        // A non-cargo binary that happens to be named with "check" is fine.
        assert!(cargo_check_denied("./scripts/check-file-size.sh").is_none());
        // clippy chained with an allowed cargo command stays allowed.
        assert!(cargo_check_denied("cargo clippy -p x && cargo test -p x").is_none());
    }
}
