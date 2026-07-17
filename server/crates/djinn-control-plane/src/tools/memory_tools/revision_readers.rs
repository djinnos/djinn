//! Ledger-backed readers behind `memory_history`, `memory_diff`, and
//! `memory_session_diff`.
//!
//! Every query goes through the project-scoped `NoteRepository` revision
//! readers delivered by lre2 — there is no direct SQL and no git fallback.
//! Cursor validation and public limit bounds live here, at the tool
//! boundary; the repository applies its own bounded-page cap, so this module
//! aggregates bounded pages up to the public limit.
//!
//! Authorization model (mirrors the established `require_admin` convention):
//! the trusted local/system path (no authenticated user context) and admin
//! users hold note-read/audit permission. Retained deleted-note history is
//! audit-only, and `memory_session_diff` redacts content bodies for callers
//! without note-read permission. Inaccessible or cross-project identifiers
//! are uniformly reported as not found, without existence disclosure.

use djinn_db::repositories::note::NoteHistoryRequest;
use djinn_db::repositories::session::SessionRepository;
use djinn_db::repositories::task_run::TaskRunRepository;
use djinn_db::{
    NoteRepository, NoteRevisionEventKind, NoteRevisionEventRow, RevisionCursor,
    RevisionLookupRequest, RevisionRangeRequest, SessionRevisionRequest,
};

use crate::server::DjinnMcpServer;
use crate::tools::acting_user::require_admin;

use super::{
    DiffParams, HISTORY_LIMIT_DEFAULT, HISTORY_LIMIT_MAX, HISTORY_START_LEDGER,
    HISTORY_START_MIGRATION_CUTOVER, HistoryParams, MemoryDiffEndpoint, MemoryDiffResponse,
    MemoryHistoryResponse, MemoryRevisionEvent, MemorySessionDiffResponse,
    SESSION_DIFF_LIMIT_DEFAULT, SESSION_DIFF_LIMIT_MAX, SessionDiffParams,
};

/// Structured invalid-parameters envelope shared with the mutation tools
/// (`invalid parameters: field: <field>, message: <message>`).
fn invalid_params(field: &str, message: &str) -> String {
    format!("invalid parameters: field: {field}, message: {message}")
}

/// Content-bearing event kinds accepted as `memory_diff` endpoints.
fn is_content_bearing(kind: NoteRevisionEventKind) -> bool {
    matches!(
        kind,
        NoteRevisionEventKind::Created
            | NoteRevisionEventKind::Updated
            | NoteRevisionEventKind::Deleted
    )
}

/// Note-read permission for session-diff body visibility and audit
/// authorization for retained deleted-note history. Follows the established
/// admin-gate convention: the trusted local/system path (no user context)
/// and admin users hold it; authenticated non-admins do not.
pub(crate) async fn caller_has_note_read_permission(server: &DjinnMcpServer) -> bool {
    require_admin(server.state.db()).await.is_ok()
}

fn map_event(row: &NoteRevisionEventRow, redact_bodies: bool) -> MemoryRevisionEvent {
    MemoryRevisionEvent {
        revision_id: row.id.clone(),
        note_id: row.note_id.clone(),
        note_seq: row.note_seq,
        event_kind: row.event_kind.as_str().to_owned(),
        content_before: if redact_bodies {
            None
        } else {
            row.snapshot.content_before.clone()
        },
        content_after: if redact_bodies {
            None
        } else {
            row.snapshot.content_after.clone()
        },
        confidence_before: row.snapshot.confidence_before,
        confidence_after: row.snapshot.confidence_after,
        actor_kind: row.attribution.actor_kind().as_str().to_owned(),
        actor_id: row.attribution.actor_id().map(ToOwned::to_owned),
        subsystem: row.attribution.subsystem().map(ToOwned::to_owned),
        session_id: row.provenance.session_id().map(ToOwned::to_owned),
        task_id: row.provenance.task_id().map(ToOwned::to_owned),
        task_run_id: row.provenance.task_run_id().map(ToOwned::to_owned),
        reason: row.reason.as_str().to_owned(),
        created_at: row.created_at.clone(),
        content_redacted: redact_bodies,
    }
}

