//! Authorship-time file-size nudge.
//!
//! ## Why this exists, and why it is not a gate
//!
//! `scripts/check-file-size.sh` gated changed Rust files on 1500 lines /
//! 51200 bytes from 2026-06-11. Measured seven weeks later: 108 files had
//! acquired the `// djinn:allow-oversize` escape marker, the oversized-file
//! count had gone 22 -> 79, and **zero** files had been split. Every file that
//! ever tripped the gate was marked; none was restructured.
//!
//! The reason is an incentive gap, not a discipline problem. Complying means
//! restructuring a module; evading means adding one comment line. At that cost
//! asymmetry evasion always wins, and a merge-time gate is exactly where the
//! asymmetry bites hardest — the author has already finished, and the cheapest
//! path back to green is the marker.
//!
//! So the pressure moved to the moment of authorship. A nudge attached to a
//! *successful* edit cannot be gamed, because there is nothing to satisfy: no
//! exit code changes, no marker helps, and ignoring it costs nothing. What it
//! can do is put a fact in front of the author at the only moment they are
//! already holding the file's structure in mind.
//!
//! ## Advisory, and structurally so
//!
//! This module has no error path. Its entry point takes a response value and
//! returns a response value; every failure mode (unreadable file, missing
//! metadata, path outside the worktree) returns the response untouched. It is
//! called *after* the mutation has been written and can never influence it.
//!
//! It deliberately does NOT live in `gate_guard`. PR #2821 and #2839 fixed a
//! read-coverage deadlock in that deny path, which workers had been escaping by
//! routing edits through the ungated `shell` tool. A size message that read as
//! a denial would push them straight back off the instrumented path — the exact
//! failure the coverage fix just undid. So it rides the success payload
//! alongside `related_files` and `jit_pitfalls`, under its own `size_nudge`
//! key, on a result that already says `"ok": true`.
//!
//! ## Choosing the threshold
//!
//! Not 1500 lines and not 51200 bytes — those numbers came from the retired
//! gate and were never derived from anything. The nudge fires on exactly one
//! condition, and it is a fact about the tool surface rather than a taste
//! judgement: **a single `read` can no longer return the whole file.**
//!
//! Two clamps produce that, both already load-bearing in `workspace.rs`:
//!
//! * `read` returns at most [`super::workspace::READ_MAX_LINES`] (2000) lines
//!   — `p.limit.unwrap_or(2000).min(2000)`.
//! * The rendered result is clamped to
//!   [`crate::output_stash::MAX_TOOL_RESULT_CHARS`] (30 000) characters, of
//!   which `read` budgets [`super::workspace::read_content_budget`] for the
//!   numbered listing itself.
//!
//! Both are read from the production constants, not copied, so the nudge
//! tracks them if they ever move.
//!
//! The resulting message is specific in the way that matters: not "this file is
//! large" (noise — large compared to what?) but "seeing this file in full costs
//! N `read` calls", which is a number the reader is about to pay.
//!
//! ## Scope and frequency
//!
//! Language-agnostic: the read budget does not care what the file is, so
//! neither does this. Generated paths and lockfiles are skipped — nobody splits
//! a `Cargo.lock`, and a nudge nobody can act on is just noise.
//!
//! Fires at most once per (session, path), tracked process-wide by the same
//! worktree-path session key `FileTime` and `jit_pitfalls` use. A worker
//! editing one big file thirty times gets one nudge, not thirty.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use super::workspace::{READ_MAX_LINES, read_content_budget};

/// Characters a numbered listing spends per line *beyond* the line's own
/// bytes: six columns of line number, a tab, and a newline.
///
/// The tab and the newline cost two characters each rather than one because
/// `output_stash` clamps the *serialized* result, and JSON escapes them as
/// `\t` and `\n`. `numbered_lines_within_budget` charges the same way.
const LISTING_LINE_OVERHEAD: usize = 6 + 2 + 2;

/// Response key carrying the nudge. Distinct from `jit_pitfalls` so a consumer
/// can tell an advisory about *this file's shape* from an advisory about *what
/// is known to go wrong here*.
const RESPONSE_KEY: &str = "size_nudge";

/// Sessions × paths already nudged. Same shape and same session key as the
/// `jit_pitfalls` first-modification set.
static NUDGED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn nudged_set() -> &'static Mutex<HashSet<String>> {
    NUDGED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Record the (session, path) pair and report whether this is the first time.
fn claim_first_nudge(session_id: &str, path: &Path) -> bool {
    let key = format!("{session_id}\u{0}{}", path.display());
    match nudged_set().lock() {
        Ok(mut set) => set.insert(key),
        // A poisoned mutex must not cost the caller their tool result. Fail
        // toward silence: an extra-quiet advisory beats a panic on a write.
        Err(_) => false,
    }
}

/// Number of `read` calls needed to see a file of `bytes`/`lines` in full.
///
/// Both clamps bind independently, so the answer is whichever is worse. The
/// byte estimate assumes no JSON-escaped characters beyond the per-line tab and
/// newline, which makes it a floor: a file full of quotes and backslashes costs
/// more reads than this returns, never fewer.
pub(crate) fn reads_required(bytes: usize, lines: usize) -> usize {
    let by_lines = lines.div_ceil(READ_MAX_LINES.max(1));
    let budget = read_content_budget().max(1);
    let listing_chars = bytes.saturating_add(lines.saturating_mul(LISTING_LINE_OVERHEAD));
    let by_chars = listing_chars.div_ceil(budget);
    by_lines.max(by_chars).max(1)
}

