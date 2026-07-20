// djinn:allow-oversize
//! Session-scoped stash for full tool outputs that exceed the truncation limit.
//!
//! Before `smart_truncate` discards the middle of a large tool result, the full
//! text is stashed here so the agent can paginate (`output_view`) or search
//! (`output_grep`) it later without re-running the command.
//!
//! Bounded: max 10 entries, max 5 MB total. FIFO eviction when either limit is
//! hit. Each reply-loop instance owns its own stash — no cross-session sharing.
//!
//! rdx6 note: the agent/slot turn-budget externalization seam remains a
//! transcript-text contract. It reuses the existing `tool_use_id` recovery path
//! and does not introduce a new durable blob or pointer format, so the
//! coordinator parser below intentionally stays behaviorally unchanged.
//!
//! Durable read-through (C6): in addition to the in-memory map, every stashed
//! blob is written once to a content-addressed file under the djinn cache dir
//! (keyed by `sha256(content)`), plus a tiny id-pointer so `output_view` /
//! `output_grep` can resolve a `tool_use_id` after the in-memory entry is gone
//! (process restart, eviction, or post-compaction `clear`). The in-memory path
//! is unchanged and always wins; the disk fallback only runs on a miss and
//! degrades gracefully (best-effort writes, clear errors on read failure).

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use djinn_core::clock::{Clock, SystemClock};
use djinn_core::models::SessionStatus;
use sha2::{Digest, Sha256};

/// Maximum number of stashed entries.
const MAX_ENTRIES: usize = 10;
/// Maximum total bytes across all entries.
const MAX_TOTAL_BYTES: usize = 5 * 1024 * 1024; // 5 MB

/// Maximum characters per tool result handed to the model. Results larger than
/// this are stashed in full and replaced inline with a `smart_truncate`d view
/// plus an `output_view`/`output_grep` navigation hint.
///
/// ~30k chars ≈ 7.5k tokens — enough for diagnosis, safe with multiple calls,
/// and well under the provider's per-string limit. Shared by every surface
/// that feeds tool results back into a conversation (the worker reply loop and
/// the chat loop) so the two can never drift — the chat path lacking this clamp
/// is what let a ~12 MB tool result reach the provider and 400 the request.
pub const MAX_TOOL_RESULT_CHARS: usize = 30_000;

/// Current durable id-pointer wire format.
///
/// Legacy pointers are still accepted as `tool_name\tcontent_hash`. New writes
/// use a versioned tab-delimited record so retention GC can classify ownership
/// and age without changing `output_view` / `output_grep` read-through:
///
/// `v1\t<tool_name>\t<content_hash>\t<session_id>\t<created_at_unix_secs>`
const DURABLE_POINTER_VERSION: &str = "v1";
const DURABLE_POINTER_VERSION_V2: &str = "v2";

struct StashedOutput {
    tool_use_id: String,
    tool_name: String,
    full_text: String,
}

/// Caller-supplied facts about the output being persisted. Ownership, content
/// hash, and creation time are established by the stash and its stored bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableOutputDetails {
    pub turn: u64,
    pub result_kind: String,
    pub original_chars: usize,
    pub stored_chars: usize,
    /// One of `complete`, `truncated`, or `partial-spill`.
    pub completeness: String,
}

impl DurableOutputDetails {
    pub fn complete(text: &str) -> Self {
        let chars = text.chars().count();
        Self {
            turn: 0,
            result_kind: "tool_result".into(),
            original_chars: chars,
            stored_chars: chars,
            completeness: "complete".into(),
        }
    }

    fn validate(&self, full_text: &str) -> Result<(), String> {
        if self.result_kind.is_empty()
            || !matches!(
                self.completeness.as_str(),
                "complete" | "truncated" | "partial-spill"
            )
            || self.stored_chars != full_text.chars().count()
            || self.original_chars < self.stored_chars
            || (self.completeness == "complete" && self.original_chars != self.stored_chars)
        {
            return Err("invalid durable output metadata".into());
        }
        Ok(())
    }
}

/// Durable output-stash root used by coordinator maintenance. Exposed as a
/// narrow integration point so GC wiring uses the same cache location as normal
/// durable writes/reads.
pub(crate) fn durable_root_for_gc() -> Option<PathBuf> {
    durable_root()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DurablePointerKind {
    Version2,
    Version1,
    /// Pre-metadata pointer (`tool_name\tcontent_hash`). Owner and timestamp are
    /// intentionally unknown so future GC can classify it safely instead of
    /// treating it as corrupt.
    Legacy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DurablePointerRecord {
    pub(crate) kind: DurablePointerKind,
    pub(crate) tool_name: String,
    pub(crate) content_hash: String,
    pub(crate) session_id: Option<String>,
    pub(crate) created_at_unix_secs: Option<u64>,
    pub(crate) tool_use_id: Option<String>,
    pub(crate) turn: Option<u64>,
    pub(crate) result_kind: Option<String>,
    pub(crate) original_chars: Option<usize>,
    pub(crate) stored_chars: Option<usize>,
    pub(crate) completeness: Option<String>,
}

impl DurablePointerRecord {
    fn new_v1(
        tool_name: &str,
        content_hash: &str,
        session_id: Option<&str>,
        created_at_unix_secs: u64,
    ) -> Self {
        Self {
            kind: DurablePointerKind::Version1,
            tool_name: tool_name.to_string(),
            content_hash: content_hash.to_string(),
            session_id: session_id.map(str::to_string),
            created_at_unix_secs: Some(created_at_unix_secs),
            tool_use_id: None,
            turn: None,
            result_kind: None,
            original_chars: None,
            stored_chars: None,
            completeness: None,
        }
    }

    fn new_v2(
        tool_name: &str,
        content_hash: &str,
        owner: &str,
        id: &str,
        details: &DurableOutputDetails,
    ) -> Self {
        Self {
            kind: DurablePointerKind::Version2,
            tool_name: tool_name.into(),
            content_hash: content_hash.into(),
            session_id: Some(owner.into()),
            created_at_unix_secs: Some(unix_time_secs()),
            tool_use_id: Some(id.into()),
            turn: Some(details.turn),
            result_kind: Some(details.result_kind.clone()),
            original_chars: Some(details.original_chars),
            stored_chars: Some(details.stored_chars),
            completeness: Some(details.completeness.clone()),
        }
    }

    fn serialize(&self) -> String {
        match self.kind {
            DurablePointerKind::Version2 => format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                DURABLE_POINTER_VERSION_V2,
                self.tool_name,
                self.content_hash,
                self.session_id.as_deref().unwrap_or(""),
                self.tool_use_id.as_deref().unwrap_or(""),
                self.turn.unwrap_or(0),
                self.result_kind.as_deref().unwrap_or("tool_result"),
                self.original_chars.unwrap_or(0),
                self.stored_chars.unwrap_or(0),
                self.completeness.as_deref().unwrap_or("complete"),
                self.created_at_unix_secs.unwrap_or(0)
            ),
            DurablePointerKind::Version1 => format!(
                "{}\t{}\t{}\t{}\t{}",
                DURABLE_POINTER_VERSION,
                self.tool_name,
                self.content_hash,
                self.session_id.as_deref().unwrap_or(""),
                self.created_at_unix_secs.unwrap_or(0)
            ),
            DurablePointerKind::Legacy => format!("{}\t{}", self.tool_name, self.content_hash),
        }
    }
}

pub(crate) fn parse_durable_pointer(raw: &str) -> Result<DurablePointerRecord, String> {
    let f: Vec<&str> = raw.trim_end().split('\t').collect();
    match f.as_slice() {
        [
            DURABLE_POINTER_VERSION_V2,
            tool,
            hash,
            owner,
            id,
            turn,
            kind,
            original,
            stored,
            completeness,
            created,
        ] => {
            if tool.is_empty()
                || hash.len() != 64
                || owner.is_empty()
                || id.is_empty()
                || kind.is_empty()
                || !matches!(*completeness, "complete" | "truncated" | "partial-spill")
            {
                return Err("corrupt durable stash pointer".into());
            }
            let parse = |v: &str| {
                v.parse()
                    .map_err(|_| "corrupt durable stash pointer metadata".to_string())
            };
            Ok(DurablePointerRecord {
                kind: DurablePointerKind::Version2,
                tool_name: (*tool).into(),
                content_hash: (*hash).into(),
                session_id: Some((*owner).into()),
                tool_use_id: Some((*id).into()),
                turn: Some(parse(turn)?),
                result_kind: Some((*kind).into()),
                original_chars: Some(
                    original
                        .parse::<usize>()
                        .map_err(|_| "corrupt durable stash pointer metadata".to_string())?,
                ),
                stored_chars: Some(
                    stored
                        .parse::<usize>()
                        .map_err(|_| "corrupt durable stash pointer metadata".to_string())?,
                ),
                completeness: Some((*completeness).into()),
                created_at_unix_secs: Some(parse(created)?),
            })
        }
        ["v1", tool, hash, session, created] if !tool.is_empty() && !hash.is_empty() => {
            Ok(DurablePointerRecord::new_v1(
                tool,
                hash,
                (!session.is_empty()).then_some(*session),
                created
                    .parse()
                    .map_err(|_| "corrupt durable stash pointer timestamp".to_string())?,
            ))
        }
        [tool, hash]
            if *tool != DURABLE_POINTER_VERSION && !tool.is_empty() && !hash.is_empty() =>
        {
            Ok(DurablePointerRecord {
                kind: DurablePointerKind::Legacy,
                tool_name: (*tool).into(),
                content_hash: (*hash).into(),
                session_id: None,
                created_at_unix_secs: None,
                tool_use_id: None,
                turn: None,
                result_kind: None,
                original_chars: None,
                stored_chars: None,
                completeness: None,
            })
        }
        _ => Err("corrupt durable stash pointer".into()),
    }
}

fn unix_time_secs() -> u64 {
    SystemClock::new()
        .now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Lowercase hex sha256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Test-only override for the durable root, so the whole test binary writes to
/// an isolated tempdir instead of the real `$HOME/.cache`. Initialized lazily on
/// first use to a unique per-run directory so no test ever touches the user's
/// real cache, and so durable state is shared across a single binary's tests.
#[cfg(test)]
static DURABLE_ROOT_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

#[cfg(test)]
fn test_durable_root() -> PathBuf {
    DURABLE_ROOT_OVERRIDE
        .get_or_init(|| crate::test_helpers::test_persistent_dir("djinn-output-stash-"))
        .clone()
}

/// Root directory for the durable stash, e.g. `$XDG_CACHE_HOME/djinn/output_stash`
/// (or `$HOME/.cache/djinn/output_stash`). `None` when neither env var is set,
/// in which case durability is silently disabled (in-memory still works).
///
/// Mirrors [`crate::sandbox::djinn_cache_dir`] — the sandbox backends already
/// permit writes beneath the djinn cache dir, so blobs land in an allowed path.
#[cfg(test)]
fn durable_root() -> Option<PathBuf> {
    Some(test_durable_root())
}

#[cfg(not(test))]
fn durable_root() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("djinn").join("output_stash"));
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| {
            PathBuf::from(h)
                .join(".cache")
                .join("djinn")
                .join("output_stash")
        })
}

