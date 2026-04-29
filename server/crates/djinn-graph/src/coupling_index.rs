//! Commit-based file-coupling ingest.
//!
//! Walks `cursor..HEAD` of the canonical clone and batches the per-commit
//! per-file change facts into [`djinn_db::CommitFileChangeRepository`].
//! Aggregates (coupling, churn) live in that repository; this module is
//! strictly the `git log` → rows pipeline.
//!
//! Hooked into [`crate::canonical_graph::ensure_canonical_graph`] so the
//! coupling index stays current across the same cadence as the canonical
//! graph. Failures are non-fatal — a stale coupling table is strictly
//! less bad than a missing canonical graph, so the caller logs and
//! continues.

use std::path::Path;
use std::time::Duration;

use djinn_db::{
    CommitFileChange, CommitFileChangeRepository, CouplingPairEvent, Database,
    derive_pair_events_into,
};
use thiserror::Error;
use tokio::process::Command;

const GIT_LOG_TIMEOUT: Duration = Duration::from_secs(120);
const BATCH_SIZE: usize = 500;
const PAIR_BATCH_SIZE: usize = 500;
const COMMIT_SENTINEL: &str = "__COMMIT__";

/// Observability rollup returned by [`ingest_new_commits`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestStats {
    pub commits_ingested: usize,
    pub rows_inserted: usize,
    /// Pair events written to `coupling_pair_events`. Includes any
    /// first-pass backfill from existing `commit_file_changes` rows.
    pub pair_events_inserted: usize,
    /// True when we invoked `git fetch --unshallow` to try to extend a
    /// shallow clone. Surfaced for tests / dashboards.
    pub unshallowed: bool,
}