/// Largest byte count that cannot possibly need a second `read`, whatever the
/// line distribution turns out to be.
///
/// A file of `b` bytes has at most `b` lines, so its worst-case listing costs
/// `b * (1 + LISTING_LINE_OVERHEAD)` characters. Below this bound the answer is
/// "one read" without opening the file — which is what keeps the common case
/// (small files, the overwhelming majority of edits) at a single `metadata`
/// call.
fn single_read_certain_bytes() -> usize {
    let by_chars = read_content_budget() / (1 + LISTING_LINE_OVERHEAD);
    by_chars.min(READ_MAX_LINES)
}

/// Paths whose size is not the author's to fix. A nudge that cannot be acted
/// on is noise, and noise is what makes advisories get filtered out wholesale.
fn is_exempt(path: &Path) -> bool {
    let display = path.to_string_lossy();
    if display.contains("/generated/") || display.contains("\\generated\\") {
        return true;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.contains(".gen.") || name.ends_with(".lock") || name.ends_with(".min.js")
}

/// Compose the advisory for a file that no longer fits in one `read`.
///
/// Shaped after the `gate_guard` prompts — a concrete measurement first, then
/// one thing to consider — but deliberately inverted in mood. `gate_guard`
/// returns `Err` and demands four facts before it will proceed. This returns
/// text on a successful result and demands nothing, and it says so twice, in
/// the first sentence and the last.
fn compose(display_path: &str, bytes: usize, lines: usize, reads: usize) -> String {
    format!(
        "The edit succeeded; this is advisory and blocks nothing.\n\
         {display_path} is now {lines} lines / {bytes} bytes. A single `read` \
         returns at most {READ_MAX_LINES} lines and about {budget} characters \
         of listing, so seeing this file in full now costs {reads} `read` \
         calls — neither you nor the next reader can hold it in one.\n\
         If there is a genuine seam here — a boundary that would still make \
         sense if nobody were measuring — this is the cheapest moment you will \
         ever get to split on it, because you already have the structure in \
         mind. If there isn't one, carry on; a file cut in an arbitrary place \
         is worse than a long one.",
        budget = read_content_budget(),
    )
}

/// Measure `path` and return the advisory, or `None` when there is nothing
/// worth saying.
///
/// `None` covers every uninteresting and every degenerate case alike: the file
/// fits in one `read`, the path is exempt, this (session, path) was already
/// nudged, or the file could not be measured at all. There is no `Err`.
async fn nudge_for(session_id: &str, path: &Path, display_path: &str) -> Option<String> {
    if is_exempt(path) {
        return None;
    }

    let bytes = tokio::fs::metadata(path).await.ok()?.len() as usize;
    // Cheap exit for the overwhelming majority of edits: below this bound no
    // line distribution can force a second read, so the file never has to be
    // opened.
    if bytes <= single_read_certain_bytes() {
        return None;
    }

    let content = tokio::fs::read(path).await.ok()?;
    let lines = content.iter().filter(|b| **b == b'\n').count()
        + usize::from(!content.is_empty() && !content.ends_with(b"\n"));

    let reads = reads_required(bytes, lines);
    if reads <= 1 {
        return None;
    }

    // Claimed last, so a file that never qualified does not burn its one
    // nudge; the first time it actually crosses the line, it still fires.
    if !claim_first_nudge(session_id, path) {
        return None;
    }

    Some(compose(display_path, bytes, lines, reads))
}

/// Append the size advisory to a successful `write`/`edit`/`apply_patch`
/// result, for whichever touched file most needs it.
///
/// Multi-file patches nudge about their single worst file rather than one entry
/// per path: three advisories in one result is a wall of text the reader skips,
/// which is the failure mode this is trying to avoid.
///
/// Returns `response` unchanged whenever there is nothing to say. It cannot
/// fail and it cannot deny — by the time it runs, the bytes are already on
/// disk.
pub(crate) async fn maybe_append_size_nudge(
    response: serde_json::Value,
    worktree_path: &Path,
    touched: &[std::path::PathBuf],
) -> serde_json::Value {
    let session_id = worktree_path.display().to_string();

    let mut best: Option<(usize, String)> = None;
    for path in touched {
        let display_path = path
            .strip_prefix(worktree_path)
            .unwrap_or(path.as_path())
            .display()
            .to_string();
        let Some(text) = nudge_for(&session_id, path, &display_path).await else {
            continue;
        };
        // Rank by rendered length only to break ties deterministically; the
        // meaningful ordering is done inside `nudge_for`, which already refuses
        // anything that fits in one read.
        let weight = text.len();
        if best.as_ref().is_none_or(|(w, _)| weight > *w) {
            best = Some((weight, text));
        }
    }

    let Some((_, text)) = best else {
        return response;
    };

    let mut response = response;
    if let Some(obj) = response.as_object_mut() {
        obj.insert(RESPONSE_KEY.to_string(), serde_json::Value::String(text));
    }
    response
}

#[cfg(test)]
mod tests;