fn validate_limit(raw: Option<i64>, default: i64, max: i64) -> Result<usize, String> {
    let limit = raw.unwrap_or(default);
    if !(1..=max).contains(&limit) {
        return Err(invalid_params(
            "limit",
            &format!("limit must be between 1 and {max}"),
        ));
    }
    Ok(limit as usize)
}

fn non_blank(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.trim().is_empty())
}

fn note_repository(server: &DjinnMcpServer) -> NoteRepository {
    NoteRepository::new(server.state.db().clone(), server.state.event_bus())
}

// ── memory_history ───────────────────────────────────────────────────────────

pub(crate) async fn note_history(
    server: &DjinnMcpServer,
    p: HistoryParams,
) -> MemoryHistoryResponse {
    fn error_response(error: String) -> MemoryHistoryResponse {
        MemoryHistoryResponse {
            events: vec![],
            next_cursor: None,
            history_start: None,
            error: Some(error),
        }
    }

    let project_id = match super::ops::resolve_project_id(server, &p.project).await {
        Ok(id) => id,
        Err(error) => return error_response(error),
    };
    let limit = match validate_limit(p.limit, HISTORY_LIMIT_DEFAULT, HISTORY_LIMIT_MAX) {
        Ok(limit) => limit,
        Err(error) => return error_response(error),
    };
    if let Some(raw) = non_blank(p.before_cursor.as_deref())
        && RevisionCursor::decode_note_history(raw).is_err()
    {
        return error_response(invalid_params("before_cursor", "invalid cursor"));
    }

    let repo = note_repository(server);
    let (events, next_cursor, note_exists) = {
        let mut events: Vec<NoteRevisionEventRow> = Vec::new();
        let mut cursor = non_blank(p.before_cursor.as_deref()).map(ToOwned::to_owned);
        loop {
            let page = match repo
                .note_revision_history(NoteHistoryRequest {
                    project_id: &project_id,
                    note_id: &p.note_id,
                    limit: limit - events.len(),
                    before: cursor.as_deref(),
                })
                .await
            {
                Ok(page) => page,
                Err(error) => return error_response(error.to_string()),
            };
            let note_exists = page.note_exists;
            cursor = page.next_cursor;
            events.extend(page.events);
            if events.len() >= limit || cursor.is_none() {
                break (events, cursor, note_exists);
            }
        }
    };

    if events.is_empty() && !note_exists {
        return error_response(format!("note not found: {}", p.note_id));
    }
    if !note_exists && !caller_has_note_read_permission(server).await {
        // Retained deleted-note history is audit-only; do not disclose it.
        return error_response(format!("note not found: {}", p.note_id));
    }
    // A live note with no retained events predates the ledger. An empty page
    // behind a caller-supplied cursor is an exhausted ledger history instead.
    let history_start = if events.is_empty() && p.before_cursor.is_none() {
        HISTORY_START_MIGRATION_CUTOVER
    } else {
        HISTORY_START_LEDGER
    };
    MemoryHistoryResponse {
        events: events.iter().map(|row| map_event(row, false)).collect(),
        next_cursor,
        history_start: Some(history_start.to_owned()),
        error: None,
    }
}

// ── memory_diff ──────────────────────────────────────────────────────────────

fn diff_endpoint(row: &NoteRevisionEventRow) -> MemoryDiffEndpoint {
    MemoryDiffEndpoint {
        revision_id: row.id.clone(),
        note_seq: row.note_seq,
        event_kind: row.event_kind.as_str().to_owned(),
        created_at: row.created_at.clone(),
    }
}

/// The diff's "before" text: the from-endpoint's explicit `content_before`
/// snapshot. Null is coerced to empty only at the create-before edge; every
/// other content-bearing endpoint persists a non-null snapshot.
fn endpoint_before(event: &NoteRevisionEventRow) -> Result<String, String> {
    match (event.event_kind, event.snapshot.content_before.as_deref()) {
        (NoteRevisionEventKind::Created, None) => Ok(String::new()),
        (_, Some(content)) => Ok(content.to_owned()),
        (_, None) => Err(format!(
            "revision snapshot is missing content: {}",
            event.id
        )),
    }
}