#[derive(Debug, Error)]
pub enum IngestError {
    #[error("spawn git log: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("git log timed out after {0:?}")]
    Timeout(Duration),
    #[error("git log exited with status {status}: {stderr}")]
    GitFailed { status: String, stderr: String },
    #[error("parse git log output: {0}")]
    Parse(String),
    #[error("db error: {0}")]
    Db(#[from] djinn_db::Error),
}

/// Walk new commits since the stored cursor and persist them to
/// `commit_file_changes`, then advance the cursor to HEAD.
///
/// * Empty repo (no commits) → returns zero counts with no error.
/// * Shallow clone with missing history → tries one `git fetch
///   --unshallow` and re-runs; if still bounded, ingests what is
///   visible and logs a warning.
/// * Binary files (`-\t-\t<path>` in `--numstat`) → rows with
///   insertions=0/deletions=0.
pub async fn ingest_new_commits(
    db: &Database,
    project_id: &str,
    project_root: &Path,
) -> Result<IngestStats, IngestError> {
    let repo = CommitFileChangeRepository::new(db.clone());

    // Short-circuit when the repo has zero commits. `git rev-parse HEAD`
    // exits non-zero on an empty repo; we treat that as a no-op instead
    // of bubbling the error up — a fresh clone with no commits is a
    // valid state for the warmer pipeline.
    let head = match git_head(project_root).await {
        Ok(sha) => sha,
        Err(_) => {
            return Ok(IngestStats::default());
        }
    };

    let cursor = repo.get_cursor(project_id).await?;
    let range = match cursor.as_deref() {
        Some(sha) if !sha.is_empty() => {
            // Cursor set → ingest `cursor..HEAD`. If the cursor points
            // at a commit git no longer has (e.g. history rewrite), fall
            // back to a full walk so we recover without human
            // intervention.
            if cursor_is_reachable(project_root, sha).await {
                format!("{sha}..HEAD")
            } else {
                tracing::warn!(
                    project_id = %project_id,
                    cursor = %sha,
                    "coupling cursor points at an unreachable commit; re-ingesting full history"
                );
                "HEAD".to_string()
            }
        }
        _ => "HEAD".to_string(),
    };

    let mut stats = IngestStats::default();
    let (output, unshallowed) = run_git_log(project_root, &range).await?;
    stats.unshallowed = unshallowed;

    let mut batch: Vec<CommitFileChange> = Vec::with_capacity(BATCH_SIZE);
    let parsed = parse_git_log(&output, project_id).map_err(IngestError::Parse)?;
    stats.commits_ingested = parsed.commits_seen;

    // Group rows by commit so we can derive pair events alongside the
    // raw row insert. We accumulate per-commit file lists in
    // `commit_files` keyed by commit_sha; the parser emits all rows
    // for one commit contiguously, so a streaming "flush on sha
    // change" would also work — using a small map keeps the code
    // path symmetric with the row batch.
    use std::collections::HashMap;
    let mut commit_meta: HashMap<String, String> = HashMap::new();
    let mut commit_files: HashMap<String, Vec<String>> = HashMap::new();
    let mut pair_buffer: Vec<CouplingPairEvent> = Vec::with_capacity(PAIR_BATCH_SIZE);

    for row in parsed.rows {
        commit_meta
            .entry(row.commit_sha.clone())
            .or_insert_with(|| row.committed_at.clone());
        commit_files
            .entry(row.commit_sha.clone())
            .or_default()
            .push(row.file_path.clone());
        batch.push(row);
        if batch.len() >= BATCH_SIZE {
            stats.rows_inserted += repo.upsert_batch(&batch).await?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        stats.rows_inserted += repo.upsert_batch(&batch).await?;
    }

    // Derive + upsert pair events. Big commits (> MAX_FILES_PER_COMMIT_FOR_PAIRS)
    // contribute zero pairs — the cap that used to live on the read
    // side as a correlated `IN (HAVING COUNT(*) <= ?)` subquery now
    // applies at write time, so the pathological self-join never
    // executes against Dolt's planner.
    for (sha, files) in &commit_files {
        let committed_at = match commit_meta.get(sha) {
            Some(at) => at,
            None => continue,
        };
        derive_pair_events_into(project_id, sha, committed_at, files, &mut pair_buffer);
        if pair_buffer.len() >= PAIR_BATCH_SIZE {
            stats.pair_events_inserted += repo.upsert_pair_events(&pair_buffer).await?;
            pair_buffer.clear();
        }
    }
    if !pair_buffer.is_empty() {
        stats.pair_events_inserted += repo.upsert_pair_events(&pair_buffer).await?;
    }

    // First-time backfill: if the cursor is unset (full-history run)
    // OR the pair-events table is empty for this project (migration
    // 20 just landed), force a rebuild from the existing
    // `commit_file_changes` rows. The ingest above only touches new
    // commits since `cursor`; this catches up the pair table for
    // projects that were ingested before pair materialisation.
    let needs_backfill = match cursor.as_deref() {
        Some(_) => repo.pair_events_count_for_project(project_id).await? == 0,
        None => false, // full-history run already wrote pairs above
    };
    if needs_backfill {
        let backfilled = repo.rebuild_pair_events_for_project(project_id).await?;
        stats.pair_events_inserted += backfilled;
        tracing::info!(
            project_id,
            backfilled,
            "coupling_pair_events: backfilled from existing commit_file_changes"
        );
    }

    // Advance cursor even when we observed zero new commits — we still
    // want the cursor to reflect "ingest ran against this HEAD".
    repo.set_cursor(project_id, &head).await?;
    Ok(stats)
}

async fn git_head(project_root: &Path) -> Result<String, IngestError> {
    let output = tokio::time::timeout(
        GIT_LOG_TIMEOUT,
        Command::new("git")
            .current_dir(project_root)
            .args(["rev-parse", "HEAD"])
            .output(),
    )
    .await
    .map_err(|_| IngestError::Timeout(GIT_LOG_TIMEOUT))??;
    if !output.status.success() {
        return Err(IngestError::GitFailed {
            status: format!("{}", output.status),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

async fn cursor_is_reachable(project_root: &Path, sha: &str) -> bool {
    let Ok(Ok(output)) = tokio::time::timeout(
        GIT_LOG_TIMEOUT,
        Command::new("git")
            .current_dir(project_root)
            .args(["cat-file", "-e", sha])
            .output(),
    )
    .await
    else {
        return false;
    };
    output.status.success()
}

/// Run `git log` and, if the output looks like a shallow clone cut
/// short of the cursor range, extend once via `git fetch --unshallow`
/// and re-run. Returns `(stdout, unshallowed)`.
async fn run_git_log(
    project_root: &Path,
    range: &str,
) -> Result<(String, bool), IngestError> {
    // Unshallow eagerly when the clone is shallow. The warm Job pod
    // does `git clone --depth 1 --single-branch` (see
    // `djinn_k8s::warm_job`) — fast for SCIP, useless for coupling: a
    // depth-1 clone walked by `git log` shows the single visible
    // commit as "everything added," producing thousands of bogus rows
    // all stamped with `change_kind = 'A'`. The previous heuristic
    // only triggered on an empty log (cursor-bounded range that
    // didn't intersect visible history); first-run / full-history
    // walks slipped past it and wrote the bad data anyway.
    let is_shallow = tokio::fs::metadata(project_root.join(".git/shallow"))
        .await
        .is_ok();
    let mut unshallowed = false;
    if is_shallow {
        tracing::info!(
            project_root = %project_root.display(),
            "coupling_index: shallow clone detected; attempting `git fetch --unshallow` before walk"
        );
        let fetch = tokio::time::timeout(
            GIT_LOG_TIMEOUT,
            Command::new("git")
                .current_dir(project_root)
                .args(["fetch", "--unshallow"])
                .output(),
        )
        .await;
        match fetch {
            Ok(Ok(result)) if result.status.success() => {
                unshallowed = true;
            }
            Ok(Ok(result)) => {
                tracing::warn!(
                    project_root = %project_root.display(),
                    stderr = %String::from_utf8_lossy(&result.stderr).trim(),
                    "coupling_index: `git fetch --unshallow` non-zero; ingesting visible history only"
                );
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    project_root = %project_root.display(),
                    error = %e,
                    "coupling_index: `git fetch --unshallow` failed to spawn; ingesting visible history only"
                );
            }
            Err(_) => {
                tracing::warn!(
                    project_root = %project_root.display(),
                    "coupling_index: `git fetch --unshallow` timed out; ingesting visible history only"
                );
            }
        }
    }
    let output = run_git_log_once(project_root, range).await?;
    Ok((output, unshallowed))
}

async fn run_git_log_once(
    project_root: &Path,
    range: &str,
) -> Result<String, IngestError> {
    // `--no-merges`: merge commits muddy the "changed together" signal.
    // `-M` / `-C`: surface renames and copies with a similarity score.
    // `--date-order`: stabilise output ordering across git versions.
    let pretty = format!("{COMMIT_SENTINEL}%H|%cI|%aE");
    let args = [
        "log",
        "--no-merges",
        "--name-status",
        "--numstat",
        "-M",
        "-C",
        "--date-order",
        "--pretty=format:",
    ];
    // Build the command incrementally so we can append `--pretty=` and
    // the range without a format-string allocation in the happy path.
    let mut cmd = Command::new("git");
    cmd.current_dir(project_root);
    cmd.arg(args[0]);
    for arg in &args[1..args.len() - 1] {
        cmd.arg(arg);
    }
    cmd.arg(format!("--pretty=format:{pretty}"));
    cmd.arg(range);

    let output = tokio::time::timeout(GIT_LOG_TIMEOUT, cmd.output())
        .await
        .map_err(|_| IngestError::Timeout(GIT_LOG_TIMEOUT))??;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        // `git log cursor..HEAD` on a repo that has zero new commits
        // exits 0 with empty output. A hard error here means the range
        // is bad (e.g. cursor unreachable) — bubble it so the caller
        // can log it.
        return Err(IngestError::GitFailed {
            status: format!("{}", output.status),
            stderr,
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parsed view over `git log --name-status --numstat` output.
#[derive(Debug, Default)]
struct ParsedLog {
    rows: Vec<CommitFileChange>,
    commits_seen: usize,
}

/// Parse the interleaved `--name-status` / `--numstat` stream emitted by
/// `git log`. Layout (per commit):
///
/// ```text
/// __COMMIT__<sha>|<iso-date>|<author-email>
/// <insertions>\t<deletions>\t<path>          (numstat, one per file)
/// …
/// <kind>\t<path>                             (name-status, one per file)
/// …
/// ```
///
/// git emits numstat first, then name-status, separated by a blank
/// line. Renames / copies show `R<score>\told\tnew` in name-status and
/// `<ins>\t<del>\told => new` or `<ins>\t<del>\t{old => new}` in
/// numstat. We key on the post-rename path so `file_path` always means
/// "the path after the commit".
fn parse_git_log(raw: &str, project_id: &str) -> Result<ParsedLog, String> {
    let mut out = ParsedLog::default();
    let mut current_sha: Option<String> = None;
    let mut current_date: Option<String> = None;
    let mut current_email: Option<String> = None;

    // Accumulators indexed by the final (post-rename) path for the
    // current commit.
    use std::collections::HashMap;
    let mut numstat: HashMap<String, (i64, i64)> = HashMap::new();
    let mut name_status: Vec<(String, String, Option<String>)> = Vec::new();

    let flush = |out: &mut ParsedLog,
                 current_sha: &Option<String>,
                 current_date: &Option<String>,
                 current_email: &Option<String>,
                 numstat: &HashMap<String, (i64, i64)>,
                 name_status: &Vec<(String, String, Option<String>)>| {
        let (Some(sha), Some(date), Some(email)) =
            (current_sha.as_ref(), current_date.as_ref(), current_email.as_ref())
        else {
            return;
        };
        for (kind, path, old_path) in name_status {
            let (ins, del) = numstat.get(path).copied().unwrap_or((0, 0));
            out.rows.push(CommitFileChange {
                project_id: project_id.to_owned(),
                commit_sha: sha.clone(),
                file_path: path.clone(),
                change_kind: kind.clone(),
                committed_at: date.clone(),
                author_email: email.clone(),
                insertions: ins,
                deletions: del,
                old_path: old_path.clone(),
            });
        }
    };

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix(COMMIT_SENTINEL) {
            // Flush the previous commit before starting a new one.
            flush(
                &mut out,
                &current_sha,
                &current_date,
                &current_email,
                &numstat,
                &name_status,
            );
            numstat.clear();
            name_status.clear();

            let mut parts = rest.splitn(3, '|');
            let sha = parts.next().ok_or("commit header missing sha")?;
            let date = parts.next().ok_or("commit header missing date")?;
            let email = parts.next().ok_or("commit header missing author email")?;
            current_sha = Some(sha.to_owned());
            current_date = Some(date.to_owned());
            current_email = Some(email.to_owned());
            out.commits_seen += 1;
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }

        // numstat: `<ins>\t<del>\t<path>` (binary: `-\t-\t<path>`)
        // name-status: `<kind>\t<path>` or `<kind>\t<old>\t<new>`
        let fields: Vec<&str> = line.split('\t').collect();
        let is_numstat = fields.len() == 3
            && (fields[0] == "-" || fields[0].chars().all(|c| c.is_ascii_digit()));
        if is_numstat {
            let (ins, del) = parse_counts(fields[0], fields[1]);
            // Two rename encodings for numstat:
            //   * brace form:  `src/{old => new}.rs` (preferred)
            //   * bare form:   `old/path => new/path` (rare, older git)
            let path_field = fields[2];
            let path = if path_field.contains('{') && path_field.contains(" => ") {
                unbrace_rename(path_field)
            } else if path_field.contains(" => ") {
                let (_old, new) = split_rename(path_field);
                new.to_owned()
            } else {
                path_field.to_owned()
            };
            numstat.insert(path, (ins, del));
            continue;
        }
        if fields.len() >= 2 {
            // name-status: kind is fields[0], rest is path(s).
            let kind = fields[0].to_owned();
            if kind.starts_with('R') || kind.starts_with('C') {
                if fields.len() >= 3 {
                    name_status.push((kind, fields[2].to_owned(), Some(fields[1].to_owned())));
                }
                continue;
            }
            // Skip type-change ('T') entries — diff-wise they're
            // usually filemode flips (symlink ↔ regular) and the
            // churn / coupling signal is dominated by real content
            // edits. Preserve everything else.
            if kind == "T" {
                continue;
            }
            name_status.push((kind, fields[1].to_owned(), None));
        }
    }

    // Flush the last commit.
    flush(
        &mut out,
        &current_sha,
        &current_date,
        &current_email,
        &numstat,
        &name_status,
    );
    Ok(out)
}

fn parse_counts(ins: &str, del: &str) -> (i64, i64) {
    let parse = |s: &str| -> i64 { s.parse::<i64>().unwrap_or(0) };
    (parse(ins), parse(del))
}

/// Resolve git's brace-rename notation `a/{old => new}/b.rs` → `a/new/b.rs`.
/// Non-rename paths pass through unchanged.
fn unbrace_rename(path: &str) -> String {
    if let Some(open) = path.find('{')
        && let Some(close_rel) = path[open..].find('}')
    {
        let close = open + close_rel;
        let inner = &path[open + 1..close];
        if let Some(arrow) = inner.find(" => ") {
            let new = &inner[arrow + 4..];
            let prefix = &path[..open];
            let suffix = &path[close + 1..];
            return format!("{prefix}{new}{suffix}");
        }
    }
    path.to_owned()
}

fn split_rename(field: &str) -> (&str, &str) {
    match field.find(" => ") {
        Some(idx) => (&field[..idx], &field[idx + 4..]),
        None => ("", field),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_handles_edit_add_delete_rename_binary() {
        // Shape: two commits. Commit 1 has an add + a normal edit.
        // Commit 2 has a rename + a binary edit + a delete.
        let raw = concat!(
            "__COMMIT__abc123|2026-04-01T00:00:00Z|dev@example.com\n",
            "5\t0\tsrc/new_file.rs\n",
            "2\t1\tsrc/existing.rs\n",
            "\n",
            "A\tsrc/new_file.rs\n",
            "M\tsrc/existing.rs\n",
            "__COMMIT__def456|2026-04-02T00:00:00Z|dev@example.com\n",
            "0\t0\tassets/logo.png\n",
            "3\t4\tsrc/renamed.rs\n",
            "0\t7\tsrc/gone.rs\n",
            "\n",
            "R100\tsrc/old_name.rs\tsrc/renamed.rs\n",
            "M\tassets/logo.png\n",
            "D\tsrc/gone.rs\n",
        );
        // Simulate binary numstat: replace the 0/0 counts above with -/-
        // for the logo file via a targeted rewrite.
        let raw = raw.replace("0\t0\tassets/logo.png", "-\t-\tassets/logo.png");

        let parsed = parse_git_log(&raw, "p1").expect("parse");
        assert_eq!(parsed.commits_seen, 2);
        assert_eq!(parsed.rows.len(), 5);

        let by_key: std::collections::HashMap<(String, String), &CommitFileChange> =
            parsed
                .rows
                .iter()
                .map(|r| ((r.commit_sha.clone(), r.file_path.clone()), r))
                .collect();

        let add = by_key
            .get(&("abc123".into(), "src/new_file.rs".into()))
            .expect("add row");
        assert_eq!(add.change_kind, "A");
        assert_eq!(add.insertions, 5);
        assert_eq!(add.deletions, 0);
        assert!(add.old_path.is_none());

        let edit = by_key
            .get(&("abc123".into(), "src/existing.rs".into()))
            .expect("edit row");
        assert_eq!(edit.change_kind, "M");
        assert_eq!(edit.insertions, 2);
        assert_eq!(edit.deletions, 1);

        let rename = by_key
            .get(&("def456".into(), "src/renamed.rs".into()))
            .expect("rename row");
        assert_eq!(rename.change_kind, "R100");
        assert_eq!(rename.old_path.as_deref(), Some("src/old_name.rs"));
        assert_eq!(rename.insertions, 3);
        assert_eq!(rename.deletions, 4);

        let binary = by_key
            .get(&("def456".into(), "assets/logo.png".into()))
            .expect("binary row");
        assert_eq!(binary.change_kind, "M");
        assert_eq!(binary.insertions, 0);
        assert_eq!(binary.deletions, 0);

        let delete = by_key
            .get(&("def456".into(), "src/gone.rs".into()))
            .expect("delete row");
        assert_eq!(delete.change_kind, "D");
    }

    #[test]
    fn parse_handles_brace_rename_numstat() {
        let raw = concat!(
            "__COMMIT__aaa|2026-04-01T00:00:00Z|dev@e.com\n",
            "1\t2\tsrc/{old => new}.rs\n",
            "\n",
            "R95\tsrc/old.rs\tsrc/new.rs\n",
        );
        let parsed = parse_git_log(raw, "p1").expect("parse");
        assert_eq!(parsed.rows.len(), 1);
        let r = &parsed.rows[0];
        assert_eq!(r.file_path, "src/new.rs");
        assert_eq!(r.change_kind, "R95");
        assert_eq!(r.insertions, 1);
        assert_eq!(r.deletions, 2);
    }

    #[test]
    fn parse_skips_type_change_entries() {
        let raw = concat!(
            "__COMMIT__aaa|2026-04-01T00:00:00Z|dev@e.com\n",
            "0\t0\tscripts/build\n",
            "\n",
            "T\tscripts/build\n",
        );
        let parsed = parse_git_log(raw, "p1").expect("parse");
        assert!(parsed.rows.is_empty());
        assert_eq!(parsed.commits_seen, 1);
    }

    #[test]
    fn parse_handles_empty_output() {
        let parsed = parse_git_log("", "p1").expect("parse");
        assert!(parsed.rows.is_empty());
        assert_eq!(parsed.commits_seen, 0);
    }

    // End-to-end test: build a tiny git repo in a tempdir, ingest, query.
    // Guarded by #[ignore] so CI without a test-Dolt instance does not
    // fail — flip the ignore to run locally once `:3307` is up.
    #[tokio::test]
    #[ignore = "requires test Dolt at :3307 and a local git binary"]
    async fn end_to_end_ingest_and_query() {
        use djinn_db::Database;

        let tmp = tempfile::Builder::new()
            .prefix("djinn-coupling-e2e-")
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
        tokio::fs::write(root.join("a.txt"), "hi\n").await.unwrap();
        tokio::fs::write(root.join("b.txt"), "yo\n").await.unwrap();
        run(&root, &["add", "."]).await;
        run(&root, &["commit", "-q", "-m", "seed"]).await;
        tokio::fs::write(root.join("a.txt"), "hi again\n").await.unwrap();
        run(&root, &["add", "a.txt"]).await;
        run(&root, &["commit", "-q", "-m", "edit a"]).await;

        let db = Database::open_in_memory().expect("db");
        let stats = ingest_new_commits(&db, "p1", &root).await.expect("ingest");
        assert!(stats.commits_ingested >= 2);
        assert!(stats.rows_inserted >= 3);

        let repo = CommitFileChangeRepository::new(db);
        let coupled = repo.top_coupled("p1", "a.txt", 10).await.expect("coupled");
        assert!(coupled.iter().any(|r| r.file_path == "b.txt"));
    }
}
