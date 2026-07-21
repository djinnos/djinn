//! Session-scoped stash for full tool outputs that exceed the truncation limit.
//!
//! Before `smart_truncate` discards the middle of a large tool result, the full
//! text is stashed here so the agent can paginate (`output_view`) or search
//! (`output_grep`) it later without re-running the command.
//!
//! Bounded: max 10 entries, max 5 MB total. FIFO eviction when either limit is
//! hit. Each reply-loop instance owns its own stash — no cross-session sharing.
//!
//! Durable read-through (C6): in addition to the in-memory map, every stashed
//! blob is written once to a content-addressed file under the djinn cache dir
//! (keyed by `sha256(content)`), plus a tiny id-pointer so `output_view` /
//! `output_grep` can resolve a `tool_use_id` after the in-memory entry is gone
//! (process restart, eviction, or post-compaction `clear`). The in-memory path
//! is unchanged and always wins; the disk fallback only runs on a miss and
//! degrades gracefully (best-effort writes, clear errors on read failure).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sha2::{Digest, Sha256};

#[cfg(test)]
use djinn_core::models::SessionStatus;

mod durable;
mod synopsis;

pub use durable::{OutputStashGcReport, OutputStashGcSession, gc_durable_output_stash};

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

/// Durable output-stash root used by coordinator maintenance. Exposed as a
/// narrow integration point so GC wiring uses the same cache location as normal
/// durable writes/reads.
#[allow(dead_code)]
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
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

impl DurablePointerRecord {
    fn new_v1(tool_name: &str, content_hash: &str, session_id: Option<&str>, created: u64) -> Self {
        Self {
            kind: DurablePointerKind::Version1,
            tool_name: tool_name.into(),
            content_hash: content_hash.into(),
            session_id: session_id.map(str::to_string),
            created_at_unix_secs: Some(created),
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
                "v2\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
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
                "v1\t{}\t{}\t{}\t{}",
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
    djinn_core::clock::Clock::now(&djinn_core::clock::SystemClock::new())
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
#[cfg(any(test, feature = "test-support"))]
static DURABLE_ROOT_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

#[cfg(any(test, feature = "test-support"))]
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
#[cfg(any(test, feature = "test-support"))]
fn durable_root() -> Option<PathBuf> {
    Some(test_durable_root())
}

#[cfg(not(any(test, feature = "test-support")))]
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

/// Historic id-pointer path: `<root>/ids/<sha256(tool_use_id)>`. Legacy and v1
/// records used this unqualified name and remain readable through the explicit
/// compatibility path below.
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
    let nanos = djinn_core::clock::Clock::now(&djinn_core::clock::SystemClock::new())
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
///
/// This convenience wrapper is used only by this module's unit tests. The
/// test-support integration path supplies an explicit durable root through
/// `OutputStash` and therefore calls `durable_read_at` instead.
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
    #[cfg(any(test, feature = "test-support"))]
    durable_root_override: Option<PathBuf>,
    #[cfg(any(test, feature = "test-support"))]
    fail_durable_writes_for_test: bool,
}

impl OutputStash {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            total_bytes: 0,
            owner_session_id: None,
            #[cfg(any(test, feature = "test-support"))]
            durable_root_override: None,
            #[cfg(any(test, feature = "test-support"))]
            fail_durable_writes_for_test: false,
        }
    }