/// The diff's "after" text: the to-endpoint's explicit `content_after`
/// snapshot. Null is coerced to empty only at the delete-after edge.
fn endpoint_after(event: &NoteRevisionEventRow) -> Result<String, String> {
    match (event.event_kind, event.snapshot.content_after.as_deref()) {
        (NoteRevisionEventKind::Deleted, None) => Ok(String::new()),
        (_, Some(content)) => Ok(content.to_owned()),
        (_, None) => Err(format!(
            "revision snapshot is missing content: {}",
            event.id
        )),
    }
}

pub(crate) async fn note_diff(server: &DjinnMcpServer, p: DiffParams) -> MemoryDiffResponse {
    fn error_response(error: String) -> MemoryDiffResponse {
        MemoryDiffResponse {
            from: None,
            to: None,
            diff: String::new(),
            intervening_events: vec![],
            error: Some(error),
        }
    }

    let project_id = match super::ops::resolve_project_id(server, &p.project).await {
        Ok(id) => id,
        Err(error) => return error_response(error),
    };
    let repo = note_repository(server);

    // Both endpoint lookups are scoped to (project, note) so inaccessible,
    // cross-project, and foreign-note revision IDs are indistinguishable.
    let from = match repo
        .revision_lookup(RevisionLookupRequest {
            project_id: &project_id,
            note_id: &p.note_id,
            revision_id: &p.from_revision_id,
        })
        .await
    {
        Ok(Some(row)) => row,
        Ok(None) => {
            return error_response(format!("revision not found: {}", p.from_revision_id));
        }
        Err(error) => return error_response(error.to_string()),
    };
    let to = match repo
        .revision_lookup(RevisionLookupRequest {
            project_id: &project_id,
            note_id: &p.note_id,
            revision_id: &p.to_revision_id,
        })
        .await
    {
        Ok(Some(row)) => row,
        Ok(None) => {
            return error_response(format!("revision not found: {}", p.to_revision_id));
        }
        Err(error) => return error_response(error.to_string()),
    };

    if !is_content_bearing(from.event_kind) {
        return error_response(invalid_params(
            "from_revision_id",
            "revision is not a content-bearing event",
        ));
    }
    if !is_content_bearing(to.event_kind) {
        return error_response(invalid_params(
            "to_revision_id",
            "revision is not a content-bearing event",
        ));
    }

    let (Some(from_seq), Some(to_seq)) = (from.note_seq, to.note_seq) else {
        return error_response("revision is missing a note sequence".to_owned());
    };
    if from_seq >= to_seq {
        return error_response(invalid_params(
            "from_revision_id",
            "from_revision_id must identify an older revision than to_revision_id",
        ));
    }

    let range = match repo
        .revision_range(RevisionRangeRequest {
            project_id: &project_id,
            note_id: &p.note_id,
            to_seq,
            from_seq,
        })
        .await
    {
        Ok(range) => range,
        Err(error) => return error_response(error.to_string()),
    };
    let intervening_events = range
        .iter()
        .filter(|row| row.id != from.id && row.id != to.id)
        .filter(|row| !is_content_bearing(row.event_kind))
        .map(|row| map_event(row, false))
        .collect();

    let before = match endpoint_before(&from) {
        Ok(before) => before,
        Err(error) => return error_response(error),
    };
    let after = match endpoint_after(&to) {
        Ok(after) => after,
        Err(error) => return error_response(error),
    };
    MemoryDiffResponse {
        from: Some(diff_endpoint(&from)),
        to: Some(diff_endpoint(&to)),
        diff: unified_diff(&before, &after),
        intervening_events,
        error: None,
    }
}

// ── memory_session_diff ──────────────────────────────────────────────────────

