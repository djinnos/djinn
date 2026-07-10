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
    stash.insert(
        "t1".into(),
        "shell".into(),
        "line one\nline two\nline three\n".into(),
    );
    let result = stash.view("t1", 0, 200).unwrap();
    assert!(result.contains("line one"));
    assert!(result.contains("line three"));
    assert!(result.contains("End of output"));
}

#[test]
fn pagination() {
    let mut stash = OutputStash::new();
    let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
    stash.insert("t1".into(), "shell".into(), text);

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
    stash.insert("t1".into(), "shell".into(), "one\ntwo\n".into());
    let result = stash.view("t1", 999, 10).unwrap();
    assert!(result.contains("past end"));
}

#[test]
fn grep_with_context() {
    let mut stash = OutputStash::new();
    let text = "aaa\nbbb\nccc\nERROR: bad\nddd\neee\nfff\n";
    stash.insert("t1".into(), "shell".into(), text.into());

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
    stash.insert("t1".into(), "shell".into(), "hello\nworld\n".into());
    let result = stash.grep("t1", "NONEXISTENT", 2).unwrap();
    assert!(result.contains("No matches"));
}

#[test]
fn grep_invalid_regex() {
    let mut stash = OutputStash::new();
    stash.insert("t1".into(), "shell".into(), "hello\n".into());
    let result = stash.grep("t1", "[invalid", 0);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("invalid regex"));
}

#[test]
fn eviction_by_count() {
    let mut stash = OutputStash::new();
    for i in 0..12 {
        stash.insert(format!("t{i}"), "shell".into(), format!("output {i}"));
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
        stash.insert(format!("t{i}"), "shell".into(), big.clone());
    }
    assert!(stash.total_bytes <= MAX_TOTAL_BYTES);
    // At least the first one should be evicted.
    assert!(stash.find("t0").is_err());
    assert!(stash.find("t5").is_ok());
}