/// Content-addressed blob path: `<root>/blobs/<sha256(content)>`.
fn blob_path(root: &std::path::Path, content_hash: &str) -> PathBuf {
    root.join("blobs").join(content_hash)
}

/// Id-pointer path: `<root>/ids/<sha256(tool_use_id)>`. The pointer file holds a
/// versioned metadata record (or, for old files, legacy `tool_name\tcontent_hash`)
/// so a bare `tool_use_id` resolves to its blob + tool name after the in-memory
/// entry is gone.
fn id_pointer_path(root: &std::path::Path, tool_use_id: &str) -> PathBuf {
    root.join("ids").join(sha256_hex(tool_use_id.as_bytes()))
}

/// V2 identity is the trusted owner plus the tool-use ID. Hash a length-delimited
/// representation so distinct `(owner, tool_use_id)` pairs cannot collide merely
/// by concatenating their strings.
fn owner_id_pointer_path(root: &std::path::Path, owner: &str, tool_use_id: &str) -> PathBuf {
    let identity = format!("{}:{owner}:{tool_use_id}", owner.len());
    root.join("ids").join(sha256_hex(identity.as_bytes()))
}

/// Session state needed by durable output-stash GC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputStashGcSession {
    pub status: SessionStatus,
    /// Terminal timestamp as Unix seconds. `None` is treated conservatively.
    pub ended_at_unix_secs: Option<u64>,
}

/// Best-effort GC statistics suitable for maintenance logs/metrics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutputStashGcReport {
    pub pointers_scanned: usize,
    pub pointers_deleted: usize,
    pub pointers_retained: usize,
    pub blobs_scanned: usize,
    pub blobs_deleted: usize,
    pub blobs_retained: usize,
    pub errors: Vec<String>,
}

impl OutputStashGcReport {
    pub fn is_success(&self) -> bool {
        self.errors.is_empty()
    }
}

fn is_terminal_session_status(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Completed | SessionStatus::Interrupted | SessionStatus::Failed
    )
}

fn file_modified_unix_secs(path: &Path) -> Result<u64, String> {
    let modified = std::fs::metadata(path)
        .map_err(|e| format!("metadata {}: {e}", path.display()))?
        .modified()
        .map_err(|e| format!("modified time {}: {e}", path.display()))?;
    Ok(modified
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0))
}

/// Garbage-collect durable output-stash id-pointers and content-addressed blobs.
///
/// `retention_cutoff_unix_secs` is the precomputed cutoff timestamp: terminal
/// session pointers are removed when their `ended_at` is at or before this
/// value, and unreferenced blobs are removed when their file mtime is at or
/// before it. The supplied lookup keeps this engine reusable and easy to unit
/// test; coordinator wiring can adapt it to the session repository later.
pub fn gc_durable_output_stash<F>(
    root: &Path,
    retention_cutoff_unix_secs: u64,
    mut lookup_session: F,
) -> OutputStashGcReport
where
    F: FnMut(&str) -> Result<Option<OutputStashGcSession>, String>,
{
    let mut report = OutputStashGcReport::default();
    let ids_dir = root.join("ids");
    let blobs_dir = root.join("blobs");

    let mut retained_hashes: HashSet<String> = HashSet::new();

    scan_pointers(
        root,
        &ids_dir,
        &mut report,
        &mut retained_hashes,
        &mut lookup_session,
        retention_cutoff_unix_secs,
    );
    scan_blobs(
        &blobs_dir,
        &mut report,
        &retained_hashes,
        retention_cutoff_unix_secs,
    );

    report
}

fn scan_pointers<F>(
    root: &Path,
    ids_dir: &Path,
    report: &mut OutputStashGcReport,
    retained_hashes: &mut HashSet<String>,
    lookup_session: &mut F,
    retention_cutoff_unix_secs: u64,
) where
    F: FnMut(&str) -> Result<Option<OutputStashGcSession>, String>,
{
    match std::fs::read_dir(ids_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        report.errors.push(format!("read pointer entry: {e}"));
                        continue;
                    }
                };
                process_pointer_entry(
                    root,
                    entry,
                    report,
                    retained_hashes,
                    lookup_session,
                    retention_cutoff_unix_secs,
                );
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => report
            .errors
            .push(format!("read ids dir {}: {e}", ids_dir.display())),
    }
}

fn process_pointer_entry<F>(
    root: &Path,
    entry: std::fs::DirEntry,
    report: &mut OutputStashGcReport,
    retained_hashes: &mut HashSet<String>,
    lookup_session: &mut F,
    retention_cutoff_unix_secs: u64,
) where
    F: FnMut(&str) -> Result<Option<OutputStashGcSession>, String>,
{
    let pointer_path = entry.path();
    if !pointer_path.is_file() {
        return;
    }
    report.pointers_scanned += 1;

    let raw = match std::fs::read_to_string(&pointer_path) {
        Ok(raw) => raw,
        Err(e) => {
            report
                .errors
                .push(format!("read pointer {}: {e}", pointer_path.display()));
            report.pointers_retained += 1;
            return;
        }
    };
    let record = match parse_durable_pointer(&raw) {
        Ok(record) => record,
        Err(e) => {
            report
                .errors
                .push(format!("parse pointer {}: {e}", pointer_path.display()));
            report.pointers_retained += 1;
            return;
        }
    };

    if !blob_path(root, &record.content_hash).is_file() {
        match std::fs::remove_file(&pointer_path) {
            Ok(()) => report.pointers_deleted += 1,
            Err(e) => {
                report.errors.push(format!(
                    "delete missing-blob pointer {}: {e}",
                    pointer_path.display()
                ));
                report.pointers_retained += 1;
            }
        }
        return;
    }

    let should_delete =
        should_delete_pointer(&record, retention_cutoff_unix_secs, lookup_session, report);

    if should_delete {
        match std::fs::remove_file(&pointer_path) {
            Ok(()) => report.pointers_deleted += 1,
            Err(e) => {
                report.errors.push(format!(
                    "delete expired pointer {}: {e}",
                    pointer_path.display()
                ));
                report.pointers_retained += 1;
                retained_hashes.insert(record.content_hash);
            }
        }
    } else {
        report.pointers_retained += 1;
        retained_hashes.insert(record.content_hash);
    }
}

fn should_delete_pointer<F>(
    record: &DurablePointerRecord,
    retention_cutoff_unix_secs: u64,
    lookup_session: &mut F,
    report: &mut OutputStashGcReport,
) -> bool
where
    F: FnMut(&str) -> Result<Option<OutputStashGcSession>, String>,
{
    if record.kind == DurablePointerKind::Legacy {
        return false;
    }
    let Some(session_id) = record.session_id.as_deref() else {
        return false;
    };
    match lookup_session(session_id) {
        Ok(Some(session)) if is_terminal_session_status(session.status) => session
            .ended_at_unix_secs
            .is_some_and(|ended_at| ended_at <= retention_cutoff_unix_secs),
        Ok(Some(_live_or_paused)) => false,
        Ok(None) => false,
        Err(e) => {
            report
                .errors
                .push(format!("lookup session {session_id}: {e}"));
            false
        }
    }
}

fn scan_blobs(
    blobs_dir: &Path,
    report: &mut OutputStashGcReport,
    retained_hashes: &HashSet<String>,
    retention_cutoff_unix_secs: u64,
) {
    match std::fs::read_dir(blobs_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        report.errors.push(format!("read blob entry: {e}"));
                        continue;
                    }
                };
                process_blob_entry(entry, report, retained_hashes, retention_cutoff_unix_secs);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => report
            .errors
            .push(format!("read blobs dir {}: {e}", blobs_dir.display())),
    }
}

fn process_blob_entry(
    entry: std::fs::DirEntry,
    report: &mut OutputStashGcReport,
    retained_hashes: &HashSet<String>,
    retention_cutoff_unix_secs: u64,
) {
    let blob = entry.path();
    if !blob.is_file() {
        return;
    }
    report.blobs_scanned += 1;
    let Some(name) = blob.file_name().and_then(|n| n.to_str()) else {
        report.blobs_retained += 1;
        return;
    };
    if retained_hashes.contains(name) {
        report.blobs_retained += 1;
        return;
    }
    match file_modified_unix_secs(&blob) {
        Ok(modified_at) if modified_at <= retention_cutoff_unix_secs => {
            match std::fs::remove_file(&blob) {
                Ok(()) => report.blobs_deleted += 1,
                Err(e) => {
                    report
                        .errors
                        .push(format!("delete blob {}: {e}", blob.display()));
                    report.blobs_retained += 1;
                }
            }
        }
        Ok(_) => report.blobs_retained += 1,
        Err(e) => {
            report.errors.push(e);
            report.blobs_retained += 1;
        }
    }
}

/// Persist a stashed blob durably. Best-effort: any IO error is swallowed so a
/// disk problem never breaks the in-memory fast path. Writes are atomic
/// (temp-file + rename) to avoid torn reads.
fn durable_write(
    tool_use_id: &str,
    tool_name: &str,
    owner: &str,
    full_text: &str,
    details: &DurableOutputDetails,
) -> Result<(), String> {
    let root = durable_root().ok_or("durable stash unavailable (no cache dir)")?;
    durable_write_at(&root, tool_use_id, tool_name, owner, full_text, details)
}