/// The exclusive session-diff selector validated at the public boundary.
#[derive(Debug, Clone, Copy)]
pub(crate) enum SessionDiffSelector<'a> {
    Session(&'a str),
    TaskRun(&'a str),
}

/// Reusable session/task-run revision reader shared with the task timeline.
///
/// Returns the mapped page (newest-first by `(created_at, id)`, every event
/// kind) and the opaque cursor for the next page. `redact_bodies` withholds
/// `content_before`/`content_after` while preserving event metadata.
pub(crate) async fn session_revision_events(
    server: &DjinnMcpServer,
    project_id: &str,
    selector: SessionDiffSelector<'_>,
    limit: usize,
    before_cursor: Option<&str>,
    redact_bodies: bool,
) -> Result<(Vec<MemoryRevisionEvent>, Option<String>), String> {
    let repo = note_repository(server);
    let mut events = Vec::new();
    let mut next_cursor = before_cursor.map(ToOwned::to_owned);
    loop {
        let request = SessionRevisionRequest {
            project_id,
            limit: limit - events.len(),
            before: next_cursor.as_deref(),
        };
        let page = match selector {
            SessionDiffSelector::Session(session_id) => {
                repo.session_revision_history(session_id, request).await
            }
            SessionDiffSelector::TaskRun(task_run_id) => {
                repo.task_run_revision_history(task_run_id, request).await
            }
        }
        .map_err(|error| error.to_string())?;
        next_cursor = page.next_cursor;
        events.extend(page.events.iter().map(|row| map_event(row, redact_bodies)));
        if events.len() >= limit || next_cursor.is_none() {
            break;
        }
    }
    Ok((events, next_cursor))
}

pub(crate) async fn session_diff(
    server: &DjinnMcpServer,
    p: SessionDiffParams,
) -> MemorySessionDiffResponse {
    fn error_response(error: String) -> MemorySessionDiffResponse {
        MemorySessionDiffResponse {
            events: vec![],
            next_cursor: None,
            error: Some(error),
        }
    }

    let project_id = match super::ops::resolve_project_id(server, &p.project).await {
        Ok(id) => id,
        Err(error) => return error_response(error),
    };
    let selector = match (
        non_blank(p.session_id.as_deref()),
        non_blank(p.task_run_id.as_deref()),
    ) {
        (Some(_), Some(_)) => {
            return error_response(invalid_params(
                "task_run_id",
                "exactly one of session_id or task_run_id must be provided",
            ));
        }
        (None, None) => {
            return error_response(invalid_params(
                "session_id",
                "exactly one of session_id or task_run_id must be provided",
            ));
        }
        (Some(session_id), None) => SessionDiffSelector::Session(session_id),
        (None, Some(task_run_id)) => SessionDiffSelector::TaskRun(task_run_id),
    };
    let limit = match validate_limit(p.limit, SESSION_DIFF_LIMIT_DEFAULT, SESSION_DIFF_LIMIT_MAX) {
        Ok(limit) => limit,
        Err(error) => return error_response(error),
    };
    if let Some(raw) = non_blank(p.before_cursor.as_deref())
        && RevisionCursor::decode_session(raw).is_err()
    {
        return error_response(invalid_params("before_cursor", "invalid cursor"));
    }

    // Existence is validated inside the caller's project so cross-project
    // selectors are indistinguishable from unknown ones.
    match selector {
        SessionDiffSelector::Session(session_id) => {
            let repo = SessionRepository::new(server.state.db().clone(), server.state.event_bus());
            match repo.get_in_project(&project_id, session_id).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return error_response(format!("session not found: {session_id}"));
                }
                Err(error) => return error_response(error.to_string()),
            }
        }
        SessionDiffSelector::TaskRun(task_run_id) => {
            let repo = TaskRunRepository::new(server.state.db().clone());
            match repo.get(task_run_id).await {
                Ok(Some(run)) if run.project_id == project_id => {}
                Ok(_) => {
                    return error_response(format!("task run not found: {task_run_id}"));
                }
                Err(error) => return error_response(error.to_string()),
            }
        }
    }

    let redact_bodies = !caller_has_note_read_permission(server).await;
    match session_revision_events(
        server,
        &project_id,
        selector,
        limit,
        non_blank(p.before_cursor.as_deref()),
        redact_bodies,
    )
    .await
    {
        Ok((events, next_cursor)) => MemorySessionDiffResponse {
            events,
            next_cursor,
            error: None,
        },
        Err(error) => error_response(error),
    }
}