    pub fn with_session_id(session_id: impl Into<String>) -> Self {
        Self {
            entries: VecDeque::new(),
            total_bytes: 0,
            owner_session_id: Some(session_id.into()),
            #[cfg(any(test, feature = "test-support"))]
            durable_root_override: None,
            #[cfg(any(test, feature = "test-support"))]
            fail_durable_writes_for_test: false,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_session_id_and_durable_root(
        session_id: impl Into<String>,
        durable_root: PathBuf,
    ) -> Self {
        Self {
            entries: VecDeque::new(),
            total_bytes: 0,
            owner_session_id: Some(session_id.into()),
            durable_root_override: Some(durable_root),
            fail_durable_writes_for_test: false,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_fail_durable_writes_for_test(&mut self, fail: bool) {
        self.fail_durable_writes_for_test = fail;
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
        #[cfg(any(test, feature = "test-support"))]
        if self.fail_durable_writes_for_test {
            return Err("injected durable output write failure".into());
        }
        if let Some(owner) = self.owner_session_id.as_deref() {
            #[cfg(any(test, feature = "test-support"))]
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
        #[cfg(any(test, feature = "test-support"))]
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
        #[cfg(any(test, feature = "test-support"))]
        let durable = if let Some(root) = self.durable_root_override.as_deref() {
            durable_read_at(root, tool_use_id, self.owner_session_id.as_deref())
        } else {
            durable_read_at(
                &durable_root().ok_or("durable stash unavailable")?,
                tool_use_id,
                self.owner_session_id.as_deref(),
            )
        };
        #[cfg(not(any(test, feature = "test-support")))]
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

/// Format the canonical, parseable transcript header for an output-stash stub.
///
/// Attribute values use backslash escapes so quote and backslash characters in
/// tool metadata cannot terminate a quoted field or otherwise make the header
/// ambiguous to transcript consumers.
pub(crate) fn format_output_stash_header(
    tool_use_id: &str,
    tool_name: &str,
    reason: &str,
    full_chars: usize,
    preview_chars: usize,
) -> String {
    format!(
        "[djinn-output-stash tool_use_id=\"{}\" tool_name=\"{}\" reason=\"{}\" full_chars=\"{full_chars}\" preview_chars=\"{preview_chars}\"]",
        escape_stash_header_value(tool_use_id),
        escape_stash_header_value(tool_name),
        escape_stash_header_value(reason),
    )
}

fn escape_stash_header_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(character),
        }
    }
    escaped
}

/// Render a successful tool result to the text handed back to the model.
///
/// Serializes `value` to text (raw string, or pretty JSON), and when that
/// exceeds [`MAX_TOOL_RESULT_CHARS`] stashes the full output under
/// `tool_use_id` and returns a `smart_truncate`d view with a typed synopsis
/// (for JSON/code/text payloads) and an `output_view`/`output_grep`
/// navigation hint appended.
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
            .unwrap()
            .insert(tool_use_id.to_string(), tool_name.to_string(), stash_text)
            .is_err()
        {
            return text;
        }
        let full_bytes = text.len();
        let full_chars = text.chars().count();
        // Generate the synopsis from the full text (before truncation) so
        // JSON/code/text classifiers see the complete payload. Only reduce the
        // smart_truncate budget when a synopsis will actually be appended, so
        // binary/undetectable payloads that return `None` preserve the previous
        // byte-for-byte truncated-stub surface.
        let synopsis_budget = 1_200;
        let synopsis = synopsis::synopsize(tool_name, &text, synopsis_budget);
        let text_budget = match &synopsis {
            Some(_) => MAX_TOOL_RESULT_CHARS.saturating_sub(synopsis_budget),
            None => MAX_TOOL_RESULT_CHARS,
        };
        text = crate::truncate::smart_truncate(&text, text_budget);
        let preview_chars = text.chars().count();
        let header = format_output_stash_header(
            tool_use_id,
            tool_name,
            "single_threshold",
            full_chars,
            preview_chars,
        );
        if let Some(synopsis) = synopsis {
            text.push_str("\n\nTool result synopsis:\n");
            text.push_str(&synopsis);
        }
        text.push_str(&format!(
            "\n\n[Full output stashed ({full_bytes} bytes). Use output_view(tool_use_id=\"{tool_use_id}\") to paginate or output_grep(tool_use_id=\"{tool_use_id}\", pattern=\"...\") to search.]"
        ));
        text = format!("{header}\n{text}");
    }
    text
}

/// Externalize an already-rendered tool-result string using the canonical
/// `[djinn-output-stash ...]` stub contract with `reason="turn_budget"`.
///
/// Unlike [`render_tool_result`], which renders from a raw JSON `value`, this
/// helper takes the *already-rendered text* (e.g. the inline result produced by
/// a prior `render_result` call) and replaces it inline with a smaller stash
/// stub when the rendered text exceeds `preview_chars`. This is the seam the
/// per-turn inline-budget post-pass consumes: it re-externalizes the largest
/// results of a parallel batch *after* they have been rendered, keeping
/// `tool_use_id` / `tool_name` / recovery metadata intact.
///
/// Behaviour:
/// * The complete original `rendered` text is preserved in `OutputStash` under
///   `tool_use_id` so `output_view` / `output_grep` can recover it in full.
/// * The inline body is `smart_truncate(rendered, preview_chars)`.
/// * The canonical header records `full_chars` and `preview_chars` as
///   **character** counts (not bytes).
/// * **Non-shrinking guard:** if the generated stub would not be smaller than
///   `rendered`, the original text is returned unchanged and no stash insertion
///   or replacement occurs. This prevents replacing useful text with a stub that
///   is longer or equal.
///
/// `preview_chars` is clamped to a minimum of `1` so a caller cannot request a
/// degenerate zero-width preview.
pub fn externalize_rendered_tool_result(
    stash: &Mutex<OutputStash>,
    tool_use_id: &str,
    tool_name: &str,
    rendered: &str,
    preview_chars: usize,
) -> String {
    let full_chars = rendered.chars().count();
    let preview_chars = preview_chars.max(1);

    // Build the preview body from the already-rendered text.
    let preview_body = crate::truncate::smart_truncate(rendered, preview_chars);
    let preview_body_chars = preview_body.chars().count();

    // Canonical parseable header. Character counts, not bytes.
    let header = format_output_stash_header(
        tool_use_id,
        tool_name,
        "turn_budget",
        full_chars,
        preview_body_chars,
    );

    let recovery_hint = format!(
        "\n\n[Full output stashed ({full_chars} chars). Use output_view(tool_use_id=\"{tool_use_id}\") to paginate or output_grep(tool_use_id=\"{tool_use_id}\", pattern=\"...\") to search.]"
    );

    let stub = format!("{header}\n{preview_body}{recovery_hint}");

    // Non-shrinking guard: only replace when the stub is strictly smaller.
    if stub.chars().count() >= full_chars {
        return rendered.to_string();
    }

    // Preserve the full rendered text for output_view / output_grep recovery.
    if stash
        .lock()
        .unwrap()
        .insert(
            tool_use_id.to_string(),
            tool_name.to_string(),
            rendered.to_string(),
        )
        .is_err()
    {
        return rendered.to_string();
    }

    stub
}

/// `true` for the stash-navigation tools handled in-process against the
/// [`OutputStash`] rather than dispatched to a real handler.
pub fn is_stash_tool(name: &str) -> bool {
    matches!(name, "output_view" | "output_grep" | "output_list")
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
    let guard = stash.lock().unwrap();
    match name {
        "output_list" => serde_json::to_string_pretty(&guard.list_durable_outputs()?)
            .map_err(|e| format!("serialize durable output list: {e}")),
        "output_view" => {
            let tid = args
                .and_then(|m| m.get("tool_use_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
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
            let tid = args
                .and_then(|m| m.get("tool_use_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
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
mod tests;
#[cfg(test)]
mod tests_synopsis;