fn durable_write_at(
    root: &Path,
    tool_use_id: &str,
    tool_name: &str,
    owner: &str,
    full_text: &str,
    details: &DurableOutputDetails,
) -> Result<(), String> {
    details.validate(full_text)?;
    let hash = sha256_hex(full_text.as_bytes());
    let blobs = root.join("blobs");
    let ids = root.join("ids");
    std::fs::create_dir_all(&blobs).map_err(|e| format!("create durable blobs: {e}"))?;
    std::fs::create_dir_all(&ids).map_err(|e| format!("create durable ids: {e}"))?;
    let pointer = owner_id_pointer_path(root, owner, tool_use_id);
    if pointer.exists() {
        let existing = parse_durable_pointer(
            &std::fs::read_to_string(&pointer).map_err(|e| format!("read durable pointer: {e}"))?,
        )?;
        let identical = existing.kind == DurablePointerKind::Version2
            && existing.session_id.as_deref() == Some(owner)
            && existing.tool_use_id.as_deref() == Some(tool_use_id)
            && existing.content_hash == hash
            && existing.tool_name == tool_name
            && existing.turn == Some(details.turn)
            && existing.result_kind.as_deref() == Some(details.result_kind.as_str())
            && existing.original_chars == Some(details.original_chars)
            && existing.stored_chars == Some(details.stored_chars)
            && existing.completeness.as_deref() == Some(details.completeness.as_str());
        if identical {
            let blob = std::fs::read(blob_path(root, &hash))
                .map_err(|_| "existing durable output blob missing or unreadable".to_string())?;
            if sha256_hex(&blob) == hash {
                return Ok(());
            }
            return Err("existing durable output blob hash mismatch".into());
        }
        return Err("conflicting durable output for owner session and tool_use_id".into());
    }
    let blob = blob_path(root, &hash);
    if !blob.exists() {
        atomic_write(&blobs, &blob, full_text.as_bytes())
            .map_err(|e| format!("write durable blob: {e}"))?;
    } else if sha256_hex(&std::fs::read(&blob).map_err(|e| format!("read durable blob: {e}"))?)
        != hash
    {
        return Err("existing durable blob hash mismatch".into());
    }
    let record = DurablePointerRecord::new_v2(tool_name, &hash, owner, tool_use_id, details);
    atomic_write(&ids, &pointer, record.serialize().as_bytes())
        .map_err(|e| format!("write durable pointer: {e}"))
}

/// Write `bytes` to `dest` atomically via a uniquely-named temp file in the same
/// `dir` followed by a rename (atomic on the same filesystem). Avoids torn reads
/// from a concurrent reader and only needs `std::fs` — no extra runtime dep.
fn atomic_write(
    dir: &std::path::Path,
    dest: &std::path::Path,
    bytes: &[u8],
) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemClock::new()
        .now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".tmp-{}-{nanos}-{seq}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
    }
    match std::fs::rename(&tmp, dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Resolve a `tool_use_id` from the durable store. Returns `(tool_name, full_text)`
/// on success. Errors (no root, missing/corrupt pointer or blob) propagate as a
/// human-readable message — never a panic.
#[cfg(test)]
fn durable_read(tool_use_id: &str) -> Result<(String, String), String> {
    let root = durable_root().ok_or("durable stash unavailable (no cache dir)")?;
    durable_read_at(&root, tool_use_id, None)
}

fn durable_read_at(
    root: &Path,
    tool_use_id: &str,
    owner: Option<&str>,
) -> Result<(String, String), String> {
    // Prefer the owner-qualified v2 address. A matching historic v1 record can
    // be read from its old address, but only when its recorded owner is the
    // trusted owner; unknown-owner legacy data never crosses that boundary.
    let owner_pointer = owner.map(|owner| owner_id_pointer_path(root, owner, tool_use_id));
    let pointer = match owner_pointer.as_ref() {
        Some(pointer) if pointer.is_file() => pointer.clone(),
        Some(_) => id_pointer_path(root, tool_use_id),
        None => id_pointer_path(root, tool_use_id),
    };
    let raw = std::fs::read_to_string(&pointer)
        .map_err(|_| format!("no durable stash for tool_use_id \"{tool_use_id}\""))?;
    let record = parse_durable_pointer(&raw)?;
    match owner {
        Some(owner)
            if record.kind == DurablePointerKind::Version2
                && owner_pointer.as_ref() == Some(&pointer)
                && record.session_id.as_deref() == Some(owner)
                && record.tool_use_id.as_deref() == Some(tool_use_id) => {}
        Some(owner)
            if record.kind == DurablePointerKind::Version1
                && record.session_id.as_deref() == Some(owner) => {}
        Some(_) => return Err("durable output belongs to another session".into()),
        // Unowned stashes retain only the historic unknown-owner compatibility
        // path; they must not become a way to cross a trusted v2 boundary.
        None if record.kind == DurablePointerKind::Version2 || record.session_id.is_some() => {
            return Err("durable output belongs to another session".into());
        }
        None => {}
    }
    let blob = blob_path(root, &record.content_hash);
    let bytes =
        std::fs::read(&blob).map_err(|_| "durable stash blob missing or unreadable".to_string())?;
    if sha256_hex(&bytes) != record.content_hash {
        return Err("durable stash blob hash mismatch".into());
    }
    let full_text = String::from_utf8(bytes)
        .map_err(|_| "durable stash blob missing or unreadable".to_string())?;
    Ok((record.tool_name, full_text))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableOutputMetadata {
    pub owner_session_id: String,
    pub tool_use_id: String,
    pub turn: u64,
    pub result_kind: String,
    pub original_chars: usize,
    pub stored_chars: usize,
    pub completeness: String,
    pub content_hash: String,
    pub tool_name: String,
    pub created_at_unix_secs: u64,
}

fn metadata_from_record(record: &DurablePointerRecord) -> Option<DurableOutputMetadata> {
    if record.kind != DurablePointerKind::Version2 {
        return None;
    }
    Some(DurableOutputMetadata {
        owner_session_id: record.session_id.clone()?,
        tool_use_id: record.tool_use_id.clone()?,
        turn: record.turn?,
        result_kind: record.result_kind.clone()?,
        original_chars: record.original_chars?,
        stored_chars: record.stored_chars?,
        completeness: record.completeness.clone()?,
        content_hash: record.content_hash.clone(),
        tool_name: record.tool_name.clone(),
        created_at_unix_secs: record.created_at_unix_secs?,
    })
}

fn list_durable_at(root: &Path, owner: &str) -> Result<Vec<DurableOutputMetadata>, String> {
    let ids = root.join("ids");
    let entries = match std::fs::read_dir(&ids) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read durable ids: {e}")),
    };
    let mut listed = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("read durable id entry: {e}"))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(record) = parse_durable_pointer(&raw) else {
            continue;
        };
        let Some(metadata) = metadata_from_record(&record) else {
            continue;
        };
        if metadata.owner_session_id != owner
            || path != owner_id_pointer_path(root, owner, &metadata.tool_use_id)
            || metadata.stored_chars > metadata.original_chars
            || (metadata.completeness == "complete"
                && metadata.stored_chars != metadata.original_chars)
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(blob_path(root, &metadata.content_hash)) else {
            continue;
        };
        if sha256_hex(&bytes) != metadata.content_hash
            || std::str::from_utf8(&bytes)
                .map_or(true, |text| text.chars().count() != metadata.stored_chars)
        {
            continue;
        }
        listed.push(metadata);
    }
    listed.sort_by(|a, b| {
        a.created_at_unix_secs
            .cmp(&b.created_at_unix_secs)
            .then_with(|| a.tool_use_id.cmp(&b.tool_use_id))
    });
    Ok(listed)
}

pub struct OutputStash {
    entries: VecDeque<StashedOutput>,
    total_bytes: usize,
    owner_session_id: Option<String>,
    #[cfg(test)]
    durable_root_override: Option<PathBuf>,
}