// ── Deterministic unified text diff ──────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
struct DiffLine<'a> {
    text: &'a str,
    newline: bool,
}

fn split_diff_lines(content: &str) -> Vec<DiffLine<'_>> {
    let mut lines = Vec::new();
    let mut rest = content;
    while let Some(idx) = rest.find('\n') {
        lines.push(DiffLine {
            text: &rest[..idx],
            newline: true,
        });
        rest = &rest[idx + 1..];
    }
    if !rest.is_empty() {
        lines.push(DiffLine {
            text: rest,
            newline: false,
        });
    }
    lines
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffOp {
    Equal,
    Delete,
    Insert,
}

/// Line edit script over both sequences. The common prefix/suffix is trimmed
/// before a bounded LCS dynamic program; when the table would exceed a fixed
/// cell budget the whole middle degrades to a deterministic replacement.
fn diff_ops(before: &[DiffLine<'_>], after: &[DiffLine<'_>]) -> Vec<DiffOp> {
    let mut prefix = 0;
    while prefix < before.len() && prefix < after.len() && before[prefix] == after[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < before.len() - prefix
        && suffix < after.len() - prefix
        && before[before.len() - 1 - suffix] == after[after.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let b_mid = &before[prefix..before.len() - suffix];
    let a_mid = &after[prefix..after.len() - suffix];

    let mut ops = vec![DiffOp::Equal; prefix];
    const MAX_CELLS: usize = 4_000_000;
    if (b_mid.len() + 1).saturating_mul(a_mid.len() + 1) > MAX_CELLS {
        ops.extend(std::iter::repeat_n(DiffOp::Delete, b_mid.len()));
        ops.extend(std::iter::repeat_n(DiffOp::Insert, a_mid.len()));
    } else {
        let n = b_mid.len();
        let m = a_mid.len();
        let width = m + 1;
        let mut table = vec![0u32; (n + 1) * width];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                table[i * width + j] = if b_mid[i] == a_mid[j] {
                    table[(i + 1) * width + j + 1] + 1
                } else {
                    table[(i + 1) * width + j].max(table[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n && j < m {
            if b_mid[i] == a_mid[j] {
                ops.push(DiffOp::Equal);
                i += 1;
                j += 1;
            } else if table[(i + 1) * width + j] >= table[i * width + j + 1] {
                ops.push(DiffOp::Delete);
                i += 1;
            } else {
                ops.push(DiffOp::Insert);
                j += 1;
            }
        }
        ops.extend(std::iter::repeat_n(DiffOp::Delete, n - i));
        ops.extend(std::iter::repeat_n(DiffOp::Insert, m - j));
    }
    ops.extend(std::iter::repeat_n(DiffOp::Equal, suffix));
    ops
}

/// Render a deterministic unified text diff between two explicit snapshots.
/// Identical inputs render as an empty string. Hunks carry three lines of
/// context and `\ No newline at end of file` markers follow unterminated
/// lines, matching unified-diff convention.
fn unified_diff(before: &str, after: &str) -> String {
    const CONTEXT: usize = 3;
    let b = split_diff_lines(before);
    let a = split_diff_lines(after);
    let ops = diff_ops(&b, &a);

    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter_map(|(idx, op)| (*op != DiffOp::Equal).then_some(idx))
        .collect();
    if changed.is_empty() {
        return String::new();
    }
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    let (mut start, mut end) = (changed[0], changed[0] + 1);
    for &idx in &changed[1..] {
        if idx - end <= 2 * CONTEXT {
            end = idx + 1;
        } else {
            hunks.push((start, end));
            start = idx;
            end = idx + 1;
        }
    }
    hunks.push((start, end));

    let mut out = String::from("--- from\n+++ to\n");
    for (hunk_start, hunk_end) in hunks {
        let render_start = hunk_start.saturating_sub(CONTEXT);
        let render_end = (hunk_end + CONTEXT).min(ops.len());
        let (mut bi, mut ai) = (0usize, 0usize);
        for op in &ops[..render_start] {
            match op {
                DiffOp::Equal => {
                    bi += 1;
                    ai += 1;
                }
                DiffOp::Delete => bi += 1,
                DiffOp::Insert => ai += 1,
            }
        }
        let (hunk_before_start, hunk_after_start) = (bi, ai);
        let (mut before_count, mut after_count) = (0usize, 0usize);
        let mut body = String::new();
        let mut push_line = |marker: char, line: DiffLine<'_>| {
            body.push(marker);
            body.push_str(line.text);
            body.push('\n');
            if !line.newline {
                body.push_str("\\ No newline at end of file\n");
            }
        };
        for op in &ops[render_start..render_end] {
            match op {
                DiffOp::Equal => {
                    push_line(' ', b[bi]);
                    bi += 1;
                    ai += 1;
                    before_count += 1;
                    after_count += 1;
                }
                DiffOp::Delete => {
                    push_line('-', b[bi]);
                    bi += 1;
                    before_count += 1;
                }
                DiffOp::Insert => {
                    push_line('+', a[ai]);
                    ai += 1;
                    after_count += 1;
                }
            }
        }
        let before_start = if before_count == 0 {
            hunk_before_start
        } else {
            hunk_before_start + 1
        };
        let after_start = if after_count == 0 {
            hunk_after_start
        } else {
            hunk_after_start + 1
        };
        out.push_str(&format!(
            "@@ -{before_start},{before_count} +{after_start},{after_count} @@\n{body}"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_diff_renders_line_changes_with_context() {
        let before = "alpha\nbeta\ngamma\n";
        let after = "alpha\nBETA\ngamma\n";
        let diff = unified_diff(before, after);
        assert_eq!(
            diff,
            "--- from\n+++ to\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n"
        );
    }

    #[test]
    fn unified_diff_renders_pure_addition_and_removal() {
        let added = unified_diff("", "one\ntwo\n");
        assert_eq!(added, "--- from\n+++ to\n@@ -0,0 +1,2 @@\n+one\n+two\n");
        let removed = unified_diff("one\ntwo\n", "");
        assert_eq!(removed, "--- from\n+++ to\n@@ -1,2 +0,0 @@\n-one\n-two\n");
    }

    #[test]
    fn unified_diff_marks_missing_trailing_newline() {
        let diff = unified_diff("same\n", "same");
        assert_eq!(
            diff,
            "--- from\n+++ to\n@@ -1,1 +1,1 @@\n-same\n+same\n\\ No newline at end of file\n"
        );
    }

    #[test]
    fn unified_diff_is_empty_for_identical_content() {
        assert_eq!(unified_diff("identical\n", "identical\n"), "");
        assert_eq!(unified_diff("", ""), "");
    }

    #[test]
    fn unified_diff_splits_distant_changes_into_hunks() {
        let before: String = (1..=15).map(|i| format!("line{i}\n")).collect();
        let after = before
            .replacen("line2\n", "LINE2\n", 1)
            .replacen("line13\n", "LINE13\n", 1);
        let diff = unified_diff(&before, &after);
        assert!(diff.contains("@@ -1,5 +1,5 @@\n line1\n-line2\n+LINE2\n line3\n line4\n line5\n"));
        assert!(diff.contains(
            "@@ -10,6 +10,6 @@\n line10\n line11\n line12\n-line13\n+LINE13\n line14\n line15\n"
        ));
    }

    #[test]
    fn unified_diff_merges_close_changes_into_one_hunk() {
        let before: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        let after = before
            .replacen("line2\n", "LINE2\n", 1)
            .replacen("line9\n", "LINE9\n", 1);
        let diff = unified_diff(&before, &after);
        assert!(diff.starts_with("--- from\n+++ to\n@@ -1,10 +1,10 @@\n"));
        // A single hunk header contains exactly two "@@" markers.
        assert_eq!(diff.matches("@@").count(), 2);
    }
}
