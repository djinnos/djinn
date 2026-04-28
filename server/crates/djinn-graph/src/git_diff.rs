//! Shell-out wrapper around `git diff --unified=0 base..head` plus a parser
//! for the resulting unified-diff hunk headers.
//!
//! Used by the `code_graph detect_changes` op (PR C4) to map a SHA range
//! to a list of `(file, start_line, end_line)` hunks, which the bridge
//! then resolves to symbols via `RepoDependencyGraph::symbols_enclosing`.
//!
//! Why shell-out instead of `git2`: this crate already shells out for the
//! coupling-index pipeline (see [`crate::coupling_index`]) and adding a
//! `git2` dep would balloon the worker binary. Match the same error /
//! timeout / process style.

use std::path::Path;
use std::time::Duration;

use thiserror::Error;
use tokio::process::Command;

const GIT_DIFF_TIMEOUT: Duration = Duration::from_secs(120);

/// A single `(file, start_line, end_line)` hunk parsed from a unified
/// diff. Mirrors the layout of `djinn_control_plane::bridge::ChangedRange`
/// — kept local here to avoid a reverse dependency from `djinn-graph` on
/// `djinn-control-plane`. The MCP bridge converts at the boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedRange {
    /// Repository-relative path of the file the hunk lives in (post-image).
    pub file: String,
    /// Inclusive 1-indexed first line of the hunk.
    pub start_line: i64,
    /// Inclusive 1-indexed last line of the hunk. Defaults to `start_line`
    /// when the caller passed a single-line hunk.
    pub end_line: Option<i64>,
}

#[derive(Debug, Error)]
pub enum GitDiffError {
    #[error("spawn git diff: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("git diff timed out after {0:?}")]
    Timeout(Duration),
    #[error("git diff exited with status {status}: {stderr}")]
    GitFailed { status: String, stderr: String },
    #[error("parse git diff output: {0}")]
    Parse(String),
}

/// Run `git diff --unified=0 from..to` against `repo_root` and return the
/// list of changed line ranges, keyed by post-image file path. One
/// [`ChangedRange`] per hunk.
///
/// * `start_line` is the 1-indexed first line of the post-image hunk.
/// * `end_line` is `start_line + count - 1` for `count > 0`. When the
///   diff header reports `+0,0` (pure deletion — the new image has no
///   lines for the hunk) we still emit a [`ChangedRange`] anchored at
///   the deletion site (the `+` line number, even when count is `0`)
///   so callers can still surface the deletion to reviewers.
/// * For pure deletions (`+a,0`), `end_line` is `Some(start_line)` —
///   the caller should treat that as a single-line marker, not a
///   genuine hunk.
///
/// Files-only mode: pass an empty range list and use a separate
/// `--name-only` invocation if you only need the touched-file set.
/// This helper always emits hunk-level data.
pub async fn diff_changed_ranges(
    repo_root: &Path,
    from: &str,
    to: &str,
) -> Result<Vec<ChangedRange>, GitDiffError> {
    let raw = run_git_diff(repo_root, from, to).await?;
    parse_unified_diff(&raw)
}

async fn run_git_diff(
    repo_root: &Path,
    from: &str,
    to: &str,
) -> Result<String, GitDiffError> {
    let range = format!("{from}..{to}");
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_root);
    cmd.arg("diff");
    cmd.arg("--unified=0");
    // `--no-color`: defensive — colour codes blow up the regex parser.
    cmd.arg("--no-color");
    // `--no-ext-diff`: side-step user-configured external diff tools.
    cmd.arg("--no-ext-diff");
    cmd.arg(&range);

    let output = tokio::time::timeout(GIT_DIFF_TIMEOUT, cmd.output())
        .await
        .map_err(|_| GitDiffError::Timeout(GIT_DIFF_TIMEOUT))??;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(GitDiffError::GitFailed {
            status: format!("{}", output.status),
            stderr,
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse a `git diff --unified=0` blob into per-hunk [`ChangedRange`]s.
///
/// Recognised lines:
/// * `diff --git a/<path> b/<path>` — start of a per-file diff. The
///   `b/<path>` post-image path becomes the [`ChangedRange::file`]
///   for every subsequent hunk until the next `diff --git` header.
/// * `@@ -<a>,<b> +<c>,<d> @@ ...` — hunk header. The `+c,d` block
///   becomes one [`ChangedRange`]:
///     * single-line form `+c` is treated as `+c,1`.
///     * `+0,0` (pure deletion) anchors at `c=0` and is skipped.
///
/// All other lines (`+++ b/`, `--- a/`, body lines) are ignored — at
/// `--unified=0` we never need to walk hunk bodies.
pub fn parse_unified_diff(raw: &str) -> Result<Vec<ChangedRange>, GitDiffError> {
    let mut out: Vec<ChangedRange> = Vec::new();
    let mut current_file: Option<String> = None;

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // `diff --git a/<old> b/<new>`. Pull the b/-side path; for
            // renames git emits the rename header in subsequent
            // `rename to <new>` lines, but the b-path is always the
            // post-image so anchoring on it is sufficient.
            current_file = parse_diff_git_header(rest);
            continue;
        }
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            // File path markers — already captured from the
            // `diff --git` header. Skip.
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            let Some(file) = current_file.as_deref() else {
                return Err(GitDiffError::Parse(format!(
                    "hunk header without preceding `diff --git`: {line}"
                )));
            };
            if let Some(range) = parse_hunk_header(rest, file)? {
                out.push(range);
            }
            continue;
        }
        // Body lines, similarity index, mode lines — ignored.
    }
    Ok(out)
}