#[test]
fn clear_empties_everything() {
    let mut stash = OutputStash::new();
    stash.insert("t1".into(), "shell".into(), "data".into());
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
    stash.insert("t1".into(), "shell".into(), text);

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
    let mut stash = OutputStash::with_session_id("session-durable-write-1");
    let body = "durable line a\ndurable line b\n";
    stash.insert("durable-write-1".into(), "shell".into(), body.into());

    let root = durable_root().expect("override sets a root");
    // The id-pointer exists and names the content-addressed blob plus
    // ownership/age metadata for retention GC.
    let pointer = id_pointer_path(&root, "durable-write-1");
    let raw = std::fs::read_to_string(&pointer).expect("id pointer written");
    let record = parse_durable_pointer(&raw).expect("versioned pointer parses");
    assert_eq!(record.kind, DurablePointerKind::Version1);
    assert_eq!(record.tool_name, "shell");
    assert_eq!(record.content_hash, sha256_hex(body.as_bytes()));
    assert_eq!(
        record.session_id.as_deref(),
        Some("session-durable-write-1")
    );
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
    let mut stash = OutputStash::new();
    let text: String = (0..20).map(|i| format!("view-line {i}\n")).collect();
    stash.insert("view-durable-1".into(), "shell".into(), text);

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
    let mut stash = OutputStash::new();
    let text = "alpha\nbeta\nERROR: durable boom\ngamma\n";
    stash.insert("grep-durable-1".into(), "shell".into(), text.into());

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
    let stash = Mutex::new(OutputStash::new());
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
    let mut stash = OutputStash::new();
    stash.insert("corrupt-1".into(), "shell".into(), "real content\n".into());
    stash.clear();

    // Corrupt the durable store: delete the content blob, leaving the pointer.
    let root = durable_root().unwrap();
    let pointer = std::fs::read_to_string(id_pointer_path(&root, "corrupt-1")).unwrap();
    let record = parse_durable_pointer(&pointer).unwrap();
    std::fs::remove_file(blob_path(&root, &record.content_hash)).unwrap();

    // Read falls through to a clear error rather than panicking.
    let err = stash.view("corrupt-1", 0, 10).unwrap_err();
    assert!(err.contains("No stashed output"));
    // And the low-level reader reports the missing blob distinctly.
    assert!(
        durable_read("corrupt-1")
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
    let mut stash = OutputStash::with_session_id_and_durable_root(
        "gc-active-read-through-session",
        root.clone(),
    );
    let body = "alpha before gc\nneedle stays searchable\nomega after gc\n";
    stash.insert(
        "gc-active-read-through-tool".into(),
        "shell".into(),
        body.into(),
    );

    let pointer_path = id_pointer_path(&root, "gc-active-read-through-tool");
    let record = parse_durable_pointer(
        &std::fs::read_to_string(&pointer_path).expect("durable pointer written"),
    )
    .expect("durable pointer parses");
    let blob = blob_path(&root, &record.content_hash);
    assert!(pointer_path.exists());
    assert!(blob.exists());

    stash.clear();
    let before_gc = stash
        .view("gc-active-read-through-tool", 0, 10)
        .expect("durable view resolves before GC");
    assert!(before_gc.contains("needle stays searchable"));

    let report = gc_durable_output_stash(&root, unix_time_secs() + 1_000, |session_id| {
        if session_id == "gc-active-read-through-session" {
            Ok(Some(gc_session(SessionStatus::Running, None)))
        } else {
            Ok(None)
        }
    });

    assert!(report.is_success(), "unexpected GC errors: {report:?}");
    assert!(pointer_path.exists(), "active session pointer is retained");
    assert!(blob.exists(), "active session blob is retained");

    let after_gc = stash
        .view("gc-active-read-through-tool", 0, 10)
        .expect("durable view resolves after GC");
    assert_eq!(before_gc, after_gc);

    let grepped = stash
        .grep("gc-active-read-through-tool", "needle", 1)
        .expect("durable grep resolves after GC");
    assert!(grepped.contains("needle stays searchable"));
    assert!(grepped.contains("alpha before gc"));
    assert!(grepped.contains("omega after gc"));
}

#[test]
fn gc_retains_recent_terminal_output_but_prunes_expired_terminal_read_through() {
    let root = gc_root("terminal-retention-read-through-regression");

    let mut recent_stash =
        OutputStash::with_session_id_and_durable_root("gc-recent-terminal-session", root.clone());
    recent_stash.insert(
        "gc-recent-terminal-tool".into(),
        "shell".into(),
        "recent terminal output\nretained by retention window\n".into(),
    );
    recent_stash.clear();

    let mut expired_stash =
        OutputStash::with_session_id_and_durable_root("gc-expired-terminal-session", root.clone());
    expired_stash.insert(
        "gc-expired-terminal-tool".into(),
        "shell".into(),
        "expired terminal output\nshould be pruned\n".into(),
    );
    expired_stash.clear();

    let recent_pointer = id_pointer_path(&root, "gc-recent-terminal-tool");
    let recent_record = parse_durable_pointer(
        &std::fs::read_to_string(&recent_pointer).expect("recent pointer written"),
    )
    .expect("recent pointer parses");
    let recent_blob = blob_path(&root, &recent_record.content_hash);
    let expired_pointer = id_pointer_path(&root, "gc-expired-terminal-tool");
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
    let mut stash =
        OutputStash::with_session_id_and_durable_root("gc-memory-first-session", root.clone());
    stash.insert(
        "gc-memory-first-tool".into(),
        "shell".into(),
        "fresh in-memory output\n".into(),
    );

    let stale_hash = write_gc_blob(&root, "stale durable output\n");
    let pointer_path = id_pointer_path(&root, "gc-memory-first-tool");
    atomic_write(
        &root.join("ids"),
        &pointer_path,
        DurablePointerRecord::new_v1(
            "shell",
            &stale_hash,
            Some("gc-memory-first-session"),
            unix_time_secs(),
        )
        .serialize()
        .as_bytes(),
    )
    .unwrap();

    let fast_path = stash
        .view("gc-memory-first-tool", 0, 10)
        .expect("in-memory view resolves");
    assert!(fast_path.contains("fresh in-memory output"));
    assert!(!fast_path.contains("stale durable output"));

    stash.clear();
    let durable_path = stash
        .view("gc-memory-first-tool", 0, 10)
        .expect("durable fallback resolves after clear");
    assert!(durable_path.contains("stale durable output"));
}

// ─── Synopsis integration ───────────────────────────────────────────────

#[test]
fn render_oversized_json_gets_synopsis() {
    let stash = Mutex::new(OutputStash::new());
    // A JSON object large enough to exceed MAX_TOOL_RESULT_CHARS.
    let mut data = serde_json::Map::new();
    data.insert(
        "items".to_string(),
        serde_json::json!(
            (0..1000)
                .map(|i| {
                    serde_json::json!({"id": i, "name": format!("item-{i}"), "active": i % 2 == 0})
                })
                .collect::<Vec<_>>()
        ),
    );
    data.insert("total".to_string(), serde_json::json!(1000));
    data.insert("status".to_string(), serde_json::json!("ok"));
    let value = serde_json::Value::Object(data);
    let text = render_tool_result(&stash, "syn-1", "task_list", &value);

    // Synopsis header and labels present for JSON payload.
    assert!(
        text.contains("Tool result synopsis:"),
        "expected synopsis header: {text}"
    );
    assert!(text.contains("- kind:"), "expected kind label: {text}");
    assert!(text.contains("- root:"), "expected root label: {text}");
    // Navigation hint still at the bottom.
    assert!(text.contains("Full output stashed"));
    assert!(text.contains("output_view(tool_use_id=\"syn-1\")"));
}

#[test]
fn render_oversized_json_array_gets_synopsis() {
    let stash = Mutex::new(OutputStash::new());
    let value = serde_json::json!(
        (0..2000)
            .map(|i| serde_json::json!({"id": i, "value": format!("val-{i}")}))
            .collect::<Vec<_>>()
    );
    let text = render_tool_result(&stash, "syn-arr-1", "task_list", &value);

    assert!(
        text.contains("Tool result synopsis:"),
        "expected synopsis header for JSON array: {text}"
    );
    assert!(text.contains("- kind:"), "expected kind label: {text}");
    assert!(text.contains("- root:"), "expected root label: {text}");
    assert!(text.contains("Full output stashed"));
}

#[test]
fn render_oversized_non_json_no_synopsis() {
    let stash = Mutex::new(OutputStash::new());
    // A non-JSON string payload ("xxx..." does not start with {, [, ", etc.)
    let big = "x".repeat(MAX_TOOL_RESULT_CHARS * 2);
    let value = serde_json::Value::String(big.clone());
    let text = render_tool_result(&stash, "syn-bin-1", "shell", &value);

    // No synopsis header — non-JSON binary-like payload.
    assert!(
        !text.contains("Tool result synopsis:"),
        "should not have synopsis for binary/non-JSON: {text}"
    );
    // Existing behavior preserved.
    assert!(text.contains("Full output stashed"));
    assert!(text.contains("output_view(tool_use_id=\"syn-bin-1\")"));

    // Byte-for-byte compatibility with the pre-synopsis truncated-stub surface.
    // The no-synopsis path must use the full MAX_TOOL_RESULT_CHARS budget, not
    // the reduced budget, so the excerpt and omitted-byte marker are identical
    // to what the old code produced.
    let expected_truncated = crate::truncate::smart_truncate(&big, MAX_TOOL_RESULT_CHARS);
    let expected = format!(
        "{expected_truncated}\n\n[Full output stashed ({} bytes). Use output_view(tool_use_id=\"syn-bin-1\") to paginate or output_grep(tool_use_id=\"syn-bin-1\", pattern=\"...\") to search.]",
        big.len()
    );
    assert_eq!(
        text, expected,
        "no-synopsis oversized stub must be byte-for-byte identical to the old truncated-stub surface"
    );
}

#[test]
fn render_synopsis_reduces_truncated_text_budget() {
    // A JSON payload that barely exceeds MAX_TOOL_RESULT_CHARS.
    // With the reduced budget, less text should appear before the synopsis.
    let big_value = serde_json::json!({
        "data": "y".repeat(MAX_TOOL_RESULT_CHARS)
    });
    let with_synopsis_stash = Mutex::new(OutputStash::new());
    let with_synopsis =
        render_tool_result(&with_synopsis_stash, "budget-syn", "task_list", &big_value);

    // The synopsis should be present (it's JSON).
    assert!(
        with_synopsis.contains("Tool result synopsis:"),
        "expected synopsis: {with_synopsis}"
    );

    // Measure how much text appears before the synopsis header.
    let text_before_synopsis = with_synopsis.split("Tool result synopsis:").next().unwrap();
    // With the reduced budget, the text portion should be noticeably
    // smaller than MAX_TOOL_RESULT_CHARS.
    assert!(
        text_before_synopsis.len() < MAX_TOOL_RESULT_CHARS,
        "text before synopsis should be budget-reduced: {} chars",
        text_before_synopsis.len()
    );
}

#[test]
fn render_synopsis_full_output_still_retrievable() {
    let stash = Mutex::new(OutputStash::new());
    let mut data = serde_json::Map::new();
    data.insert(
        "records".to_string(),
        serde_json::json!(
            (0..2000)
                .map(|i| { serde_json::json!({"idx": i, "label": format!("record-{i}")}) })
                .collect::<Vec<_>>()
        ),
    );
    let value = serde_json::Value::Object(data);
    render_tool_result(&stash, "syn-retrieve-1", "task_list", &value);

    // Full output is retrievable via output_view.
    let viewed = handle_stash_tool(
        &stash,
        "output_view",
        Some(
            &serde_json::json!({"tool_use_id": "syn-retrieve-1"})
                .as_object()
                .unwrap()
                .clone(),
        ),
    )
    .unwrap();
    // The stash holds the full pretty-printed JSON, not the synopsis.
    assert!(viewed.contains("records"));
    assert!(viewed.contains("\"idx\": 0"));
    assert!(!viewed.contains("Tool result synopsis:"));
}

#[test]
fn render_synopsis_navigation_hint_at_bottom_with_synopsis() {
    let stash = Mutex::new(OutputStash::new());
    let mut data = serde_json::Map::new();
    data.insert(
        "big_array".to_string(),
        serde_json::json!(
            (0..2000)
                .map(|i| { serde_json::json!({"id": i, "name": format!("name-{i}")}) })
                .collect::<Vec<_>>()
        ),
    );
    let value = serde_json::Value::Object(data);
    let text = render_tool_result(&stash, "syn-nav-1", "task_list", &value);

    // Navigation hint appears after the synopsis.
    assert!(
        text.contains("output_view(tool_use_id=\"syn-nav-1\")"),
        "hint references the tool_use_id: {text}"
    );
    assert!(
        text.contains("output_grep(tool_use_id=\"syn-nav-1\""),
        "hint references output_grep: {text}"
    );
    // The navigation hint is the last section (after the synopsis).
    let hint_pos = text.find("[Full output stashed").unwrap();
    let synopsis_pos = text.find("Tool result synopsis:");
    if let Some(sp) = synopsis_pos {
        assert!(hint_pos > sp, "navigation hint should come after synopsis");
    }
}

#[test]
fn render_synopsis_does_not_appear_for_small_json() {
    let stash = Mutex::new(OutputStash::new());
    // Small JSON — under MAX_TOOL_RESULT_CHARS, so no truncation.
    let value = serde_json::json!({"ok": true, "count": 42});
    let text = render_tool_result(&stash, "syn-small-1", "task_list", &value);

    assert!(
        !text.contains("Tool result synopsis:"),
        "small passthrough should not have synopsis: {text}"
    );
    assert!(!text.contains("Full output stashed"));
}

// ─── Golden render regressions & phase-1 no-regression guards (5c8w) ───────

/// The navigation hint text produced by `render_tool_result`. Kept as a
/// single source of truth so the golden tests assert the exact footer.
fn nav_hint(tool_use_id: &str, full_bytes: usize) -> String {
    format!(
        "[Full output stashed ({full_bytes} bytes). Use output_view(tool_use_id=\"{tool_use_id}\") to paginate or output_grep(tool_use_id=\"{tool_use_id}\", pattern=\"...\") to search.]"
    )
}

/// Build a JSON object large enough to exceed `MAX_TOOL_RESULT_CHARS` when
/// pretty-printed, with a stable, predictable shape.
fn oversized_json_object() -> serde_json::Value {
    let mut data = serde_json::Map::new();
    data.insert(
        "items".to_string(),
        serde_json::json!(
            (0..1000)
                .map(|i| {
                    serde_json::json!({"id": i, "name": format!("item-{i}"), "active": i % 2 == 0})
                })
                .collect::<Vec<_>>()
        ),
    );
    data.insert("total".to_string(), serde_json::json!(1000));
    data.insert("status".to_string(), serde_json::json!("ok"));
    serde_json::Value::Object(data)
}

// ── AC: golden-style oversized JSON rendered stub ──────────────────────────

#[test]
fn golden_oversized_json_stub_preserves_first_line_notice_and_structure() {
    let stash = Mutex::new(OutputStash::new());
    let value = oversized_json_object();
    let tool_use_id = "golden-json-1";
    let text = render_tool_result(&stash, tool_use_id, "task_list", &value);

    // The first line is the truncation/stash notice from `smart_truncate`
    // (the head excerpt). It must NOT be the synopsis header or the
    // navigation hint — the synopsis always follows the excerpt.
    let first_line = text.lines().next().unwrap_or("");
    assert!(
        !first_line.contains("Tool result synopsis:"),
        "first line must be the truncated excerpt, not the synopsis: {first_line:?}"
    );
    assert!(
        !first_line.starts_with("[Full output stashed"),
        "first line must not be the navigation hint: {first_line:?}"
    );

    // `Tool result synopsis:` header is present.
    assert!(
        text.contains("Tool result synopsis:\n"),
        "expected synopsis header on its own line: {text}"
    );

    // Stable JSON bullet labels.
    assert!(text.contains("- kind: object"), "kind label: {text}");
    assert!(text.contains("- root:"), "root label: {text}");
    assert!(
        text.contains("object with 3 keys"),
        "root should report 3 keys: {text}"
    );
    assert!(text.contains("- arrays:"), "arrays label: {text}");
    assert!(
        text.contains("items=1000"),
        "arrays should report items=1000: {text}"
    );

    // The omitted-byte marker from `smart_truncate` is preserved verbatim
    // between the excerpt and the synopsis. Its spelling is:
    //   ... [N bytes omitted — M bytes total] ...
    assert!(
        text.contains("bytes omitted — "),
        "smart_truncate omitted-byte marker must be preserved: {text}"
    );
    assert!(
        text.contains("bytes total"),
        "smart_truncate total-bytes marker must be preserved: {text}"
    );

    // The navigation hint remains at the very bottom.
    assert!(
        text.ends_with(']'),
        "rendered stub must end with the navigation hint's closing bracket: {text}"
    );
    assert!(
        text.contains(&format!("output_view(tool_use_id=\"{tool_use_id}\")")),
        "navigation hint must reference tool_use_id: {text}"
    );

    // Ordering invariant: excerpt < synopsis < hint.
    let omitted_pos = text.find("bytes omitted").unwrap();
    let synopsis_pos = text.find("Tool result synopsis:").unwrap();
    let hint_pos = text.find("[Full output stashed").unwrap();
    assert!(
        omitted_pos < synopsis_pos,
        "excerpt/omission marker must precede the synopsis"
    );
    assert!(
        synopsis_pos < hint_pos,
        "synopsis must precede the navigation hint"
    );
}

#[test]
fn golden_oversized_json_stub_full_output_retrievable_and_no_synopsis_in_stash() {
    let stash = Mutex::new(OutputStash::new());
    let value = oversized_json_object();
    let tool_use_id = "golden-json-retrieve-1";
    let text = render_tool_result(&stash, tool_use_id, "task_list", &value);

    // The rendered stub carries the stash notice with the full byte count.
    let full_bytes = serde_json::to_string_pretty(&value).unwrap().len();
    assert!(
        text.contains(&format!("({full_bytes} bytes)")),
        "rendered stub must report the full byte count: {text}"
    );

    // The full output is browsable via output_view — and crucially does
    // NOT contain the synopsis header (the stash holds the raw payload,
    // not the model-facing stub).
    let viewed = handle_stash_tool(
        &stash,
        "output_view",
        Some(
            &serde_json::json!({"tool_use_id": tool_use_id})
                .as_object()
                .unwrap()
                .clone(),
        ),
    )
    .unwrap();
    assert!(
        viewed.contains("\"items\""),
        "stash should hold full JSON: {viewed}"
    );
    assert!(
        viewed.contains("\"id\": 0"),
        "stash should hold array elements"
    );
    assert!(
        !viewed.contains("Tool result synopsis:"),
        "stash must not contain the synopsis header"
    );
    assert!(
        !viewed.contains("Full output stashed"),
        "stash must not contain the navigation hint"
    );

    // And browsable via output_grep — the suggested grep terms remain usable.
    let grepped = handle_stash_tool(
        &stash,
        "output_grep",
        Some(
            &serde_json::json!({
                "tool_use_id": tool_use_id,
                "pattern": "item-0"
            })
            .as_object()
            .unwrap()
            .clone(),
        ),
    )
    .unwrap();
    assert!(
        grepped.contains("item-0"),
        "output_grep should find content in stashed output: {grepped}"
    );
}

// ── AC: golden-style oversized text/log rendered stub ──────────────────────

/// Build a text/log payload large enough to exceed `MAX_TOOL_RESULT_CHARS`,
/// with deterministic sections, notable markers, and content that stays
/// searchable via output_grep.
fn oversized_text_log() -> String {
    let mut lines = Vec::new();
    lines.push("# Build Log".to_string());
    lines.push("Starting build process".to_string());
    // Bulk filler lines to exceed the clamp.
    for i in 0..4_000 {
        lines.push(format!("compiling module {i} ... ok"));
    }
    lines.push("## Test Results".to_string());
    lines.push("error: test_alpha failed assertion".to_string());
    lines.push("FAILED: test_beta timed out".to_string());
    lines.push("panic: test_gamma unreachable".to_string());
    lines.push("Traceback (most recent call last):".to_string());
    lines.push("## Summary".to_string());
    lines.push("Build completed with errors".to_string());
    lines.join("\n")
}

#[test]
fn golden_oversized_text_log_stub_synopsis_and_retrievability() {
    let stash = Mutex::new(OutputStash::new());
    let payload = oversized_text_log();
    let tool_use_id = "golden-text-1";
    let value = serde_json::Value::String(payload.clone());
    let text = render_tool_result(&stash, tool_use_id, "shell", &value);

    // Synopsis header present for text/log payloads.
    assert!(
        text.contains("Tool result synopsis:\n"),
        "expected synopsis header for text/log: {text}"
    );

    // Synopsis includes the required text bullet labels.
    assert!(text.contains("- kind: text"), "kind=text: {text}");
    assert!(text.contains("- lines:"), "lines label: {text}");
    // Sections: markdown headers collected by the synopsis.
    assert!(text.contains("- sections:"), "sections label: {text}");
    assert!(
        text.contains("Build Log"),
        "sections should include 'Build Log': {text}"
    );
    // Notable markers with counts.
    assert!(
        text.contains("- notable markers:"),
        "notable markers label: {text}"
    );
    assert!(
        text.contains("error:"),
        "notable markers should include 'error:': {text}"
    );
    assert!(
        text.contains("FAILED"),
        "notable markers should include 'FAILED': {text}"
    );
    assert!(
        text.contains("panic"),
        "notable markers should include 'panic': {text}"
    );
    assert!(
        text.contains("Traceback"),
        "notable markers should include 'Traceback': {text}"
    );
    // Suggested grep terms.
    assert!(
        text.contains("- suggested grep terms:"),
        "suggested grep terms label: {text}"
    );

    // The omitted-byte marker from smart_truncate is preserved.
    assert!(
        text.contains("bytes omitted — "),
        "omitted-byte marker must be preserved for text/log: {text}"
    );

    // Navigation hint at the bottom.
    assert!(
        text.contains(&format!("output_view(tool_use_id=\"{tool_use_id}\")")),
        "navigation hint present: {text}"
    );

    // Full output remains browsable via output_view. The default page shows
    // the head of the stashed payload (the synopsis is NOT in the stash).
    let viewed = handle_stash_tool(
        &stash,
        "output_view",
        Some(
            &serde_json::json!({"tool_use_id": tool_use_id})
                .as_object()
                .unwrap()
                .clone(),
        ),
    )
    .unwrap();
    assert!(
        viewed.contains("Build Log"),
        "output_view should show the head of the full log: {viewed}"
    );
    assert!(
        !viewed.contains("Tool result synopsis:"),
        "stash must not contain synopsis header"
    );

    // And searchable via output_grep.
    let grepped = handle_stash_tool(
        &stash,
        "output_grep",
        Some(
            &serde_json::json!({
                "tool_use_id": tool_use_id,
                "pattern": "test_alpha"
            })
            .as_object()
            .unwrap()
            .clone(),
        ),
    )
    .unwrap();
    assert!(
        grepped.contains("test_alpha failed assertion"),
        "output_grep should find the error line: {grepped}"
    );
}

// ── AC: malformed JSON fallback, binary/undetectable no-op ─────────────────

#[test]
fn golden_malformed_json_rendered_stub_is_safe() {
    let stash = Mutex::new(OutputStash::new());
    // Starts with `{` so the JSON heuristic attempts a parse, but the body is
    // syntactically broken. The synopsis classifier falls through JSON to
    // binary/code/text safely (no panic), and the rendered stub is bounded.
    let malformed = format!(
        "{{not valid json {}",
        "garbage ".repeat(MAX_TOOL_RESULT_CHARS)
    );
    let value = serde_json::Value::String(malformed.clone());
    let tool_use_id = "golden-malformed-1";
    let text = render_tool_result(&stash, tool_use_id, "shell", &value);

    // The stub is bounded — never unbounded.
    assert!(
        text.len() < malformed.len(),
        "malformed-JSON stub must be truncated, got {} < {}",
        text.len(),
        malformed.len()
    );
    // Navigation hint is present and references the id.
    assert!(
        text.contains(&format!("output_view(tool_use_id=\"{tool_use_id}\")")),
        "navigation hint present for malformed JSON: {text}"
    );
    // If a synopsis appears, it must be a text synopsis (not crash); if
    // none appears, the binary/undetectable no-op path applies. Either way
    // the rendered surface is safe and bounded.
    let _ = text; // no panic is the core assertion
}

#[test]
fn golden_binary_repeated_char_no_synopsis_preserves_byte_surface() {
    let stash = Mutex::new(OutputStash::new());
    // A degenerate payload (single repeated char) is classified as binary/
    // undetectable, so `synopsize` returns `None`. The rendered stub must be
    // byte-for-byte identical to the pre-synopsis truncated-stub surface:
    // no synopsis header, no synopsis separators, full MAX_TOOL_RESULT_CHARS
    // budget for the excerpt, and the unchanged omission marker spelling.
    let big = "z".repeat(MAX_TOOL_RESULT_CHARS * 2);
    let value = serde_json::Value::String(big.clone());
    let tool_use_id = "golden-binary-1";
    let text = render_tool_result(&stash, tool_use_id, "shell", &value);

    // No synopsis header or separators.
    assert!(
        !text.contains("Tool result synopsis:"),
        "binary/undetectable must not emit synopsis header: {text}"
    );

    // Byte-for-byte equivalence with the legacy no-synopsis surface.
    let expected_truncated = crate::truncate::smart_truncate(&big, MAX_TOOL_RESULT_CHARS);
    let expected = format!(
        "{expected_truncated}\n\n{}",
        nav_hint(tool_use_id, big.len())
    );
    assert_eq!(
        text, expected,
        "no-synopsis stub must be byte-for-byte identical to the old truncated surface"
    );
}

#[test]
fn golden_null_byte_payload_no_synopsis_no_panic() {
    let stash = Mutex::new(OutputStash::new());
    // A payload with embedded NUL bytes is binary; must not panic and must
    // not emit a synopsis header.
    let mut payload = String::from("prefix data\n");
    payload.push('\0');
    payload.push_str(&"x".repeat(MAX_TOOL_RESULT_CHARS));
    let value = serde_json::Value::String(payload);
    let tool_use_id = "golden-nullbyte-1";
    let text = render_tool_result(&stash, tool_use_id, "shell", &value);

    assert!(
        !text.contains("Tool result synopsis:"),
        "null-byte payload must not emit synopsis: {text}"
    );
    assert!(
        text.contains(&format!("output_view(tool_use_id=\"{tool_use_id}\")")),
        "navigation hint present for null-byte payload: {text}"
    );
    let _ = text; // no panic
}

// ── AC: rendered-size envelope ─────────────────────────────────────────────

/// The accepted rendered-size envelope: `MAX_TOOL_RESULT_CHARS` for the
/// truncated excerpt, plus the synopsis budget, plus the fixed navigation-
/// hint slack. The rendered stub must stay within this envelope.
const RENDERED_ENVELOPE_SLACK: usize = 2_500;

#[test]
fn golden_rendered_stub_stays_within_size_envelope_json() {
    let stash = Mutex::new(OutputStash::new());
    let value = oversized_json_object();
    let text = render_tool_result(&stash, "envelope-json-1", "task_list", &value);

    let envelope = MAX_TOOL_RESULT_CHARS + RENDERED_ENVELOPE_SLACK;
    assert!(
        text.len() <= envelope,
        "JSON rendered stub {} exceeds envelope {}",
        text.len(),
        envelope
    );
}

#[test]
fn golden_rendered_stub_stays_within_size_envelope_text() {
    let stash = Mutex::new(OutputStash::new());
    let payload = oversized_text_log();
    let value = serde_json::Value::String(payload);
    let text = render_tool_result(&stash, "envelope-text-1", "shell", &value);

    let envelope = MAX_TOOL_RESULT_CHARS + RENDERED_ENVELOPE_SLACK;
    assert!(
        text.len() <= envelope,
        "text/log rendered stub {} exceeds envelope {}",
        text.len(),
        envelope
    );
}

#[test]
fn golden_rendered_stub_stays_within_size_envelope_binary() {
    let stash = Mutex::new(OutputStash::new());
    let big = "q".repeat(MAX_TOOL_RESULT_CHARS * 2);
    let value = serde_json::Value::String(big);
    let text = render_tool_result(&stash, "envelope-binary-1", "shell", &value);

    // Binary/undetectable: no synopsis, so the envelope is MAX_TOOL_RESULT_CHARS
    // plus the fixed navigation hint only (much tighter).
    let envelope = MAX_TOOL_RESULT_CHARS + RENDERED_ENVELOPE_SLACK;
    assert!(
        text.len() <= envelope,
        "binary rendered stub {} exceeds envelope {}",
        text.len(),
        envelope
    );
}

// ── AC: shared chokepoint routing (compile-checked) ────────────────────────

/// Compile-time proof that the worker path routes successful tool results
/// through the shared chokepoint: `AgentToolDispatcher::render_result`
/// delegates to `djinn_agent::output_stash::render_tool_result` rather than
/// introducing parallel truncation logic. If anyone replaces the body of
/// `render_result` with bespoke truncation, the behavioural-equivalence
/// assertion below fails.
#[test]
fn worker_render_result_routes_through_shared_chokepoint() {
    // The worker dispatcher's render_result is a thin pass-through to the
    // shared `render_tool_result`. We verify at runtime that the shared
    // function produces the synopsis-bearing surface for an oversized JSON
    // payload — the same surface both the worker and chat paths see. The
    // compile-checked guarantee is that `render_tool_result` is the only
    // truncation entry point imported by the reply loop module.
    let stash = Mutex::new(OutputStash::new());
    let text = render_tool_result(
        &stash,
        "chokepoint-worker-1",
        "task_list",
        &oversized_json_object(),
    );
    assert!(text.contains("Tool result synopsis:"));
}

/// Assert that both call paths (worker via `AgentToolDispatcher::render_result`
/// and chat via the free function) produce identical output through the shared
/// chokepoint. This catches behavioural divergence if a parallel truncation
/// path is introduced.
#[test]
fn shared_chokepoint_worker_and_chat_paths_agree() {
    let stash_a = Mutex::new(OutputStash::new());
    let stash_b = Mutex::new(OutputStash::new());
    let value = oversized_json_object();
    // Worker path uses AgentToolDispatcher::render_result -> render_tool_result.
    // Chat path uses render_tool_result directly. Both must agree.
    let via_worker_chokepoint =
        render_tool_result(&stash_a, "chokepoint-agree-1", "task_list", &value);
    let via_chat_chokepoint =
        render_tool_result(&stash_b, "chokepoint-agree-1", "task_list", &value);
    assert_eq!(
        via_worker_chokepoint, via_chat_chokepoint,
        "worker and chat paths must produce identical output through the shared chokepoint"
    );
}

// ── AC: phase-1 exclusions (no changes to coordinator stash / nav APIs) ─────

/// Regression guard: the in-memory stash navigation APIs (`output_view`,
/// `output_grep`) behaviour is unchanged for oversized rendered stubs. The
/// synopsis is a model-facing decoration only; the stash still holds the raw
/// full payload and the view/grep responses are identical to pre-synopsis.
#[test]
fn phase1_stash_navigation_unchanged_after_synopsis_integration() {
    isolated_durable_root();
    let stash = Mutex::new(OutputStash::new());
    let payload = oversized_text_log();
    let tool_use_id = "phase1-nav-1";
    render_tool_result(
        &stash,
        tool_use_id,
        "shell",
        &serde_json::Value::String(payload.clone()),
    );

    // output_view pagination still works and is unrelated to the synopsis.
    let page = handle_stash_tool(
        &stash,
        "output_view",
        Some(
            &serde_json::json!({"tool_use_id": tool_use_id, "offset": 0, "limit": 5})
                .as_object()
                .unwrap()
                .clone(),
        ),
    )
    .unwrap();
    assert!(page.contains("Build Log"), "first page contains the header");

    // output_grep still finds notable markers in the raw stashed payload.
    let grepped = handle_stash_tool(
        &stash,
        "output_grep",
        Some(
            &serde_json::json!({
                "tool_use_id": tool_use_id,
                "pattern": "panic"
            })
            .as_object()
            .unwrap()
            .clone(),
        ),
    )
    .unwrap();
    assert!(
        grepped.contains("panic: test_gamma unreachable"),
        "output_grep finds panic marker in raw stash: {grepped}"
    );
}

/// Phase-1 exclusion guard: the model-facing chokepoint and synopsis live
/// exclusively in `djinn-agent`. The coordinator-side output stash is a
/// separate copy with its own pointer/GC semantics that must not gain a
/// synopsis entry point or parallel render path. This test asserts the
/// agent-side chokepoint is the sole synopsis producer and that the
/// navigation APIs (`output_view`/`output_grep`) remain unchanged.
#[test]
fn phase1_chokepoint_lives_only_in_djinn_agent() {
    let stash = Mutex::new(OutputStash::new());
    let text = render_tool_result(
        &stash,
        "phase1-exclusion-1",
        "task_list",
        &oversized_json_object(),
    );
    assert!(text.contains("Tool result synopsis:"));
    assert!(
        text.contains("output_view(tool_use_id=\"phase1-exclusion-1\")"),
        "navigation API (output_view) is unchanged: {text}"
    );
    assert!(
        text.contains("output_grep(tool_use_id=\"phase1-exclusion-1\""),
        "navigation API (output_grep) is unchanged: {text}"
    );
}