impl OutputStash {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            total_bytes: 0,
            owner_session_id: None,
            #[cfg(test)]
            durable_root_override: None,
        }
    }

    pub fn with_session_id(session_id: impl Into<String>) -> Self {
        Self {
            entries: VecDeque::new(),
            total_bytes: 0,
            owner_session_id: Some(session_id.into()),
            #[cfg(test)]
            durable_root_override: None,
        }
    }

    #[cfg(test)]
    fn with_session_id_and_durable_root(
        session_id: impl Into<String>,
        durable_root: PathBuf,
    ) -> Self {
        Self {
            entries: VecDeque::new(),
            total_bytes: 0,
            owner_session_id: Some(session_id.into()),
            durable_root_override: Some(durable_root),
        }
    }

    /// Stash full tool output with the compatibility metadata used by existing
    /// callers. New durable writers should use [`Self::insert_with_metadata`].
    pub fn insert(
        &mut self,
        tool_use_id: String,
        tool_name: String,
        full_text: String,
    ) -> Result<(), String> {
        let details = DurableOutputDetails::complete(&full_text);
        self.insert_with_metadata(tool_use_id, tool_name, full_text, details)
    }

    /// Stash output and durably persist the supplied result facts. The durable
    /// write happens before mutating the FIFO so a failed write cannot cause a
    /// caller to replace its inline result with an unusable durable pointer.
    pub fn insert_with_metadata(
        &mut self,
        tool_use_id: String,
        tool_name: String,
        full_text: String,
        details: DurableOutputDetails,
    ) -> Result<(), String> {
        if let Some(owner) = self.owner_session_id.as_deref() {
            #[cfg(test)]
            if let Some(root) = self.durable_root_override.as_deref() {
                durable_write_at(root, &tool_use_id, &tool_name, owner, &full_text, &details)?;
            } else {
                durable_write(&tool_use_id, &tool_name, owner, &full_text, &details)?;
            }
            #[cfg(not(test))]
            durable_write(&tool_use_id, &tool_name, owner, &full_text, &details)?;
        }

        if self.entries.iter().any(|entry| {
            entry.tool_use_id == tool_use_id
                && entry.tool_name == tool_name
                && entry.full_text == full_text
        }) {
            return Ok(());
        }
        let new_bytes = full_text.len();

        // Evict until we have room for the new entry (both count and bytes).
        while self.entries.len() >= MAX_ENTRIES
            || (self.total_bytes + new_bytes > MAX_TOTAL_BYTES && !self.entries.is_empty())
        {
            if let Some(evicted) = self.entries.pop_front() {
                self.total_bytes -= evicted.full_text.len();
            }
        }

        self.total_bytes += new_bytes;
        self.entries.push_back(StashedOutput {
            tool_use_id,
            tool_name,
            full_text,
        });
        Ok(())
    }

    /// Authoritatively list valid durable records for this stash's trusted
    /// session. Historic, foreign, corrupt, and missing-blob pointers are never
    /// returned. Retention expiry is represented by GC removing its pointer.
    pub fn list_durable_outputs(&self) -> Result<Vec<DurableOutputMetadata>, String> {
        let owner = self
            .owner_session_id
            .as_deref()
            .ok_or("durable output listing requires a trusted session")?;
        #[cfg(test)]
        if let Some(root) = self.durable_root_override.as_deref() {
            return list_durable_at(root, owner);
        }
        let root = durable_root().ok_or("durable stash unavailable (no cache dir)")?;
        list_durable_at(&root, owner)
    }

    /// Resolve the `(tool_name, full_text)` for a `tool_use_id`, preferring the
    /// in-memory entry and falling back to the durable on-disk store when the
    /// in-memory map has been evicted / cleared / lost to a restart.
    fn resolve(&self, tool_use_id: &str) -> Result<(String, String), String> {
        if let Some(entry) = self.entries.iter().find(|e| e.tool_use_id == tool_use_id) {
            return Ok((entry.tool_name.clone(), entry.full_text.clone()));
        }
        // In-memory miss: try the durable store, but surface the familiar
        // not-found message if disk has nothing either.
        #[cfg(test)]
        let durable = if let Some(root) = self.durable_root_override.as_deref() {
            durable_read_at(root, tool_use_id, self.owner_session_id.as_deref())
        } else {
            durable_read_at(
                &durable_root().ok_or("durable stash unavailable")?,
                tool_use_id,
                self.owner_session_id.as_deref(),
            )
        };
        #[cfg(not(test))]
        let durable = durable_read_at(
            &durable_root().ok_or("durable stash unavailable")?,
            tool_use_id,
            self.owner_session_id.as_deref(),
        );

        durable.map_err(|_| {
            format!(
                "No stashed output for tool_use_id \"{tool_use_id}\". \
                 Stashed outputs are cleared after context compaction and \
                 only exist for results that were truncated."
            )
        })
    }

    /// Paginated line view of a stashed output.
    pub fn view(&self, tool_use_id: &str, offset: usize, limit: usize) -> Result<String, String> {
        let (_tool_name, full_text) = self.resolve(tool_use_id)?;
        let lines: Vec<&str> = full_text.lines().collect();
        let total_lines = lines.len();

        if offset >= total_lines {
            return Ok(format!(
                "[offset {offset} is past end of output ({total_lines} lines)]"
            ));
        }

        let end = (offset + limit).min(total_lines);
        let mut result = String::new();

        // Line-number width for alignment.
        let width = end.to_string().len();
        for (i, line) in lines[offset..end].iter().enumerate() {
            let line_num = offset + i + 1; // 1-based
            result.push_str(&format!("{line_num:>width$}  {line}\n"));
        }

        // Navigation hint.
        if end < total_lines {
            result.push_str(&format!(
                "\n[Showing lines {}-{} of {total_lines}. Use output_view(tool_use_id=\"{}\", offset={end}) to see more.]",
                offset + 1,
                end,
                tool_use_id,
            ));
        } else {
            result.push_str(&format!("\n[End of output ({total_lines} lines)]"));
        }

        Ok(result)
    }

    /// Regex search within a stashed output, returning matching lines with context.
    pub fn grep(
        &self,
        tool_use_id: &str,
        pattern: &str,
        context_lines: usize,
    ) -> Result<String, String> {
        let (tool_name, full_text) = self.resolve(tool_use_id)?;
        let re = regex::Regex::new(pattern).map_err(|e| format!("invalid regex: {e}"))?;

        let lines: Vec<&str> = full_text.lines().collect();
        let total_lines = lines.len();

        // Collect matching line indices.
        let matches: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| re.is_match(line))
            .map(|(i, _)| i)
            .collect();

        if matches.is_empty() {
            return Ok(format!(
                "[No matches for pattern \"{pattern}\" in output from {tool_name} ({total_lines} lines)]"
            ));
        }

        // Build context ranges (merge overlapping).
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for &m in &matches {
            let start = m.saturating_sub(context_lines);
            let end = (m + context_lines + 1).min(total_lines);
            if let Some(last) = ranges.last_mut()
                && start <= last.1
            {
                last.1 = end;
                continue;
            }
            ranges.push((start, end));
        }

        // Cap output at 30KB to avoid recursive truncation.
        const MAX_GREP_BYTES: usize = 30_000;
        let mut result = String::new();
        let width = total_lines.to_string().len();
        let mut capped = false;

        for (ri, &(start, end)) in ranges.iter().enumerate() {
            if ri > 0 {
                result.push_str("  ...\n");
            }
            for (i, line) in lines.iter().enumerate().take(end).skip(start) {
                let marker = if matches.contains(&i) { ">" } else { " " };
                let formatted = format!("{}{:>width$}  {}\n", marker, i + 1, line);
                if result.len() + formatted.len() > MAX_GREP_BYTES {
                    capped = true;
                    break;
                }
                result.push_str(&formatted);
            }
            if capped {
                break;
            }
        }

        let match_count = matches.len();
        if capped {
            result.push_str(&format!(
                "\n[Output capped at 30KB. {match_count} total matches for \"{pattern}\". \
                 Use output_view to paginate the full output.]"
            ));
        } else {
            result.push_str(&format!(
                "\n[{match_count} match{} for \"{pattern}\" in {total_lines} lines]",
                if match_count == 1 { "" } else { "es" }
            ));
        }

        Ok(result)
    }

    /// Clear all stashed outputs (called after compaction).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.total_bytes = 0;
    }

    /// In-memory-only lookup. Used by tests to assert the *in-memory* map state
    /// (eviction / clear). Production reads go through [`Self::resolve`], which
    /// additionally falls back to the durable on-disk store.
    #[cfg(test)]
    fn find(&self, tool_use_id: &str) -> Result<&StashedOutput, String> {
        self.entries
            .iter()
            .find(|e| e.tool_use_id == tool_use_id)
            .ok_or_else(|| format!("No stashed output for tool_use_id \"{tool_use_id}\""))
    }
}