/// Extract the post-image (`b/`) path from a `diff --git a/x b/y` header
/// suffix. Returns `None` when the format is unrecognised so the parser
/// surfaces a clear error on the next hunk.
fn parse_diff_git_header(rest: &str) -> Option<String> {
    // The suffix is `a/<old> b/<new>`. Quoted paths are wrapped in `"..."`
    // when they contain whitespace — git's quoting rules are conservative
    // (only special chars), so for our purposes splitting on whitespace
    // and finding the `b/` token is sufficient.
    let mut parts = rest.split_whitespace();
    let _a = parts.next()?;
    let b = parts.next()?;
    let path = b.strip_prefix("b/")?;
    // Strip surrounding quotes if git emitted a quoted path.
    let path = path.trim_matches('"');
    Some(path.to_string())
}

/// Parse the post-image side of a hunk header — the `+c,d` block that
/// follows the leading `-a,b`. Returns `None` for `+0,0` (pure delete
/// with no anchor on the new image).
fn parse_hunk_header(
    rest: &str,
    file: &str,
) -> Result<Option<ChangedRange>, GitDiffError> {
    // `rest` is everything after the leading `@@ ` — e.g.
    // `-12,3 +30,5 @@ fn foo()`. We only need the `+...` token.
    let plus_token = rest
        .split_whitespace()
        .find(|tok| tok.starts_with('+'))
        .ok_or_else(|| {
            GitDiffError::Parse(format!("hunk header missing `+` token: @@ {rest}"))
        })?;
    let inside = plus_token
        .strip_prefix('+')
        .expect("checked starts_with above");
    let (start_str, count_str) = match inside.split_once(',') {
        Some((s, c)) => (s, c),
        None => (inside, "1"),
    };
    let start: i64 = start_str.parse().map_err(|e| {
        GitDiffError::Parse(format!(
            "hunk header start not numeric ({file}, +{start_str}): {e}"
        ))
    })?;
    let count: i64 = count_str.parse().map_err(|e| {
        GitDiffError::Parse(format!(
            "hunk header count not numeric ({file}, +{start_str},{count_str}): {e}"
        ))
    })?;
    // Pure deletion (`+0,0`) carries no post-image anchor — drop it.
    if start == 0 && count == 0 {
        return Ok(None);
    }
    let end = if count == 0 {
        // git emits `+a,0` for a deletion *anchored* between two lines
        // — the new image still has line `a` (and the hunk represents
        // a deletion just below). Surface it as a single-line marker
        // so reviewers see the file is touched without misreporting
        // the line range.
        start
    } else {
        start + count - 1
    };
    Ok(Some(ChangedRange {
        file: file.to_string(),
        start_line: start,
        end_line: Some(end),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_hunk_addition() {
        let raw = "\
diff --git a/src/lib.rs b/src/lib.rs
index 0000001..0000002 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -42,0 +43,3 @@ fn pre_existing()
+pub fn added() {}
+pub fn also_added() {}
+pub fn third() {}
";
        let ranges = parse_unified_diff(raw).expect("parse");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].file, "src/lib.rs");
        assert_eq!(ranges[0].start_line, 43);
        assert_eq!(ranges[0].end_line, Some(45));
    }

    #[test]
    fn parses_multiple_hunks_in_same_file() {
        let raw = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,1 +10,1 @@ fn a()
-old line
+new line
@@ -50,0 +51,2 @@
+inserted_a
+inserted_b
";
        let ranges = parse_unified_diff(raw).expect("parse");
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].start_line, 10);
        assert_eq!(ranges[0].end_line, Some(10));
        assert_eq!(ranges[1].start_line, 51);
        assert_eq!(ranges[1].end_line, Some(52));
    }

    #[test]
    fn parses_hunks_across_multiple_files() {
        let raw = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,1 +1,1 @@
-x
+y
diff --git a/src/b.rs b/src/b.rs
--- a/src/b.rs
+++ b/src/b.rs
@@ -100,2 +100,3 @@
-old1
-old2
+new1
+new2
+new3
";
        let ranges = parse_unified_diff(raw).expect("parse");
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].file, "src/a.rs");
        assert_eq!(ranges[1].file, "src/b.rs");
        assert_eq!(ranges[1].start_line, 100);
        assert_eq!(ranges[1].end_line, Some(102));
    }

    #[test]
    fn pure_deletion_with_zero_count_anchors_at_start() {
        let raw = "\
diff --git a/src/x.rs b/src/x.rs
--- a/src/x.rs
+++ b/src/x.rs
@@ -10,3 +9,0 @@ fn foo()
-deleted_a
-deleted_b
-deleted_c
";
        let ranges = parse_unified_diff(raw).expect("parse");
        // Pure delete anchored at `+9,0` becomes a single-line marker
        // at line 9 of the new image.
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].file, "src/x.rs");
        assert_eq!(ranges[0].start_line, 9);
        assert_eq!(ranges[0].end_line, Some(9));
    }

    #[test]
    fn pure_zero_zero_hunk_is_dropped() {
        // Synthetic — `+0,0` should never reach a real reviewer (it
        // means "no anchor on either side") but defensively we drop it.
        let raw = "\
diff --git a/src/y.rs b/src/y.rs
--- a/src/y.rs
+++ b/src/y.rs
@@ -1,1 +0,0 @@
-only line
";
        let ranges = parse_unified_diff(raw).expect("parse");
        assert!(ranges.is_empty());
    }

    #[test]
    fn empty_diff_yields_no_ranges() {
        let ranges = parse_unified_diff("").expect("parse");
        assert!(ranges.is_empty());
    }

    #[test]
    fn hunk_header_without_diff_git_errs() {
        let raw = "@@ -1,1 +1,1 @@\n-x\n+y\n";
        let err = parse_unified_diff(raw).expect_err("should err");
        match err {
            GitDiffError::Parse(_) => {}
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    /// End-to-end against a real `git` binary in a tempdir. Matches the
    /// `coupling_index::end_to_end_ingest_and_query` style.
    #[tokio::test]
    async fn diff_changed_ranges_against_real_repo() {
        let tmp = tempfile::Builder::new()
            .prefix("djinn-git-diff-e2e-")
            .tempdir_in(".")
            .expect("tempdir");
        let root = tmp.path().to_path_buf();

        async fn run(root: &std::path::Path, args: &[&str]) {
            let out = Command::new("git")
                .current_dir(root)
                .args(args)
                .output()
                .await
                .expect("git");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        run(&root, &["init", "-q", "-b", "main"]).await;
        run(&root, &["config", "user.email", "t@t"]).await;
        run(&root, &["config", "user.name", "t"]).await;
        tokio::fs::write(root.join("a.rs"), "fn one() {}\nfn two() {}\nfn three() {}\n")
            .await
            .expect("write");
        run(&root, &["add", "."]).await;
        run(&root, &["commit", "-q", "-m", "seed"]).await;

        // Capture base sha.
        let base_out = Command::new("git")
            .current_dir(&root)
            .args(["rev-parse", "HEAD"])
            .output()
            .await
            .expect("rev-parse");
        let base_sha = String::from_utf8_lossy(&base_out.stdout).trim().to_string();

        // Modify line 2 → produce a hunk at line 2.
        tokio::fs::write(
            root.join("a.rs"),
            "fn one() {}\nfn two_renamed() {}\nfn three() {}\n",
        )
        .await
        .expect("write");
        run(&root, &["add", "a.rs"]).await;
        run(&root, &["commit", "-q", "-m", "rename two"]).await;

        let head_out = Command::new("git")
            .current_dir(&root)
            .args(["rev-parse", "HEAD"])
            .output()
            .await
            .expect("rev-parse");
        let head_sha = String::from_utf8_lossy(&head_out.stdout).trim().to_string();

        let ranges = diff_changed_ranges(&root, &base_sha, &head_sha)
            .await
            .expect("diff");
        assert_eq!(ranges.len(), 1, "expected one hunk, got {:?}", ranges);
        assert_eq!(ranges[0].file, "a.rs");
        assert_eq!(ranges[0].start_line, 2);
        assert_eq!(ranges[0].end_line, Some(2));
    }
}
