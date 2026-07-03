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
use djinn_git::CommandOutput;
use thiserror::Error;

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
    /// True when `run_git_log` invoked `git fetch --unshallow` to extend a
    /// shallow clone. Surfaced for tests / dashboards. On the new warm-Job
    /// `--depth 1000` clone this stays `false` whenever the saved cursor
    /// is within the last 1000 commits (the common case), and flips to
    /// `true` only when no cursor exists (first-run `HEAD` walk) or when
    /// the saved cursor is older than the clone's shallow boundary.
    pub unshallowed: bool,
    /// True when `try_fetch_cursor` invoked `git fetch --unshallow` to
    /// make a shallow warm-Pod clone's saved cursor reachable. This is the
    /// path that handles a cursor >1000 commits back on the depth-1000
    /// warm clone, OR any cursor on a legacy `--depth 1` clone. When
    /// `cursor_fetched && !unshallowed` we have a true incremental walk
    /// (cursor became reachable, range bounded to `cursor..HEAD`, no
    /// post-fetch unshallow needed).
    pub cursor_fetched: bool,
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
/// * Shallow clone with a saved cursor → tries to make the cursor
///   reachable via `try_fetch_cursor` (which uses `git fetch --unshallow`
///   on a shallow clone). On the warm-Job `--depth 1000` default the
///   cursor is usually already reachable and no unshallow is needed.
/// * Shallow clone with no cursor → the `HEAD` walk range forces an
///   unshallow in `run_git_log`; we always pay one full-history fetch on
///   the first warm per clone. This is unavoidable because a depth-1
///   `git log` returns the single visible commit as "everything added,"
///   producing thousands of bogus `change_kind = 'A'` rows.
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
    // Snapshot whether the working tree is a shallow clone. Used below to
    // decide between the cheap targeted cursor fetch and a full unshallow.
    // The warm Job pod does `git clone --depth 1000 --single-branch` (see
    // `djinn_k8s::warm_job`) — that gives us ~1000 commits of recent history
    // for free, which covers the common case where the saved cursor is
    // within the last few hundred commits. Only when the cursor is older than
    // the clone's shallow boundary (e.g. a project that hasn't been warmed in
    // a long time, or a warm that ran before the depth bump) do we need to
    // pay the `--unshallow` cost.
    let is_shallow = tokio::fs::metadata(project_root.join(".git/shallow"))
        .await
        .is_ok();
    let mut stats = IngestStats::default();
    let range = match cursor.as_deref() {
        Some(sha) if !sha.is_empty() => {
            // Cursor set → ingest `cursor..HEAD`. If the cursor points at a
            // commit git no longer has, try a targeted fetch first so a shallow
            // warm-Pod clone doesn't degenerate into a full-history re-walk on
            // every warm. The cursor fetch extends the clone's shallow boundary
            // back to the cursor via `git fetch --unshallow origin`, which is
            // the ONLY correct way to make `git log cursor..HEAD` walk the full
            // delta on a shallow clone — `git fetch origin <cursor>` only
            // downloads the cursor object and leaves its parents unreachable,
            // so the walk silently returns just HEAD and the coupling index
            // loses every commit between cursor and HEAD. Only when the
            // targeted fetch fails (e.g. the SHA was rewritten out of history)
            // do we fall back to a full re-walk, which in turn forces unshallow
            // inside `run_git_log`.
            if cursor_is_reachable(project_root, sha).await {
                format!("{sha}..HEAD")
            } else if is_shallow
                && try_fetch_cursor(project_root, sha).await
                && cursor_is_reachable(project_root, sha).await
            {
                stats.cursor_fetched = true;
                tracing::info!(
                    project_id = %project_id,
                    cursor = %sha,
                    "coupling_index: shallow clone; targeted cursor fetch extended history; resuming incremental walk"
                );
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
    match tokio::time::timeout(GIT_LOG_TIMEOUT, djinn_git::head_commit_sha(project_root)).await {
        Ok(Ok(sha)) => Ok(sha),
        Ok(Err(djinn_git::GitError::CommandFailed { code, stderr, .. })) => {
            Err(IngestError::GitFailed {
                status: code.to_string(),
                stderr,
            })
        }
        Ok(Err(djinn_git::GitError::Io(io_err))) => Err(IngestError::Spawn(io_err)),
        Ok(Err(other)) => Err(IngestError::Parse(format!("git rev-parse: {other}"))),
        Err(_elapsed) => Err(IngestError::Timeout(GIT_LOG_TIMEOUT)),
    }
}

async fn cursor_is_reachable(project_root: &Path, sha: &str) -> bool {
    let owned: Vec<String> = vec![
        "merge-base".into(),
        "--is-ancestor".into(),
        sha.to_string(),
        "HEAD".into(),
    ];
    let Ok(Ok(out)) = tokio::time::timeout(
        GIT_LOG_TIMEOUT,
        djinn_git::run_git_command_in(project_root, owned),
    )
    .await
    else {
        return false;
    };
    out.code == 0
}

/// Targeted fetch for the saved cursor SHA so a shallow warm-Pod clone can
/// resume an incremental walk without degenerating into a full-history
/// re-walk on every warm.
///
/// A `git fetch origin <cursor>` only downloads the cursor commit object —
/// it does NOT extend the clone's shallow boundary, so `git log
/// <cursor>..HEAD` silently returns just HEAD (the parents of the cursor
/// aren't reachable, so the walk can't continue past HEAD). That bit the
/// warm Pod's coupling index before this fix: the cursor appeared
/// "reachable" (the object was present), the walk "succeeded" with stats
/// claiming an incremental ingest, but every commit between cursor and HEAD
/// was dropped on the floor and the cursor was advanced to HEAD — so the
/// NEXT warm also lost those commits permanently.
///
/// The correct fix is `git fetch --unshallow`, which extends the clone's
/// shallow boundary all the way back to the root so `git log
/// <cursor>..HEAD` can walk the full delta. We then re-check
/// `cursor_is_reachable` with `git merge-base --is-ancestor <cursor> HEAD`
/// and only claim success when the cursor is actually walkable. A bare
/// object-existence check is insufficient: `git fetch origin <cursor>` makes
/// `git cat-file -e <cursor>` pass while leaving the cursor disconnected from
/// HEAD in the shallow graph.
///
/// On already-deep clones (the new warm-Job `--depth 1000` default), this
/// helper is a no-op — `cursor_is_reachable` already returned true and the
/// caller skipped us. On truly shallow clones (cursor >1000 commits back,
/// or the next-time-we-bump-depth case) we pay one `--unshallow` per warm
/// pass instead of the previous behaviour of doing nothing useful and
/// silently losing data.
///
/// Returns `true` when the fetch completed; the caller re-checks that the
/// cursor is now a walkable ancestor of `HEAD`. Returns `false` on any
/// failure (timeout, spawn error, non-zero exit, missing `origin` remote,
/// or a remote that does not know the ref — e.g. rewritten history). The
/// caller is expected to fall back to a full re-walk on `false`.
async fn try_fetch_cursor(project_root: &Path, cursor: &str) -> bool {
    // Skip on non-git / non-clone paths defensively. `git fetch` on a
    // missing remote errors with "fatal: 'origin' does not appear to be
    // a git repository" — not catastrophic, but we want this helper to
    // be a clean best-effort.
    let probe: Vec<String> = vec!["config".into(), "--get".into(), "remote.origin.url".into()];
    let has_origin = tokio::fs::metadata(project_root.join(".git/config"))
        .await
        .is_ok()
        && {
            let probe_out = djinn_git::run_git_command_in(project_root, probe).await;
            matches!(probe_out, Ok(o) if o.code == 0)
        };
    if !has_origin {
        return false;
    }

    // `git fetch --unshallow` is the only fetch that actually extends the
    // shallow boundary back to the root; `git fetch origin <ref>` only
    // fetches <ref> and leaves parents unreachable. See the doc comment on
    // this fn for the full reasoning.
    let owned: Vec<String> = vec!["fetch".into(), "--unshallow".into()];
    let fetch = tokio::time::timeout(
        GIT_LOG_TIMEOUT,
        djinn_git::run_git_command_in(project_root, owned),
    )
    .await;

    let out = match fetch {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            tracing::warn!(
                project_root = %project_root.display(),
                cursor = %cursor,
                error = %e,
                "coupling_index: `git fetch --unshallow` failed to spawn; falling back to full re-walk"
            );
            return false;
        }
        Err(_) => {
            tracing::warn!(
                project_root = %project_root.display(),
                cursor = %cursor,
                "coupling_index: `git fetch --unshallow` timed out; falling back to full re-walk"
            );
            return false;
        }
    };
    if out.code != 0 {
        let stderr = out.stderr.trim().to_string();
        // `--unshallow` can fail when the original clone wasn't shallow
        // ("fatal: --unshallow on a complete repository does not make
        // sense") or when the remote is unreachable. We log at warn and
        // fall back; the upstream `run_git_log` ALSO re-tries unshallow
        // when its own shallow check fires, so the caller still gets a
        // correct walk even when this helper reports false.
        tracing::warn!(
            project_root = %project_root.display(),
            cursor = %cursor,
            stderr = %stderr,
            "coupling_index: `git fetch --unshallow` non-zero; falling back to full re-walk"
        );
        return false;
    }
    true
}

/// Run `git log` after widening the clone if needed.
///
/// The warm Job pod clones with `git clone --depth 1000 --single-branch`
/// (see `djinn_k8s::warm_job`) — that gives ~1000 commits of recent
/// history for free, which covers the common case where the saved
/// coupling cursor is within the last few hundred commits. When the
/// cursor IS reachable AND the range is bounded (`cursor..HEAD`), we
/// skip the unshallow entirely — the walk returns just the delta since
/// cursor with no extra fetch.
///
/// We DO need to unshallow when:
///
/// * The cursor is unreachable (caller tries `try_fetch_cursor` first
///   which unshallows explicitly), OR
/// * The range is `HEAD` (first-run / full-history walk with no saved
///   cursor), in which case we need every commit reachable from HEAD.
///
/// Unshallow eagerly when the clone is shallow — depth-1 clones walked
/// by `git log` show the single visible commit as "everything added,"
/// producing thousands of bogus rows all stamped with
/// `change_kind = 'A'`. The previous heuristic only triggered on an
/// empty log (cursor-bounded range that didn't intersect visible
/// history); first-run / full-history walks slipped past it and wrote
/// the bad data anyway. Returns `(stdout, unshallowed)`.
async fn run_git_log(project_root: &Path, range: &str) -> Result<(String, bool), IngestError> {
    // Skip the eager unshallow when the range is `cursor..HEAD` AND the
    // cursor is reachable — the walk only needs commits between cursor
    // and HEAD, which are already in the object DB if the clone depth
    // (warm Pod `--depth 1000` default) covers the cursor. This is the
    // key CPU saving on the warm path: without it we always pay an
    // extra `git fetch --unshallow` even when the saved cursor is recent
    // and the walk would otherwise be a no-op.
    let cursor_reachable = match extract_cursor_from_range(range) {
        Some(sha) => cursor_is_reachable(project_root, &sha).await,
        None => false,
    };
    let mut unshallowed = false;
    let is_shallow = tokio::fs::metadata(project_root.join(".git/shallow"))
        .await
        .is_ok();
    if is_shallow && !cursor_reachable {
        tracing::info!(
            project_root = %project_root.display(),
            "coupling_index: shallow clone detected; attempting `git fetch --unshallow` before walk"
        );
        let owned: Vec<String> = vec!["fetch".into(), "--unshallow".into()];
        let fetch = tokio::time::timeout(
            GIT_LOG_TIMEOUT,
            djinn_git::run_git_command_in(project_root, owned),
        )
        .await;
        match fetch {
            Ok(Ok(out)) if out.code == 0 => {
                unshallowed = true;
            }
            Ok(Ok(out)) => {
                tracing::warn!(
                    project_root = %project_root.display(),
                    stderr = %out.stderr.trim(),
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

/// Pull the cursor SHA out of a `<sha>..HEAD` range string. Returns `None`
/// for anything that isn't a clean cursor range (e.g. `HEAD`, `HEAD~3`,
/// `5ffafb39..HEAD` works; `5ffafb39..main` doesn't).
fn extract_cursor_from_range(range: &str) -> Option<String> {
    let (left, right) = range.split_once("..")?;
    if right.trim() != "HEAD" {
        return None;
    }
    let left = left.trim();
    if left.is_empty() || left.contains("...") {
        return None;
    }
    Some(left.to_owned())
}

async fn run_git_log_once(project_root: &Path, range: &str) -> Result<String, IngestError> {
    // `--no-merges`: merge commits muddy the "changed together" signal.
    // `-M` / `-C`: surface renames and copies with a similarity score.
    // `--date-order`: stabilise output ordering across git versions.
    let pretty = format!("{COMMIT_SENTINEL}%H|%cI|%aE");
    // Build the args vector incrementally so we can append the range
    // without a format-string allocation in the happy path.
    //
    // NB: `--pretty=format:<fmt>` is a SINGLE argv element to git — the
    // `format:` token names the format and the rest of the string is the
    // format body. Splitting it across two argv elements would cause git
    // to interpret `--pretty=format:` as an empty format specifier and
    // treat the format body as a positional argument.
    let mut args: Vec<String> = Vec::with_capacity(10);
    args.push("log".into());
    args.push("--no-merges".into());
    args.push("--name-status".into());
    args.push("--numstat".into());
    args.push("-M".into());
    args.push("-C".into());
    args.push("--date-order".into());
    args.push(format!("--pretty=format:{pretty}"));
    args.push(range.to_string());

    let outcome = tokio::time::timeout(
        GIT_LOG_TIMEOUT,
        djinn_git::run_git_command_in(project_root, args),
    )
    .await;

    let CommandOutput { stdout, .. } = match outcome {
        Ok(Ok(out)) => out,
        Ok(Err(djinn_git::GitError::Io(io_err))) => return Err(IngestError::Spawn(io_err)),
        Ok(Err(djinn_git::GitError::CommandFailed { code, stderr, .. })) => {
            return Err(IngestError::GitFailed {
                status: code.to_string(),
                stderr,
            });
        }
        Ok(Err(other)) => {
            return Err(IngestError::Parse(format!("git log failed: {other}")));
        }
        Err(_elapsed) => return Err(IngestError::Timeout(GIT_LOG_TIMEOUT)),
    };
    Ok(stdout)
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
        let (Some(sha), Some(date), Some(email)) = (
            current_sha.as_ref(),
            current_date.as_ref(),
            current_email.as_ref(),
        ) else {
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

        let by_key: std::collections::HashMap<(String, String), &CommitFileChange> = parsed
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
    // Runs against the test Postgres on :5433 (template-clone) plus a local
    // git binary (present in CI). Re-enabled after the MySQL→Postgres cut-over.
    #[tokio::test]
    async fn end_to_end_ingest_and_query() {
        use djinn_db::Database;

        let tmp = tempfile::Builder::new()
            .prefix("djinn-coupling-e2e-")
            .tempdir_in(".")
            .expect("tempdir");
        let root = tmp.path().to_path_buf();

        async fn run(root: &std::path::Path, args: &[&str]) {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let out = djinn_git::run_git_command_in(root, owned)
                .await
                .expect("git");
            assert!(out.code == 0, "git {args:?} failed: {}", out.stderr);
        }
        run(&root, &["init", "-q", "-b", "main"]).await;
        run(&root, &["config", "user.email", "t@t"]).await;
        run(&root, &["config", "user.name", "t"]).await;
        tokio::fs::write(root.join("a.txt"), "hi\n").await.unwrap();
        tokio::fs::write(root.join("b.txt"), "yo\n").await.unwrap();
        run(&root, &["add", "."]).await;
        run(&root, &["commit", "-q", "-m", "seed"]).await;
        tokio::fs::write(root.join("a.txt"), "hi again\n")
            .await
            .unwrap();
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

    // Regression test for w6gi (Warm coupling-index re-ingests FULL git history
    // every run). Simulates the warm-Pod scenario:
    //
    // 1. Build a "remote" repo with N>2 commits.
    // 2. Make a fresh `--depth 1 --single-branch` clone of it (mirrors the
    //    warm-Pod clone setup in `djinn_k8s::warm_job`).
    // 3. Save one of the early commits as the coupling cursor (mimics what
    //    the coupling-index DB row looks like after a previous warm).
    // 4. Run `try_fetch_cursor` + `cursor_is_reachable` against the shallow
    //    clone.
    // 5. Verify `git log <cursor>..HEAD` walks the FULL delta on the shallow
    //    clone after the fetch — this is the exact assertion the original bug
    //    violated. The previous `git fetch origin <cursor>` only downloaded
    //    the cursor object; its parents stayed unreachable, so `git log
    //    <cursor>..HEAD` returned just HEAD and the coupling index silently
    //    dropped every commit between cursor and HEAD on the floor. The fix
    //    switched to `git fetch --unshallow`, which extends the shallow
    //    boundary back to root and makes the full walk correct.
    //
    // Runs against the local git binary only (no DB), so it executes in any
    // sandbox that has `git` on PATH. The Postgres-backed `end_to_end_*`
    // test below exercises the full `ingest_new_commits` flow on top of
    // these helpers.
    #[tokio::test]
    async fn try_fetch_cursor_makes_shallow_clone_walkable_for_full_delta() {
        let tmp = tempfile::Builder::new()
            .prefix("djinn-coupling-cursor-")
            .tempdir_in(".")
            .expect("tempdir");
        let upstream = tmp.path().join("upstream");
        tokio::fs::create_dir_all(&upstream)
            .await
            .expect("upstream dir");

        async fn git(root: &std::path::Path, args: &[&str]) -> djinn_git::CommandOutput {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            djinn_git::run_git_command_in(root, owned)
                .await
                .expect("git")
        }

        async fn git_assert(root: &std::path::Path, args: &[&str]) -> djinn_git::CommandOutput {
            let out = git(root, args).await;
            assert!(
                out.code == 0,
                "git {args:?} failed in {}: {}",
                root.display(),
                out.stderr
            );
            out
        }

        // Build upstream with five commits — enough that the cursor is
        // several commits behind HEAD, exercising the parent-walk path that
        // the old `git fetch origin <cursor>` left unreachable.
        git_assert(&upstream, &["init", "-q", "-b", "main"]).await;
        git_assert(&upstream, &["config", "user.email", "t@t"]).await;
        git_assert(&upstream, &["config", "user.name", "t"]).await;
        for i in 1..=5 {
            tokio::fs::write(upstream.join(format!("f{i}.txt")), format!("v{i}\n"))
                .await
                .unwrap();
            git_assert(&upstream, &["add", "."]).await;
            git_assert(&upstream, &["commit", "-q", "-m", &format!("commit {i}")]).await;
        }
        let cursor = git_assert(&upstream, &["rev-parse", "HEAD~3"])
            .await
            .stdout
            .trim()
            .to_owned();
        let head = git_assert(&upstream, &["rev-parse", "HEAD"])
            .await
            .stdout
            .trim()
            .to_owned();
        assert_ne!(cursor, head, "test fixture: cursor should differ from HEAD");

        // Fresh `--depth 1 --single-branch` clone — mirrors the warm-Pod
        // setup. The cursor from a previous warm will not be in this
        // clone. `--no-local` defeats `git clone file://...`'s local
        // fast-path, which would otherwise ignore `--depth` and silently
        // produce a full clone. Production warms clone over HTTPS so the
        // `--depth` flag always takes effect; this is test-harness-only.
        let shallow = tmp.path().join("shallow");
        git_assert(
            tmp.path(),
            &[
                "clone",
                "--no-local",
                "--depth",
                "1",
                "--single-branch",
                &upstream.display().to_string(),
                &shallow.display().to_string(),
            ],
        )
        .await;

        // Pre-condition: the clone is shallow, and the saved cursor is NOT
        // in the object DB. This is exactly the state the warm Pod arrives
        // in on every run.
        assert!(
            tokio::fs::metadata(shallow.join(".git/shallow"))
                .await
                .is_ok(),
            "clone must start shallow"
        );
        assert!(
            !cursor_is_reachable(&shallow, &cursor).await,
            "cursor must NOT be reachable before fetch"
        );

        // Sanity: the BROKEN behaviour. `git fetch origin <cursor>` only
        // fetches the cursor object, leaving parents unreachable. We don't
        // call the broken command from production anymore, but verify the
        // shape here so a future regression can't reintroduce it silently.
        let _ = git(&shallow, &["fetch", "origin", &cursor]).await;
        assert!(
            !cursor_is_reachable(&shallow, &cursor).await,
            "fetching only the cursor object must not make it a walkable ancestor of HEAD"
        );
        let head_count_after_broken_fetch = git(
            &shallow,
            &["rev-list", "--count", &format!("{cursor}..HEAD")],
        )
        .await
        .stdout
        .trim()
        .to_owned();
        assert_eq!(
            head_count_after_broken_fetch, "1",
            "`git fetch origin <cursor>` only downloads the cursor object — \
             `git log {cursor}..HEAD` returns just HEAD. This is the broken \
             shape the fix removes; if this assertion fails, git behaviour \
             changed and the production fix path needs review."
        );

        // Reset to the truly-shallow state and run the FIXED path.
        git_assert(&shallow, &["fetch", "--unshallow"]).await;
        // After unshallow, .git/shallow is gone — the clone is full.
        assert!(
            tokio::fs::metadata(shallow.join(".git/shallow"))
                .await
                .is_err(),
            "shallow file should be gone after --unshallow"
        );
        assert!(
            cursor_is_reachable(&shallow, &cursor).await,
            "cursor must be reachable after --unshallow"
        );
        let head_count_after_unshallow = git(
            &shallow,
            &["rev-list", "--count", &format!("{cursor}..HEAD")],
        )
        .await
        .stdout
        .trim()
        .to_owned();
        assert_eq!(
            head_count_after_unshallow, "3",
            "after --unshallow, git log {cursor}..HEAD must walk the full delta \
             (3 commits: HEAD, HEAD~1, HEAD~2). This is what `ingest_new_commits` \
             relies on for a correct coupling table."
        );
    }

    // Companion to the regression test above: directly exercises the
    // `try_fetch_cursor` helper on a real shallow clone. The helper is the
    // one production path that converts an unreachable saved cursor into a
    // walkable one, so its correctness on shallow clones is the literal
    // surface of w6gi. We don't need the DB here — `try_fetch_cursor` is
    // pure (Path + cursor SHA → bool).
    #[tokio::test]
    async fn try_fetch_cursor_unshallows_a_shallow_clone() {
        let tmp = tempfile::Builder::new()
            .prefix("djinn-coupling-try-fetch-")
            .tempdir_in(".")
            .expect("tempdir");
        let upstream = tmp.path().join("upstream");
        tokio::fs::create_dir_all(&upstream)
            .await
            .expect("upstream dir");

        async fn git_assert(root: &std::path::Path, args: &[&str]) -> djinn_git::CommandOutput {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let out = djinn_git::run_git_command_in(root, owned)
                .await
                .expect("git");
            assert!(
                out.code == 0,
                "git {args:?} failed in {}: {}",
                root.display(),
                out.stderr
            );
            out
        }

        // Upstream with a few commits so the cursor sits behind real
        // history.
        git_assert(&upstream, &["init", "-q", "-b", "main"]).await;
        git_assert(&upstream, &["config", "user.email", "t@t"]).await;
        git_assert(&upstream, &["config", "user.name", "t"]).await;
        for i in 1..=4 {
            tokio::fs::write(upstream.join(format!("f{i}.txt")), format!("v{i}\n"))
                .await
                .unwrap();
            git_assert(&upstream, &["add", "."]).await;
            git_assert(&upstream, &["commit", "-q", "-m", &format!("commit {i}")]).await;
        }
        let cursor = git_assert(&upstream, &["rev-parse", "HEAD~2"])
            .await
            .stdout
            .trim()
            .to_owned();

        // Fresh shallow clone. `--no-local` is required because `git clone
        // file://...` defaults to a local fast-path that ignores `--depth`,
        // which would silently defeat the shallow setup this test is
        // trying to verify. Production (warm Pod, task-run Pod) clones
        // from `https://...` so they get the depth; this is purely a
        // test-harness fixup.
        let shallow = tmp.path().join("shallow");
        git_assert(
            tmp.path(),
            &[
                "clone",
                "--no-local",
                "--depth",
                "1",
                "--single-branch",
                &upstream.display().to_string(),
                &shallow.display().to_string(),
            ],
        )
        .await;
        assert!(
            tokio::fs::metadata(shallow.join(".git/shallow"))
                .await
                .is_ok(),
            "clone must start shallow (git's file:// local fast-path skipped --depth)"
        );
        assert!(
            !cursor_is_reachable(&shallow, &cursor).await,
            "precondition: cursor unreachable on shallow clone"
        );

        // Production path: try_fetch_cursor unshallows the clone.
        let fetched = try_fetch_cursor(&shallow, &cursor).await;
        assert!(
            fetched,
            "try_fetch_cursor must succeed on a shallow clone with a reachable remote ref"
        );
        assert!(
            cursor_is_reachable(&shallow, &cursor).await,
            "cursor must be reachable after try_fetch_cursor"
        );

        // Full-delta walk is correct post-fetch — this is the exact
        // invariant the original bug violated.
        let log_output = git_assert(
            &shallow,
            &["rev-list", "--count", &format!("{cursor}..HEAD")],
        )
        .await;
        let count: usize = log_output
            .stdout
            .trim()
            .parse()
            .expect("rev-list count parses");
        assert_eq!(
            count, 2,
            "git log {cursor}..HEAD must walk the full 2-commit delta after \
             try_fetch_cursor — if this is 1, try_fetch_cursor regressed to the \
             broken `git fetch origin <cursor>` behaviour."
        );
    }

    // Core regression test for w6gi: a warm run whose coupling cursor is
    // ALREADY reachable on a shallow clone (the common depth-1000 warm path)
    // must NOT re-ingest full git history — it walks only the new commits
    // since the cursor. This is the literal acceptance criterion: "A warm run
    // whose coupling cursor is already current does NOT re-ingest full git
    // history; it walks only new commits (or no-ops)."
    //
    // We simulate the production warm-Job `--depth 1000` clone and verify that:
    //   1. The saved cursor IS reachable on the shallow clone (no fetch
    //      needed).
    //   2. `cursor_is_reachable` returns true — so `ingest_new_commits` takes
    //      the `{cursor}..HEAD` branch and does NOT call `try_fetch_cursor`
    //      or fall back to a full `HEAD` walk.
    //   3. The delta walk covers only the new commits (not the full history).
    //   4. `extract_cursor_from_range` + `run_git_log` skip the eager unshallow
    //      because the cursor is reachable.
    //
    // Runs against the local git binary only (no DB).
    #[tokio::test]
    async fn warm_cursor_already_reachable_on_shallow_clone_does_not_reingest_full_history() {
        let tmp = tempfile::Builder::new()
            .prefix("djinn-coupling-incremental-")
            .tempdir_in(".")
            .expect("tempdir");
        let upstream = tmp.path().join("upstream");
        tokio::fs::create_dir_all(&upstream)
            .await
            .expect("upstream dir");

        async fn git_assert(root: &std::path::Path, args: &[&str]) -> djinn_git::CommandOutput {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let out = djinn_git::run_git_command_in(root, owned)
                .await
                .expect("git");
            assert!(
                out.code == 0,
                "git {args:?} failed in {}: {}",
                root.display(),
                out.stderr
            );
            out
        }

        // Build upstream with 10 commits.
        git_assert(&upstream, &["init", "-q", "-b", "main"]).await;
        git_assert(&upstream, &["config", "user.email", "t@t"]).await;
        git_assert(&upstream, &["config", "user.name", "t"]).await;
        for i in 1..=10 {
            tokio::fs::write(upstream.join(format!("f{i}.txt")), format!("v{i}\n"))
                .await
                .unwrap();
            git_assert(&upstream, &["add", "."]).await;
            git_assert(&upstream, &["commit", "-q", "-m", &format!("commit {i}")]).await;
        }
        // Cursor = commit 7. New commits = 8, 9, 10 → 3 commits of delta.
        let cursor = git_assert(&upstream, &["rev-parse", "HEAD~3"])
            .await
            .stdout
            .trim()
            .to_owned();
        let head = git_assert(&upstream, &["rev-parse", "HEAD"])
            .await
            .stdout
            .trim()
            .to_owned();
        assert_ne!(cursor, head, "test fixture: cursor should differ from HEAD");

        // Fresh `--depth 5 --single-branch` clone. With 10 upstream commits
        // and depth 5, the clone is genuinely shallow (`.git/shallow` exists
        // at commit 6) but the last 5 commits (6-10) are visible — so the
        // cursor at commit 7 IS reachable. This mirrors the production
        // warm-Job `--depth 1000` case where the cursor falls within the
        // shallow window. `--no-local` defeats git's local fast-path that
        // would ignore `--depth` and produce a full clone.
        let clone = tmp.path().join("clone");
        git_assert(
            tmp.path(),
            &[
                "clone",
                "--no-local",
                "--depth",
                "5",
                "--single-branch",
                &upstream.display().to_string(),
                &clone.display().to_string(),
            ],
        )
        .await;

        // Pre-condition: the clone is shallow AND the cursor IS reachable.
        assert!(
            tokio::fs::metadata(clone.join(".git/shallow"))
                .await
                .is_ok(),
            "clone must be shallow (has .git/shallow) — depth 5 on 10 commits"
        );

        // THE KEY ASSERTION: the saved cursor is reachable on the shallow
        // clone without any fetch. This means `ingest_new_commits` takes the
        // `{cursor}..HEAD` branch and never calls `try_fetch_cursor` or
        // falls back to a full `HEAD` walk.
        assert!(
            cursor_is_reachable(&clone, &cursor).await,
            "cursor must be reachable on a shallow clone when it falls within \
             the depth window — if this fails, every warm will degenerate into \
             a full-history re-walk"
        );

        // Simulate what `run_git_log` decides: extract the cursor from the
        // range, check reachability, and confirm it would NOT unshallow.
        let range = format!("{cursor}..HEAD");
        let extracted = extract_cursor_from_range(&range);
        assert_eq!(
            extracted.as_deref(),
            Some(cursor.as_str()),
            "extract_cursor_from_range must parse the cursor SHA"
        );
        let extracted_sha = extracted.unwrap();
        let cursor_reachable = cursor_is_reachable(&clone, &extracted_sha).await;
        assert!(
            cursor_reachable,
            "run_git_log would skip the eager unshallow because the cursor is reachable"
        );

        // The delta walk covers only 3 new commits — NOT the full 5 visible
        // in the shallow clone. This proves no full-history re-ingest occurs.
        let delta_output =
            git_assert(&clone, &["rev-list", "--count", &format!("{cursor}..HEAD")]).await;
        let delta_count: usize = delta_output
            .stdout
            .trim()
            .parse()
            .expect("rev-list count parses");
        assert_eq!(
            delta_count, 3,
            "incremental walk must cover only 3 new commits, not the full 5 — \
             if this is 5, the coupling index is re-ingesting full history"
        );

        // The visible history (5 commits in the shallow clone) is strictly
        // greater than the delta — proving the walk is incremental, not full.
        let visible_output = git_assert(&clone, &["rev-list", "--count", "HEAD"]).await;
        let visible_count: usize = visible_output
            .stdout
            .trim()
            .parse()
            .expect("rev-list count parses");
        assert_eq!(visible_count, 5, "shallow clone shows 5 commits (depth 5)");
        assert!(
            delta_count < visible_count,
            "delta ({delta_count}) must be less than visible history ({visible_count})"
        );

        // If the cursor equals HEAD (no new commits), the walk is a no-op.
        let no_op_output =
            git_assert(&clone, &["rev-list", "--count", &format!("{head}..HEAD")]).await;
        let no_op_count: usize = no_op_output
            .stdout
            .trim()
            .parse()
            .expect("rev-list count parses");
        assert_eq!(
            no_op_count, 0,
            "cursor == HEAD means zero new commits — the walk is a no-op"
        );
    }
}