impl Default for OutputStash {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the browsable text to stash for a tool result.
///
/// For `shell` the model wants to page through raw stdout/stderr — not the
/// `{"ok":true,"stdout":"…"}` JSON envelope — so we splice those fields into a
/// log-like view. For every other tool we return `None` and the caller stashes
/// the pretty-printed JSON as-is.
pub fn extract_stash_content(tool_name: &str, value: &serde_json::Value) -> Option<String> {
    if tool_name != "shell" {
        return None;
    }
    let obj = value.as_object()?;
    let stdout = obj.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
    let stderr = obj.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
    let exit_code = obj.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(-1);

    let mut out = String::with_capacity(stdout.len() + stderr.len() + 64);
    if !stdout.is_empty() {
        out.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("--- stderr ---\n");
        out.push_str(stderr);
    }
    if exit_code != 0 {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("[exit code: {exit_code}]"));
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

/// Render a successful tool result to the text handed back to the model.
///
/// Serializes `value` to text (raw string, or pretty JSON), and when that
/// exceeds [`MAX_TOOL_RESULT_CHARS`] stashes the full output under
/// `tool_use_id` and returns a `smart_truncate`d view with an
/// `output_view`/`output_grep` navigation hint appended.
///
/// This is the single chokepoint both the worker reply loop and the chat loop
/// route successful tool results through, so neither can ever ship an
/// unbounded result into a conversation again.
pub fn render_tool_result(
    stash: &Mutex<OutputStash>,
    tool_use_id: &str,
    tool_name: &str,
    value: &serde_json::Value,
) -> String {
    let mut text = if value.is_string() {
        value.as_str().unwrap_or("").to_string()
    } else {
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    };
    if text.len() > MAX_TOOL_RESULT_CHARS {
        let stash_text = extract_stash_content(tool_name, value).unwrap_or_else(|| text.clone());
        if stash
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(tool_use_id.to_string(), tool_name.to_string(), stash_text)
            .is_err()
        {
            return text;
        }
        let full_bytes = text.len();
        text = crate::truncate::smart_truncate(&text, MAX_TOOL_RESULT_CHARS);
        text.push_str(&format!(
            "\n\n[Full output stashed ({full_bytes} bytes). Use output_view(tool_use_id=\"{tool_use_id}\") to paginate or output_grep(tool_use_id=\"{tool_use_id}\", pattern=\"...\") to search.]"
        ));
    }
    text
}

/// `true` for the two stash-navigation tools handled in-process against the
/// [`OutputStash`] rather than dispatched to a real handler.
pub fn is_stash_tool(name: &str) -> bool {
    name == "output_view" || name == "output_grep"
}

/// Service an `output_view` / `output_grep` call against the stash.
///
/// Returns `Ok(text)` for a successful view/grep (including the "no match" and
/// "offset past end" informational cases) or `Err(message)` for an unknown
/// `tool_use_id`, an invalid regex, or a name that isn't a stash tool.
pub fn handle_stash_tool(
    stash: &Mutex<OutputStash>,
    name: &str,
    args: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Result<String, String> {
    let guard = stash
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let tid = args
        .and_then(|m| m.get("tool_use_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match name {
        "output_view" => {
            let offset = args
                .and_then(|m| m.get("offset"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let limit = args
                .and_then(|m| m.get("limit"))
                .and_then(|v| v.as_u64())
                .unwrap_or(200) as usize;
            guard.view(tid, offset, limit)
        }
        "output_grep" => {
            let pattern = args
                .and_then(|m| m.get("pattern"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ctx_lines = args
                .and_then(|m| m.get("context_lines"))
                .and_then(|v| v.as_u64())
                .unwrap_or(3) as usize;
            guard.grep(tid, pattern, ctx_lines)
        }
        other => Err(format!("not a stash tool: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Force-initialize the test-binary-wide durable root (an isolated, persistent
    /// tempdir) before a durable-path assertion. In test builds `durable_root`
    /// always resolves here, so the real `$HOME/.cache` is never touched; this is
    /// just an explicit marker that the test depends on durable state.
    fn isolated_durable_root() {
        let _ = durable_root();
    }

    #[test]
    fn insert_and_view_round_trip() {
        let mut stash = OutputStash::new();
        stash
            .insert(
                "t1".into(),
                "shell".into(),
                "line one\nline two\nline three\n".into(),
            )
            .unwrap();
        let result = stash.view("t1", 0, 200).unwrap();
        assert!(result.contains("line one"));
        assert!(result.contains("line three"));
        assert!(result.contains("End of output"));
    }

    #[test]
    fn pagination() {
        let mut stash = OutputStash::new();
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        stash.insert("t1".into(), "shell".into(), text).unwrap();

        let page1 = stash.view("t1", 0, 10).unwrap();
        assert!(page1.contains("line 0"));
        assert!(page1.contains("line 9"));
        assert!(!page1.contains("line 10"));
        assert!(page1.contains("offset=10"));

        let page2 = stash.view("t1", 10, 10).unwrap();
        assert!(page2.contains("line 10"));
        assert!(page2.contains("line 19"));
    }

    #[test]
    fn view_offset_past_end() {
        let mut stash = OutputStash::new();
        stash
            .insert("t1".into(), "shell".into(), "one\ntwo\n".into())
            .unwrap();
        let result = stash.view("t1", 999, 10).unwrap();
        assert!(result.contains("past end"));
    }

    #[test]
    fn grep_with_context() {
        let mut stash = OutputStash::new();
        let text = "aaa\nbbb\nccc\nERROR: bad\nddd\neee\nfff\n";
        stash
            .insert("t1".into(), "shell".into(), text.into())
            .unwrap();

        let result = stash.grep("t1", "ERROR", 1).unwrap();
        assert!(result.contains(">"));
        assert!(result.contains("ERROR: bad"));
        assert!(result.contains("ccc")); // context before
        assert!(result.contains("ddd")); // context after
        assert!(result.contains("1 match"));
    }

    #[test]
    fn grep_no_matches() {
        let mut stash = OutputStash::new();
        stash
            .insert("t1".into(), "shell".into(), "hello\nworld\n".into())
            .unwrap();
        let result = stash.grep("t1", "NONEXISTENT", 2).unwrap();
        assert!(result.contains("No matches"));
    }

    #[test]
    fn grep_invalid_regex() {
        let mut stash = OutputStash::new();
        stash
            .insert("t1".into(), "shell".into(), "hello\n".into())
            .unwrap();
        let result = stash.grep("t1", "[invalid", 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid regex"));
    }

    #[test]
    fn eviction_by_count() {
        let mut stash = OutputStash::new();
        for i in 0..12 {
            stash
                .insert(format!("t{i}"), "shell".into(), format!("output {i}"))
                .unwrap();
        }
        // Oldest should be evicted; only last 10 remain.
        assert!(stash.find("t0").is_err());
        assert!(stash.find("t1").is_err());
        assert!(stash.find("t2").is_ok());
        assert!(stash.find("t11").is_ok());
        assert_eq!(stash.entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn eviction_by_bytes() {
        let mut stash = OutputStash::new();
        // Each entry is ~1MB. After 5, inserting a 6th should evict.
        let big = "x".repeat(1_024 * 1_024);
        for i in 0..6 {
            stash
                .insert(format!("t{i}"), "shell".into(), big.clone())
                .unwrap();
        }
        assert!(stash.total_bytes <= MAX_TOTAL_BYTES);
        // At least the first one should be evicted.
        assert!(stash.find("t0").is_err());
        assert!(stash.find("t5").is_ok());
    }

    #[test]
    fn clear_empties_everything() {
        let mut stash = OutputStash::new();
        stash
            .insert("t1".into(), "shell".into(), "data".into())
            .unwrap();
        stash.clear();
        assert!(stash.find("t1").is_err());
        assert_eq!(stash.total_bytes, 0);
        assert!(stash.entries.is_empty());
    }

    #[test]
    fn unknown_id_error() {
        let stash = OutputStash::new();
        assert!(stash.view("nonexistent", 0, 10).is_err());
        assert!(stash.grep("nonexistent", "foo", 0).is_err());
    }

    #[test]
    fn grep_output_capping() {
        let mut stash = OutputStash::new();
        // Create output where every line matches — should cap at 30KB.
        let text: String = (0..10_000).map(|i| format!("MATCH line {i}\n")).collect();
        stash.insert("t1".into(), "shell".into(), text).unwrap();

        let result = stash.grep("t1", "MATCH", 0).unwrap();
        assert!(result.len() <= 31_000); // small slack for footer
        assert!(result.contains("capped at 30KB"));
    }

    #[test]
    fn render_small_result_is_passthrough() {
        isolated_durable_root();
        let stash = Mutex::new(OutputStash::new());
        let value = serde_json::json!({"ok": true, "rows": 3});
        // Unique id so neither the in-memory map nor the durable store has it.
        let text = render_tool_result(&stash, "small-passthrough-1", "task_list", &value);
        // Pretty JSON, untruncated, nothing stashed (no in-memory, no durable).
        assert!(text.contains("\"rows\""));
        assert!(!text.contains("Full output stashed"));
        assert!(
            stash
                .lock()
                .unwrap()
                .view("small-passthrough-1", 0, 10)
                .is_err()
        );
    }

    #[test]
    fn render_oversized_result_truncates_and_stashes() {
        let stash = Mutex::new(OutputStash::new());
        // A string value well over the clamp.
        let big = "x".repeat(MAX_TOOL_RESULT_CHARS * 2);
        let value = serde_json::Value::String(big.clone());
        let text = render_tool_result(&stash, "call-1", "shell", &value);

        // The inline text is clamped and carries the navigation hint…
        assert!(text.len() < big.len());
        assert!(text.contains("Full output stashed"));
        assert!(text.contains("output_view(tool_use_id=\"call-1\")"));
        // …and the full output is retrievable from the stash.
        let viewed = handle_stash_tool(
            &stash,
            "output_view",
            Some(
                &serde_json::json!({"tool_use_id": "call-1"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .unwrap();
        assert!(viewed.contains("xxx"));
    }

    #[test]
    fn render_oversized_shell_stashes_raw_stdout() {
        let stash = Mutex::new(OutputStash::new());
        let stdout = "line\n".repeat(MAX_TOOL_RESULT_CHARS); // far over the clamp
        let value = serde_json::json!({
            "ok": true, "exit_code": 0, "stdout": stdout, "stderr": ""
        });
        render_tool_result(&stash, "sh-1", "shell", &value);
        // The stash holds raw stdout (no JSON envelope), via extract_stash_content.
        let grepped = handle_stash_tool(
            &stash,
            "output_grep",
            Some(
                &serde_json::json!({
                    "tool_use_id": "sh-1", "pattern": "line"
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .unwrap();
        assert!(grepped.contains("line"));
        assert!(!grepped.contains("\"stdout\""));
    }

    #[test]
    fn handle_stash_tool_rejects_unknown_name() {
        let stash = Mutex::new(OutputStash::new());
        assert!(handle_stash_tool(&stash, "shell", None).is_err());
    }

    // ─── C6: durable (sha256 disk-backed) read-through ─────────────────────────

    #[test]
    fn stash_insert_writes_durable_blob_to_disk() {
        isolated_durable_root();
        let session = "session-durable-write-1";
        let tool = "durable-write-1";
        let mut stash = OutputStash::with_session_id(session);
        let body = "durable line a\ndurable line b\n";
        stash
            .insert(tool.into(), "shell".into(), body.into())
            .unwrap();

        let root = durable_root().expect("override sets a root");
        // The owner-qualified id-pointer exists and names the content-addressed
        // blob plus ownership/age/metadata for retention GC.
        let pointer = owner_id_pointer_path(&root, session, tool);
        let raw = std::fs::read_to_string(&pointer).expect("id pointer written");
        let record = parse_durable_pointer(&raw).expect("versioned pointer parses");
        assert_eq!(record.kind, DurablePointerKind::Version2);
        assert_eq!(record.tool_name, "shell");
        assert_eq!(record.content_hash, sha256_hex(body.as_bytes()));
        assert_eq!(record.session_id.as_deref(), Some(session));
        assert_eq!(record.tool_use_id.as_deref(), Some(tool));
        assert!(record.created_at_unix_secs.unwrap_or_default() > 0);
        // The blob exists and round-trips the exact content.
        let blob = blob_path(&root, &record.content_hash);
        assert_eq!(std::fs::read_to_string(&blob).unwrap(), body);
    }

    #[test]
    fn durable_pointer_parser_accepts_legacy_unknown_owner() {
        let body = "legacy durable line\n";
        let hash = sha256_hex(body.as_bytes());
        let record = parse_durable_pointer(&format!("shell\t{hash}\n")).unwrap();
        assert_eq!(record.kind, DurablePointerKind::Legacy);
        assert_eq!(record.tool_name, "shell");
        assert_eq!(record.content_hash, hash);
        assert_eq!(record.session_id, None);
        assert_eq!(record.created_at_unix_secs, None);
    }

    #[test]
    fn durable_read_resolves_legacy_pointer() {
        isolated_durable_root();
        let root = durable_root().expect("override sets a root");
        let body = "legacy read-through body\n";
        let hash = sha256_hex(body.as_bytes());
        let blobs_dir = root.join("blobs");
        let ids_dir = root.join("ids");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        std::fs::create_dir_all(&ids_dir).unwrap();

        atomic_write(&blobs_dir, &blob_path(&root, &hash), body.as_bytes()).unwrap();
        atomic_write(
            &ids_dir,
            &id_pointer_path(&root, "legacy-pointer-1"),
            format!("shell\t{hash}").as_bytes(),
        )
        .unwrap();

        let stash = OutputStash::new();
        let viewed = stash.view("legacy-pointer-1", 0, 10).unwrap();
        assert!(viewed.contains("legacy read-through body"));

        let (tool_name, full_text) = durable_read("legacy-pointer-1").unwrap();
        assert_eq!(tool_name, "shell");
        assert_eq!(full_text, body);
    }

    #[test]
    fn output_view_fast_path_then_durable_path_after_eviction() {
        isolated_durable_root();
        let mut stash = OutputStash::with_session_id("view-durable-session");
        let text: String = (0..20).map(|i| format!("view-line {i}\n")).collect();
        stash
            .insert("view-durable-1".into(), "shell".into(), text)
            .unwrap();

        // Fast path: in-memory entry present.
        let fast = stash.view("view-durable-1", 0, 5).unwrap();
        assert!(fast.contains("view-line 0"));
        assert!(fast.contains("view-line 4"));

        // Drop the in-memory entry (simulates eviction / clear / restart).
        stash.clear();
        assert!(stash.find("view-durable-1").is_err());

        // Durable path: view still resolves from disk by the id pointer.
        let durable = stash.view("view-durable-1", 0, 5).unwrap();
        assert!(durable.contains("view-line 0"));
        assert!(durable.contains("view-line 4"));
    }

    #[test]
    fn output_grep_fast_path_then_durable_path_after_eviction() {
        isolated_durable_root();
        let mut stash = OutputStash::with_session_id("grep-durable-session");
        let text = "alpha\nbeta\nERROR: durable boom\ngamma\n";
        stash
            .insert("grep-durable-1".into(), "shell".into(), text.into())
            .unwrap();

        // Fast path.
        let fast = stash.grep("grep-durable-1", "ERROR", 1).unwrap();
        assert!(fast.contains("ERROR: durable boom"));

        // Drop in-memory state.
        stash.clear();
        assert!(stash.find("grep-durable-1").is_err());

        // Durable path: grep still resolves from disk.
        let durable = stash.grep("grep-durable-1", "ERROR", 1).unwrap();
        assert!(durable.contains("ERROR: durable boom"));
        assert!(durable.contains("beta")); // context before
        assert!(durable.contains("gamma")); // context after
    }

    #[test]
    fn durable_path_survives_via_handle_stash_tool() {
        isolated_durable_root();
        let stash = Mutex::new(OutputStash::with_session_id("render-durable-session"));
        let big = "y".repeat(MAX_TOOL_RESULT_CHARS * 2);
        render_tool_result(
            &stash,
            "render-durable-1",
            "shell",
            &serde_json::Value::String(big),
        );

        // Wipe the in-memory map, leaving only the durable blob.
        stash.lock().unwrap().clear();

        let viewed = handle_stash_tool(
            &stash,
            "output_view",
            Some(
                &serde_json::json!({"tool_use_id": "render-durable-1"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .expect("durable view resolves after in-memory clear");
        assert!(viewed.contains("yyy"));
    }

    #[test]
    fn missing_durable_blob_degrades_gracefully() {
        isolated_durable_root();
        let stash = OutputStash::new();
        // Never inserted: neither in-memory nor on disk → clean error, no panic.
        let err = stash.view("never-stashed-id", 0, 10).unwrap_err();
        assert!(err.contains("No stashed output"));
    }

    #[test]
    fn corrupt_durable_blob_degrades_gracefully() {
        isolated_durable_root();
        let session = "corrupt-session-1";
        let tool = "corrupt-1";
        let mut stash = OutputStash::with_session_id(session);
        stash
            .insert(tool.into(), "shell".into(), "real content\n".into())
            .unwrap();
        stash.clear();

        // Corrupt the durable store: delete the content blob, leaving the pointer.
        let root = durable_root().unwrap();
        let pointer = std::fs::read_to_string(owner_id_pointer_path(&root, session, tool)).unwrap();
        let record = parse_durable_pointer(&pointer).unwrap();
        std::fs::remove_file(blob_path(&root, &record.content_hash)).unwrap();

        // Read falls through to a clear error rather than panicking.
        let err = stash.view(tool, 0, 10).unwrap_err();
        assert!(err.contains("No stashed output"));
        // And the low-level reader reports the missing blob distinctly.
        assert!(
            durable_read_at(&root, tool, Some(session))
                .unwrap_err()
                .contains("blob missing")
        );
    }

    // ─── Durable output-stash GC ──────────────────────────────────────────────

    fn gc_root(name: &str) -> PathBuf {
        let root = crate::test_helpers::test_persistent_dir("djinn-output-stash-gc-").join(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("ids")).unwrap();
        std::fs::create_dir_all(root.join("blobs")).unwrap();
        root
    }

    fn write_gc_blob(root: &Path, body: &str) -> String {
        let hash = sha256_hex(body.as_bytes());
        let blobs_dir = root.join("blobs");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        atomic_write(&blobs_dir, &blob_path(root, &hash), body.as_bytes()).unwrap();
        hash
    }

    fn write_gc_pointer(root: &Path, pointer_name: &str, record: DurablePointerRecord) -> PathBuf {
        let ids_dir = root.join("ids");
        std::fs::create_dir_all(&ids_dir).unwrap();
        let path = ids_dir.join(pointer_name);
        atomic_write(&ids_dir, &path, record.serialize().as_bytes()).unwrap();
        path
    }

    fn write_legacy_gc_pointer(
        root: &Path,
        pointer_name: &str,
        tool_name: &str,
        content_hash: &str,
    ) -> PathBuf {
        let ids_dir = root.join("ids");
        std::fs::create_dir_all(&ids_dir).unwrap();
        let path = ids_dir.join(pointer_name);
        atomic_write(
            &ids_dir,
            &path,
            format!("{tool_name}\t{content_hash}").as_bytes(),
        )
        .unwrap();
        path
    }

    fn gc_session(status: SessionStatus, ended_at_unix_secs: Option<u64>) -> OutputStashGcSession {
        OutputStashGcSession {
            status,
            ended_at_unix_secs,
        }
    }

    #[test]
    fn gc_deletes_expired_terminal_pointers_and_retains_live_or_recent_sessions() {
        let root = gc_root("terminal-cutoff");
        let expired_hash = write_gc_blob(&root, "expired terminal body");
        let recent_hash = write_gc_blob(&root, "recent terminal body");
        let running_hash = write_gc_blob(&root, "running body");
        let paused_hash = write_gc_blob(&root, "paused body");

        let expired = write_gc_pointer(
            &root,
            "expired",
            DurablePointerRecord::new_v1("shell", &expired_hash, Some("expired-session"), 1),
        );
        let recent = write_gc_pointer(
            &root,
            "recent",
            DurablePointerRecord::new_v1("shell", &recent_hash, Some("recent-session"), 1),
        );
        let running = write_gc_pointer(
            &root,
            "running",
            DurablePointerRecord::new_v1("shell", &running_hash, Some("running-session"), 1),
        );
        let paused = write_gc_pointer(
            &root,
            "paused",
            DurablePointerRecord::new_v1("shell", &paused_hash, Some("paused-session"), 1),
        );

        let mut sessions = std::collections::HashMap::new();
        sessions.insert(
            "expired-session",
            gc_session(SessionStatus::Completed, Some(999)),
        );
        sessions.insert(
            "recent-session",
            gc_session(SessionStatus::Failed, Some(1_001)),
        );
        sessions.insert("running-session", gc_session(SessionStatus::Running, None));
        sessions.insert("paused-session", gc_session(SessionStatus::Paused, None));

        let report = gc_durable_output_stash(&root, 1_000, |id| Ok(sessions.get(id).cloned()));

        assert!(report.is_success(), "unexpected GC errors: {report:?}");
        assert_eq!(report.pointers_scanned, 4);
        assert_eq!(report.pointers_deleted, 1);
        assert!(!expired.exists());
        assert!(recent.exists());
        assert!(running.exists());
        assert!(paused.exists());
    }

    #[test]
    fn gc_keeps_blob_referenced_by_retained_pointer_after_shared_expired_pointer_removed() {
        let root = gc_root("shared-hash");
        let hash = write_gc_blob(&root, "same shared content");
        let expired = write_gc_pointer(
            &root,
            "expired-shared",
            DurablePointerRecord::new_v1("shell", &hash, Some("expired-session"), 1),
        );
        let live = write_gc_pointer(
            &root,
            "live-shared",
            DurablePointerRecord::new_v1("shell", &hash, Some("live-session"), 1),
        );

        let mut sessions = std::collections::HashMap::new();
        sessions.insert(
            "expired-session",
            gc_session(SessionStatus::Interrupted, Some(10)),
        );
        sessions.insert("live-session", gc_session(SessionStatus::Running, None));

        let report = gc_durable_output_stash(&root, unix_time_secs() + 1_000, |id| {
            Ok(sessions.get(id).cloned())
        });

        assert!(report.is_success(), "unexpected GC errors: {report:?}");
        assert_eq!(report.pointers_deleted, 1);
        assert!(!expired.exists());
        assert!(live.exists());
        assert!(blob_path(&root, &hash).exists());
    }

    #[test]
    fn gc_deletes_pointers_whose_blobs_are_missing() {
        let root = gc_root("missing-blob-pointer");
        let missing_hash = sha256_hex(b"not written");
        let orphan_pointer = write_gc_pointer(
            &root,
            "orphan-pointer",
            DurablePointerRecord::new_v1("shell", &missing_hash, Some("live-session"), 1),
        );

        let report = gc_durable_output_stash(&root, 0, |_| {
            Ok(Some(gc_session(SessionStatus::Running, None)))
        });

        assert!(report.is_success(), "unexpected GC errors: {report:?}");
        assert_eq!(report.pointers_deleted, 1);
        assert!(!orphan_pointer.exists());
    }

    #[test]
    fn gc_deletes_unreferenced_blobs_older_than_cutoff() {
        let root = gc_root("unreferenced-blob");
        let recent_hash = write_gc_blob(&root, "recent unreferenced content");

        let recent_report = gc_durable_output_stash(&root, 0, |_| Ok(None));
        assert!(
            recent_report.is_success(),
            "unexpected GC errors: {recent_report:?}"
        );
        assert_eq!(recent_report.blobs_deleted, 0);
        assert!(blob_path(&root, &recent_hash).exists());

        let old_hash = write_gc_blob(&root, "old unreferenced content");
        let old_report = gc_durable_output_stash(&root, unix_time_secs() + 1_000, |_| Ok(None));
        assert!(
            old_report.is_success(),
            "unexpected GC errors: {old_report:?}"
        );
        assert_eq!(old_report.blobs_deleted, 2);
        assert!(!blob_path(&root, &old_hash).exists());
        assert!(!blob_path(&root, &recent_hash).exists());

        let protected_hash = write_gc_blob(&root, "protected content");
        write_gc_pointer(
            &root,
            "protected-pointer",
            DurablePointerRecord::new_v1("shell", &protected_hash, Some("live-session"), 1),
        );
        let protected_report = gc_durable_output_stash(&root, unix_time_secs() + 1_000, |_| {
            Ok(Some(gc_session(SessionStatus::Running, None)))
        });
        assert!(
            protected_report.is_success(),
            "unexpected GC errors: {protected_report:?}"
        );
        assert_eq!(protected_report.blobs_deleted, 0);
        assert!(blob_path(&root, &protected_hash).exists());
    }

    #[test]
    fn gc_retains_legacy_and_unknown_owner_pointers_conservatively() {
        let root = gc_root("legacy-unknown-owner");
        let legacy_hash = write_gc_blob(&root, "legacy body");
        let unknown_hash = write_gc_blob(&root, "unknown v1 owner body");
        let legacy = write_legacy_gc_pointer(&root, "legacy", "shell", &legacy_hash);
        let unknown = write_gc_pointer(
            &root,
            "unknown-owner",
            DurablePointerRecord::new_v1("shell", &unknown_hash, None, 1),
        );

        let report = gc_durable_output_stash(&root, unix_time_secs() + 1_000, |_| {
            panic!("legacy/unknown-owner pointers must not consult session lookup")
        });

        assert!(report.is_success(), "unexpected GC errors: {report:?}");
        assert_eq!(report.pointers_deleted, 0);
        assert!(legacy.exists());
        assert!(unknown.exists());
        assert!(blob_path(&root, &legacy_hash).exists());
        assert!(blob_path(&root, &unknown_hash).exists());
    }

    #[test]
    fn gc_lookup_failure_retains_pointer_so_next_sweep_can_retry() {
        let root = gc_root("retry-after-session-lookup-failure");
        let hash = write_gc_blob(&root, "retryable terminal body");
        let pointer = write_gc_pointer(
            &root,
            "retryable-pointer",
            DurablePointerRecord::new_v1("shell", &hash, Some("terminal-session"), 1),
        );

        let failed_report = gc_durable_output_stash(&root, 1_000, |_| {
            Err("temporary session repository outage".to_string())
        });

        assert!(!failed_report.is_success());
        assert_eq!(failed_report.pointers_deleted, 0);
        assert_eq!(failed_report.pointers_retained, 1);
        assert!(pointer.exists(), "failed GC must not mark work complete");
        assert!(blob_path(&root, &hash).exists());

        let retry_report = gc_durable_output_stash(&root, 1_000, |_| {
            Ok(Some(gc_session(SessionStatus::Completed, Some(999))))
        });

        assert!(
            retry_report.is_success(),
            "retry should succeed: {retry_report:?}"
        );
        assert_eq!(retry_report.pointers_deleted, 1);
        assert!(!pointer.exists(), "next sweep retries retained failed work");
    }

    #[test]
    fn output_view_and_grep_read_through_survives_gc_for_active_session() {
        let root = gc_root("active-read-through-regression");
        let session = "gc-active-read-through-session";
        let tool = "gc-active-read-through-tool";
        let mut stash = OutputStash::with_session_id_and_durable_root(session, root.clone());
        let body = "alpha before gc\nneedle stays searchable\nomega after gc\n";
        stash
            .insert(tool.into(), "shell".into(), body.into())
            .unwrap();

        let pointer_path = owner_id_pointer_path(&root, session, tool);
        let record = parse_durable_pointer(
            &std::fs::read_to_string(&pointer_path).expect("durable pointer written"),
        )
        .expect("durable pointer parses");
        assert_eq!(record.kind, DurablePointerKind::Version2);
        let blob = blob_path(&root, &record.content_hash);
        assert!(pointer_path.exists());
        assert!(blob.exists());

        stash.clear();
        let before_gc = stash
            .view(tool, 0, 10)
            .expect("durable view resolves before GC");
        assert!(before_gc.contains("needle stays searchable"));

        let report = gc_durable_output_stash(&root, unix_time_secs() + 1_000, |session_id| {
            if session_id == session {
                Ok(Some(gc_session(SessionStatus::Running, None)))
            } else {
                Ok(None)
            }
        });

        assert!(report.is_success(), "unexpected GC errors: {report:?}");
        assert!(pointer_path.exists(), "active session pointer is retained");
        assert!(blob.exists(), "active session blob is retained");

        let after_gc = stash
            .view(tool, 0, 10)
            .expect("durable view resolves after GC");
        assert_eq!(before_gc, after_gc);

        let grepped = stash
            .grep(tool, "needle", 1)
            .expect("durable grep resolves after GC");
        assert!(grepped.contains("needle stays searchable"));
        assert!(grepped.contains("alpha before gc"));
        assert!(grepped.contains("omega after gc"));
    }

    #[test]
    fn gc_retains_recent_terminal_output_but_prunes_expired_terminal_read_through() {
        let root = gc_root("terminal-retention-read-through-regression");

        let mut recent_stash = OutputStash::with_session_id_and_durable_root(
            "gc-recent-terminal-session",
            root.clone(),
        );
        recent_stash
            .insert(
                "gc-recent-terminal-tool".into(),
                "shell".into(),
                "recent terminal output\nretained by retention window\n".into(),
            )
            .unwrap();
        recent_stash.clear();

        let mut expired_stash = OutputStash::with_session_id_and_durable_root(
            "gc-expired-terminal-session",
            root.clone(),
        );
        expired_stash
            .insert(
                "gc-expired-terminal-tool".into(),
                "shell".into(),
                "expired terminal output\nshould be pruned\n".into(),
            )
            .unwrap();
        expired_stash.clear();

        let recent_pointer = owner_id_pointer_path(
            &root,
            "gc-recent-terminal-session",
            "gc-recent-terminal-tool",
        );
        let recent_record = parse_durable_pointer(
            &std::fs::read_to_string(&recent_pointer).expect("recent pointer written"),
        )
        .expect("recent pointer parses");
        let recent_blob = blob_path(&root, &recent_record.content_hash);
        let expired_pointer = owner_id_pointer_path(
            &root,
            "gc-expired-terminal-session",
            "gc-expired-terminal-tool",
        );
        let expired_record = parse_durable_pointer(
            &std::fs::read_to_string(&expired_pointer).expect("expired pointer written"),
        )
        .expect("expired pointer parses");
        let expired_blob = blob_path(&root, &expired_record.content_hash);

        assert!(
            recent_stash
                .view("gc-recent-terminal-tool", 0, 10)
                .unwrap()
                .contains("retained by retention window")
        );
        assert!(
            expired_stash
                .view("gc-expired-terminal-tool", 0, 10)
                .unwrap()
                .contains("should be pruned")
        );

        let cutoff = unix_time_secs() + 1_000;
        let report = gc_durable_output_stash(&root, cutoff, |session_id| match session_id {
            "gc-recent-terminal-session" => Ok(Some(gc_session(
                SessionStatus::Completed,
                Some(cutoff.saturating_add(1)),
            ))),
            "gc-expired-terminal-session" => Ok(Some(gc_session(
                SessionStatus::Failed,
                Some(cutoff.saturating_sub(1)),
            ))),
            _ => Ok(None),
        });

        assert!(report.is_success(), "unexpected GC errors: {report:?}");
        assert!(
            recent_pointer.exists(),
            "recent terminal pointer is retained"
        );
        assert!(recent_blob.exists(), "recent terminal blob is retained");
        assert!(
            recent_stash
                .grep("gc-recent-terminal-tool", "retained", 0)
                .expect("retained terminal grep still resolves")
                .contains("retained by retention window")
        );

        assert!(
            !expired_pointer.exists(),
            "expired terminal pointer is pruned"
        );
        assert!(!expired_blob.exists(), "expired terminal blob is pruned");
        assert!(
            expired_stash
                .view("gc-expired-terminal-tool", 0, 10)
                .unwrap_err()
                .contains("No stashed output")
        );
    }

    #[test]
    fn in_memory_output_still_wins_when_durable_pointer_is_stale() {
        let root = gc_root("in-memory-first-regression");
        let session = "gc-memory-first-session";
        let tool = "gc-memory-first-tool";
        let mut stash = OutputStash::with_session_id_and_durable_root(session, root.clone());
        stash
            .insert(
                tool.into(),
                "shell".into(),
                "fresh in-memory output\n".into(),
            )
            .unwrap();

        let stale_hash = write_gc_blob(&root, "stale durable output\n");
        let pointer_path = owner_id_pointer_path(&root, session, tool);
        atomic_write(
            &root.join("ids"),
            &pointer_path,
            DurablePointerRecord::new_v1("shell", &stale_hash, Some(session), unix_time_secs())
                .serialize()
                .as_bytes(),
        )
        .unwrap();

        let fast_path = stash.view(tool, 0, 10).expect("in-memory view resolves");
        assert!(fast_path.contains("fresh in-memory output"));
        assert!(!fast_path.contains("stale durable output"));

        stash.clear();
        let durable_path = stash
            .view(tool, 0, 10)
            .expect("durable fallback resolves after clear");
        assert!(durable_path.contains("stale durable output"));
    }

    // ─── V2 durable metadata contract ─────────────────────────────────────────

    fn write_v2_gc_pointer(
        root: &Path,
        pointer_name: &str,
        tool_name: &str,
        content_hash: &str,
        owner: &str,
        id: &str,
        details: &DurableOutputDetails,
    ) -> PathBuf {
        let record = DurablePointerRecord::new_v2(tool_name, content_hash, owner, id, details);
        write_gc_pointer(root, pointer_name, record)
    }

    #[test]
    fn v2_durable_pointer_round_trips_through_serialize_and_parse() {
        let body = "v2 round-trip body\n";
        let hash = sha256_hex(body.as_bytes());
        let details = DurableOutputDetails {
            turn: 3,
            result_kind: "tool_result".into(),
            original_chars: body.chars().count(),
            stored_chars: body.chars().count(),
            completeness: "complete".into(),
        };
        let record = DurablePointerRecord::new_v2("shell", &hash, "owner-x", "call-x", &details);
        let serialized = record.serialize();
        let parsed = parse_durable_pointer(&serialized).expect("v2 round-trips");
        assert_eq!(parsed.kind, DurablePointerKind::Version2);
        assert_eq!(parsed.tool_name, "shell");
        assert_eq!(parsed.content_hash, hash);
        assert_eq!(parsed.session_id.as_deref(), Some("owner-x"));
        assert_eq!(parsed.tool_use_id.as_deref(), Some("call-x"));
        assert_eq!(parsed.turn, Some(3));
        assert_eq!(parsed.result_kind.as_deref(), Some("tool_result"));
        assert_eq!(parsed.original_chars, Some(body.chars().count()));
        assert_eq!(parsed.stored_chars, Some(body.chars().count()));
        assert_eq!(parsed.completeness.as_deref(), Some("complete"));
        assert!(parsed.created_at_unix_secs.unwrap_or_default() > 0);
    }

    #[test]
    fn v2_parser_rejects_corrupt_records() {
        // Short hash
        assert!(
            parse_durable_pointer(
                "v2\tshell\tshort\towner\tid\t0\ttool_result\t10\t10\tcomplete\t1000"
            )
            .is_err()
        );
        // Missing owner
        assert!(parse_durable_pointer(
            "v2\tshell\tcccc5555555555555555555555555555555555555555555555555555555555\t\tid\t0\ttool_result\t10\t10\tcomplete\t1000"
        )
        .is_err());
        // Bad completeness
        assert!(parse_durable_pointer(
            "v2\tshell\tcccc5555555555555555555555555555555555555555555555555555555555\towner\tid\t0\ttool_result\t10\t10\tbogus\t1000"
        )
        .is_err());
        // Wrong field count
        assert!(parse_durable_pointer("v2\tshell\thash\towner").is_err());
    }

    #[test]
    fn v2_parser_still_accepts_v1_and_legacy() {
        let v1 = parse_durable_pointer(
            "v1\tshell\tcccc5555555555555555555555555555555555555555555555555555555555\tsession-a\t1000",
        )
        .unwrap();
        assert_eq!(v1.kind, DurablePointerKind::Version1);
        assert_eq!(v1.session_id.as_deref(), Some("session-a"));

        let legacy = parse_durable_pointer(
            "shell\tcccc5555555555555555555555555555555555555555555555555555555555",
        )
        .unwrap();
        assert_eq!(legacy.kind, DurablePointerKind::Legacy);
        assert_eq!(legacy.session_id, None);
    }

    #[test]
    fn gc_preserves_v2_pointer_for_active_session() {
        let root = gc_root("v2-active-session");
        let body = "v2 active body\n";
        let hash = write_gc_blob(&root, body);
        let details = DurableOutputDetails {
            turn: 1,
            result_kind: "tool_result".into(),
            original_chars: body.chars().count(),
            stored_chars: body.chars().count(),
            completeness: "complete".into(),
        };
        let pointer = write_v2_gc_pointer(
            &root,
            "v2-active",
            "shell",
            &hash,
            "active-session",
            "call-active",
            &details,
        );

        let report = gc_durable_output_stash(&root, unix_time_secs() + 1_000, |id| {
            assert_eq!(id, "active-session");
            Ok(Some(gc_session(SessionStatus::Running, None)))
        });

        assert!(report.is_success(), "unexpected GC errors: {report:?}");
        assert_eq!(report.pointers_deleted, 0);
        assert!(pointer.exists());
        assert!(blob_path(&root, &hash).exists());
    }

    #[test]
    fn gc_removes_v2_pointer_for_expired_terminal_session() {
        let root = gc_root("v2-expired-terminal");
        let body = "v2 expired body\n";
        let hash = write_gc_blob(&root, body);
        let details = DurableOutputDetails {
            turn: 1,
            result_kind: "tool_result".into(),
            original_chars: body.chars().count(),
            stored_chars: body.chars().count(),
            completeness: "complete".into(),
        };
        let pointer = write_v2_gc_pointer(
            &root,
            "v2-expired",
            "shell",
            &hash,
            "expired-session",
            "call-expired",
            &details,
        );

        let cutoff = unix_time_secs() + 1_000;
        let report = gc_durable_output_stash(&root, cutoff, |id| {
            assert_eq!(id, "expired-session");
            Ok(Some(gc_session(
                SessionStatus::Completed,
                Some(cutoff.saturating_sub(1)),
            )))
        });

        assert!(report.is_success(), "unexpected GC errors: {report:?}");
        assert_eq!(report.pointers_deleted, 1);
        assert!(!pointer.exists());
        // Blob is unreferenced and its mtime is at or before the future cutoff.
        assert!(!blob_path(&root, &hash).exists());
    }

    #[test]
    fn gc_keeps_blob_referenced_by_retained_v2_pointer_after_expired_v2_removed() {
        let root = gc_root("v2-shared-hash");
        let body = "v2 shared content\n";
        let hash = write_gc_blob(&root, body);
        let details = DurableOutputDetails {
            turn: 1,
            result_kind: "tool_result".into(),
            original_chars: body.chars().count(),
            stored_chars: body.chars().count(),
            completeness: "complete".into(),
        };
        let expired = write_v2_gc_pointer(
            &root,
            "v2-expired-shared",
            "shell",
            &hash,
            "expired-session",
            "call-exp",
            &details,
        );
        let live = write_v2_gc_pointer(
            &root,
            "v2-live-shared",
            "shell",
            &hash,
            "live-session",
            "call-live",
            &details,
        );

        let report = gc_durable_output_stash(&root, unix_time_secs() + 1_000, |id| match id {
            "expired-session" => Ok(Some(gc_session(SessionStatus::Interrupted, Some(10)))),
            "live-session" => Ok(Some(gc_session(SessionStatus::Running, None))),
            _ => Ok(None),
        });

        assert!(report.is_success(), "unexpected GC errors: {report:?}");
        assert_eq!(report.pointers_deleted, 1);
        assert!(!expired.exists());
        assert!(live.exists());
        assert!(blob_path(&root, &hash).exists());
    }

    #[test]
    fn gc_removes_v2_pointer_whose_blob_is_missing() {
        let root = gc_root("v2-missing-blob");
        let missing_hash = sha256_hex(b"this blob was never written to disk for this test pointer");
        let details = DurableOutputDetails {
            turn: 1,
            result_kind: "tool_result".into(),
            original_chars: 10,
            stored_chars: 10,
            completeness: "complete".into(),
        };
        let pointer = write_v2_gc_pointer(
            &root,
            "v2-orphan",
            "shell",
            &missing_hash,
            "live-session",
            "call-orphan",
            &details,
        );

        let report = gc_durable_output_stash(&root, 0, |_| {
            Ok(Some(gc_session(SessionStatus::Running, None)))
        });

        assert!(report.is_success(), "unexpected GC errors: {report:?}");
        assert_eq!(report.pointers_deleted, 1);
        assert!(!pointer.exists());
    }

    #[test]
    fn v2_insert_is_idempotent_for_identical_retry() {
        let root = gc_root("v2-idempotent");
        let mut stash = OutputStash::with_session_id_and_durable_root("owner-a", root.clone());
        let body = "idempotent body\n";
        stash
            .insert("call-a".into(), "shell".into(), body.into())
            .unwrap();
        // Identical retry succeeds without error.
        stash
            .insert("call-a".into(), "shell".into(), body.into())
            .unwrap();
        let pointer = owner_id_pointer_path(&root, "owner-a", "call-a");
        assert!(pointer.exists());
    }

    #[test]
    fn v2_insert_rejects_conflicting_metadata() {
        let root = gc_root("v2-conflict");
        let mut stash = OutputStash::with_session_id_and_durable_root("owner-a", root.clone());
        let body = "conflict body\n";
        stash
            .insert_with_metadata(
                "call-c".into(),
                "shell".into(),
                body.into(),
                DurableOutputDetails {
                    turn: 1,
                    result_kind: "tool_result".into(),
                    original_chars: body.chars().count(),
                    stored_chars: body.chars().count(),
                    completeness: "complete".into(),
                },
            )
            .unwrap();
        // Conflicting turn should error.
        let err = stash
            .insert_with_metadata(
                "call-c".into(),
                "shell".into(),
                body.into(),
                DurableOutputDetails {
                    turn: 2,
                    result_kind: "tool_result".into(),
                    original_chars: body.chars().count(),
                    stored_chars: body.chars().count(),
                    completeness: "complete".into(),
                },
            )
            .unwrap_err();
        assert!(err.contains("conflicting durable output"));
    }

    #[test]
    fn v2_insert_rejects_missing_blob_on_retry() {
        let root = gc_root("v2-missing-blob-retry");
        let mut stash = OutputStash::with_session_id_and_durable_root("owner-a", root.clone());
        let body = "blob-retry body\n";
        stash
            .insert("call-d".into(), "shell".into(), body.into())
            .unwrap();
        // Delete the blob, then an identical retry must fail (not silently succeed).
        let pointer =
            std::fs::read_to_string(owner_id_pointer_path(&root, "owner-a", "call-d")).unwrap();
        let record = parse_durable_pointer(&pointer).unwrap();
        std::fs::remove_file(blob_path(&root, &record.content_hash)).unwrap();
        assert!(
            stash
                .insert("call-d".into(), "shell".into(), body.into())
                .is_err()
        );
    }

    #[test]
    fn v2_cross_session_isolation_prevents_unauthorized_view() {
        let root = gc_root("v2-cross-session");
        let mut owner_a = OutputStash::with_session_id_and_durable_root("owner-a", root.clone());
        let mut owner_b = OutputStash::with_session_id_and_durable_root("owner-b", root.clone());

        owner_a
            .insert(
                "call-shared".into(),
                "shell".into(),
                "secret from A\n".into(),
            )
            .unwrap();
        owner_b
            .insert(
                "call-shared".into(),
                "shell".into(),
                "secret from B\n".into(),
            )
            .unwrap();

        // Each session resolves its own output despite the shared tool_use_id.
        assert!(
            owner_a
                .view("call-shared", 0, 10)
                .unwrap()
                .contains("secret from A")
        );
        assert!(
            owner_b
                .view("call-shared", 0, 10)
                .unwrap()
                .contains("secret from B")
        );

        // After clear, each session resolves only its own durable record.
        owner_a.clear();
        owner_b.clear();
        assert!(
            owner_a
                .view("call-shared", 0, 10)
                .unwrap()
                .contains("secret from A")
        );
        assert!(
            owner_b
                .view("call-shared", 0, 10)
                .unwrap()
                .contains("secret from B")
        );
    }

    #[test]
    fn v2_listing_only_returns_trusted_session_records() {
        let root = gc_root("v2-listing");
        let mut owner_a = OutputStash::with_session_id_and_durable_root("owner-a", root.clone());
        let mut owner_b = OutputStash::with_session_id_and_durable_root("owner-b", root.clone());

        owner_a
            .insert("call-a-1".into(), "shell".into(), "A first\n".into())
            .unwrap();
        owner_a
            .insert("call-a-2".into(), "shell".into(), "A second\n".into())
            .unwrap();
        owner_b
            .insert("call-b-1".into(), "shell".into(), "B first\n".into())
            .unwrap();

        let a_listed = owner_a.list_durable_outputs().unwrap();
        assert_eq!(a_listed.len(), 2);
        let b_listed = owner_b.list_durable_outputs().unwrap();
        assert_eq!(b_listed.len(), 1);

        // After reopen (no in-memory), listing still works.
        drop(owner_a);
        let reopened_a = OutputStash::with_session_id_and_durable_root("owner-a", root.clone());
        let reopened_list = reopened_a.list_durable_outputs().unwrap();
        assert_eq!(reopened_list.len(), 2);
        // Sorted by created_at then tool_use_id.
        assert_eq!(reopened_list[0].tool_use_id, "call-a-1");
        assert_eq!(reopened_list[1].tool_use_id, "call-a-2");
    }

    #[test]
    fn v2_listing_excludes_foreign_corrupt_and_missing_blob_records() {
        let root = gc_root("v2-listing-exclusions");
        let mut owner = OutputStash::with_session_id_and_durable_root("owner-a", root.clone());
        owner
            .insert("call-good".into(), "shell".into(), "good output\n".into())
            .unwrap();

        // Write a foreign v2 pointer in the same ids dir (different owner).
        let foreign_hash = write_gc_blob(&root, "foreign content\n");
        let foreign_details = DurableOutputDetails {
            turn: 1,
            result_kind: "tool_result".into(),
            original_chars: 16,
            stored_chars: 16,
            completeness: "complete".into(),
        };
        write_v2_gc_pointer(
            &root,
            "foreign",
            "shell",
            &foreign_hash,
            "owner-b",
            "call-foreign",
            &foreign_details,
        );

        // Write a corrupt pointer.
        let corrupt_dir = root.join("ids");
        std::fs::create_dir_all(&corrupt_dir).unwrap();
        atomic_write(
            &corrupt_dir,
            &corrupt_dir.join("corrupt-pointer"),
            b"garbage data",
        )
        .unwrap();

        // Delete the blob for a good pointer.
        let good_pointer =
            std::fs::read_to_string(owner_id_pointer_path(&root, "owner-a", "call-good")).unwrap();
        let good_record = parse_durable_pointer(&good_pointer).unwrap();
        std::fs::remove_file(blob_path(&root, &good_record.content_hash)).unwrap();

        let listed = owner.list_durable_outputs().unwrap();
        assert_eq!(
            listed.len(),
            0,
            "listing must exclude foreign, corrupt, and missing-blob records"
        );
    }

    #[test]
    fn unowned_stash_cannot_view_v2_record() {
        let root = gc_root("v2-unowned-cannot-view");
        let mut owner = OutputStash::with_session_id_and_durable_root("owner-a", root.clone());
        owner
            .insert(
                "call-secret".into(),
                "shell".into(),
                "secret content\n".into(),
            )
            .unwrap();

        // A stash with no session_id must NOT resolve a v2 record.
        let unowned = OutputStash::with_session_id_and_durable_root("nobody", root.clone());
        assert!(unowned.view("call-secret", 0, 10).is_err());
    }
}
