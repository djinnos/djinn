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
    assert!(!text.contains("[djinn-output-stash"));
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
    assert!(text.starts_with(
        "[djinn-output-stash tool_use_id=\"call-1\" tool_name=\"shell\" reason=\"single_threshold\" full_chars=\"60000\""
    ));
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
fn render_header_uses_character_counts_and_escapes_metadata() {
    let stash = Mutex::new(OutputStash::new());
    let tool_use_id = "call-\\\"é";
    let tool_name = "tool-\\\"name";
    let full = "é".repeat(MAX_TOOL_RESULT_CHARS + 1);
    let text = render_tool_result(
        &stash,
        tool_use_id,
        tool_name,
        &serde_json::Value::String(full.clone()),
    );
    let header = text.lines().next().expect("canonical header");

    assert!(header.starts_with(
        "[djinn-output-stash tool_use_id=\"call-\\\\\\\"é\" tool_name=\"tool-\\\\\\\"name\" reason=\"single_threshold\""
    ));
    assert!(header.contains(&format!("full_chars=\"{}\"", full.chars().count())));
    assert!(header.contains("preview_chars=\""));
    assert!(text.contains("output_view(tool_use_id=\"call-\\\"é\")"));
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
    stash
        .insert("durable-write-1".into(), "shell".into(), body.into())
        .unwrap();

    let root = durable_root().expect("override sets a root");
    // The id-pointer exists and names the content-addressed blob plus
    // ownership/age metadata for retention GC.
    let pointer = owner_id_pointer_path(&root, "session-durable-write-1", "durable-write-1");
    let raw = std::fs::read_to_string(&pointer).expect("id pointer written");
    let record = parse_durable_pointer(&raw).expect("versioned pointer parses");
    assert_eq!(record.kind, DurablePointerKind::Version2);
    assert_eq!(record.tool_use_id.as_deref(), Some("durable-write-1"));
    assert_eq!(record.turn, Some(0));
    assert_eq!(record.completeness.as_deref(), Some("complete"));
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
    let mut stash = OutputStash::with_session_id("corrupt-durable-session");
    stash
        .insert("corrupt-1".into(), "shell".into(), "real content\n".into())
        .unwrap();
    stash.clear();

    // Corrupt the durable store: delete the content blob, leaving the pointer.
    let root = durable_root().unwrap();
    let pointer = std::fs::read_to_string(owner_id_pointer_path(
        &root,
        "corrupt-durable-session",
        "corrupt-1",
    ))
    .unwrap();
    let record = parse_durable_pointer(&pointer).unwrap();
    std::fs::remove_file(blob_path(&root, &record.content_hash)).unwrap();

    // Read falls through to a clear error rather than panicking.
    let err = stash.view("corrupt-1", 0, 10).unwrap_err();
    assert!(err.contains("No stashed output"));
    // And the low-level reader reports the missing blob distinctly.
    assert!(
        durable_read_at(&root, "corrupt-1", Some("corrupt-durable-session"))
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
    stash
        .insert(
            "gc-active-read-through-tool".into(),
            "shell".into(),
            body.into(),
        )
        .unwrap();

    let pointer_path = owner_id_pointer_path(
        &root,
        "gc-active-read-through-session",
        "gc-active-read-through-tool",
    );
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
    recent_stash
        .insert(
            "gc-recent-terminal-tool".into(),
            "shell".into(),
            "recent terminal output\nretained by retention window\n".into(),
        )
        .unwrap();
    recent_stash.clear();

    let mut expired_stash =
        OutputStash::with_session_id_and_durable_root("gc-expired-terminal-session", root.clone());
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
    let mut stash =
        OutputStash::with_session_id_and_durable_root("gc-memory-first-session", root.clone());
    stash
        .insert(
            "gc-memory-first-tool".into(),
            "shell".into(),
            "fresh in-memory output\n".into(),
        )
        .unwrap();

    let stale_hash = write_gc_blob(&root, "stale durable output\n");
    let pointer_path =
        owner_id_pointer_path(&root, "gc-memory-first-session", "gc-memory-first-tool");
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
        "{}\n{expected_truncated}\n\n[Full output stashed ({} bytes). Use output_view(tool_use_id=\"syn-bin-1\") to paginate or output_grep(tool_use_id=\"syn-bin-1\", pattern=\"...\") to search.]",
        format_output_stash_header(
            "syn-bin-1",
            "shell",
            "single_threshold",
            big.chars().count(),
            expected_truncated.chars().count(),
        ),
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
fn externalize_rendered_tool_result_turn_budget_stashes_and_renders_stub() {
    let stash = Mutex::new(OutputStash::new());
    let rendered = "line 0\n".repeat(500);
    let stub = externalize_rendered_tool_result(&stash, "turn-budget-1", "shell", &rendered, 100);

    // The inline stub is strictly smaller than the original rendered text.
    assert!(stub.chars().count() < rendered.chars().count());
    // Canonical header with reason="turn_budget".
    assert!(stub.starts_with(
        "[djinn-output-stash tool_use_id=\"turn-budget-1\" tool_name=\"shell\" reason=\"turn_budget\""
    ));
    // Recovery hint references the right tool and recovery tools.
    assert!(stub.contains("output_view(tool_use_id=\"turn-budget-1\")"));
    assert!(stub.contains("output_grep(tool_use_id=\"turn-budget-1\""));

    // The complete rendered text was stashed and is recoverable.
    let viewed = handle_stash_tool(
        &stash,
        "output_view",
        Some(
            &serde_json::json!({"tool_use_id": "turn-budget-1", "limit": 500})
                .as_object()
                .unwrap()
                .clone(),
        ),
    )
    .unwrap();
    assert!(viewed.contains("line 0"));
    assert_eq!(viewed.matches("line 0").count(), 500);
}

#[test]
fn externalize_rendered_tool_result_counts_full_and_preview_chars() {
    let stash = Mutex::new(OutputStash::new());
    let rendered = "x".repeat(5_000);
    let preview_budget = 200;
    let stub = externalize_rendered_tool_result(
        &stash,
        "char-count-1",
        "shell",
        &rendered,
        preview_budget,
    );

    let full_chars = rendered.chars().count();
    let preview_body = crate::truncate::smart_truncate(&rendered, preview_budget);
    let preview_body_chars = preview_body.chars().count();

    let header = stub.lines().next().expect("canonical header");
    assert!(header.contains(&format!("full_chars=\"{full_chars}\"")));
    assert!(header.contains(&format!("preview_chars=\"{preview_body_chars}\"")));
    // Counts are character counts, not bytes.
    assert_eq!(full_chars, 5_000);
    assert!(preview_body_chars <= preview_budget);
}

#[test]
fn externalize_rendered_tool_result_escapes_header_values() {
    let stash = Mutex::new(OutputStash::new());
    let tool_use_id = "call-\\\"é";
    let tool_name = "tool-\\\"name";
    let rendered = "é".repeat(2_000);
    let stub = externalize_rendered_tool_result(&stash, tool_use_id, tool_name, &rendered, 200);

    let header = stub.lines().next().expect("canonical header");
    // Escaped quote and backslash so the header stays parseable.
    assert!(header.starts_with(
        "[djinn-output-stash tool_use_id=\"call-\\\\\\\"é\" tool_name=\"tool-\\\\\\\"name\" reason=\"turn_budget\""
    ));
    // Character count is still correct for the multibyte payload.
    assert!(header.contains("full_chars=\"2000\""));
    assert!(header.contains("reason=\"turn_budget\""));
    // preview_chars reports the actual character count of the truncated preview.
    assert!(header.contains("preview_chars=\""));
}

#[test]
fn externalize_rendered_tool_result_non_shrinking_guard_returns_original() {
    let stash = Mutex::new(OutputStash::new());
    let rendered = "small result";
    let output = externalize_rendered_tool_result(&stash, "small-1", "read", rendered, 10);

    // The stub would not be smaller, so the original is returned unchanged.
    assert_eq!(output, rendered);
    // No stash insertion happened.
    assert!(stash.lock().unwrap().view("small-1", 0, 10).is_err());
}

#[test]
fn externalize_rendered_tool_result_preview_size_is_configurable() {
    let stash = Mutex::new(OutputStash::new());
    let rendered = "0123456789".repeat(200);

    let stub_small =
        externalize_rendered_tool_result(&stash, "preview-small-1", "shell", &rendered, 50);
    let stub_large =
        externalize_rendered_tool_result(&stash, "preview-large-1", "shell", &rendered, 500);

    // Larger preview budget → larger inline body and larger reported preview_chars.
    let preview_chars_small = extract_header_preview_chars(stub_small.lines().next().unwrap());
    let preview_chars_large = extract_header_preview_chars(stub_large.lines().next().unwrap());
    assert!(preview_chars_small < preview_chars_large);
    assert!(stub_large.len() > stub_small.len());
}

#[test]
fn externalize_rendered_tool_result_matches_single_threshold_escaping_semantics() {
    let tool_use_id = "call-\\\"é";
    let tool_name = "tool-\\\"name";
    let full = "é".repeat(MAX_TOOL_RESULT_CHARS + 1);

    // Single-threshold path renders from a JSON value.
    let single_stash = Mutex::new(OutputStash::new());
    let single = render_tool_result(
        &single_stash,
        tool_use_id,
        tool_name,
        &serde_json::Value::String(full.clone()),
    );
    let single_header = single.lines().next().unwrap();

    // Turn-budget path re-externalizes the already-rendered text.
    let turn_stash = Mutex::new(OutputStash::new());
    let turn = externalize_rendered_tool_result(
        &turn_stash,
        tool_use_id,
        tool_name,
        &full,
        MAX_TOOL_RESULT_CHARS,
    );
    let turn_header = turn.lines().next().unwrap();

    // Both use the same escaping for tool_use_id / tool_name.
    let expected_prefix = format!(
        "[djinn-output-stash tool_use_id=\"{}\" tool_name=\"{}\"",
        escape_stash_header_value(tool_use_id),
        escape_stash_header_value(tool_name),
    );
    assert!(single_header.starts_with(&expected_prefix));
    assert!(turn_header.starts_with(&expected_prefix));

    // Both report the same full character count.
    let full_chars = full.chars().count();
    assert!(single_header.contains(&format!("full_chars=\"{full_chars}\"")));
    assert!(turn_header.contains(&format!("full_chars=\"{full_chars}\"")));
}

fn extract_header_preview_chars(header: &str) -> usize {
    header
        .split("preview_chars=\"")
        .nth(1)
        .unwrap()
        .split('\"')
        .next()
        .unwrap()
        .parse()
        .unwrap()
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

#[test]
fn durable_metadata_listing_retry_and_session_isolation() {
    let root = gc_root("metadata-listing-contract");
    let details = DurableOutputDetails {
        turn: 7,
        result_kind: "shell_stdout".into(),
        original_chars: 40,
        stored_chars: 4,
        completeness: "partial-spill".into(),
    };
    let mut owner = OutputStash::with_session_id_and_durable_root("owner-a", root.clone());
    owner
        .insert_with_metadata(
            "tool-a".into(),
            "shell".into(),
            "éééé".into(),
            details.clone(),
        )
        .unwrap();
    // A usable identical retry is idempotent.
    owner
        .insert_with_metadata(
            "tool-a".into(),
            "shell".into(),
            "éééé".into(),
            details.clone(),
        )
        .unwrap();
    assert!(
        owner
            .insert_with_metadata(
                "tool-a".into(),
                "shell".into(),
                "éééé".into(),
                DurableOutputDetails {
                    turn: 8,
                    ..details.clone()
                },
            )
            .is_err()
    );

    // Process-style reopen has no in-memory entry but lists and resolves it.
    drop(owner);
    let mut reopened = OutputStash::with_session_id_and_durable_root("owner-a", root.clone());
    let listed = reopened.list_durable_outputs().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].tool_use_id, "tool-a");
    assert_eq!(listed[0].turn, 7);
    assert_eq!(listed[0].stored_chars, 4);
    assert_eq!(listed[0].completeness, "partial-spill");
    assert!(reopened.view("tool-a", 0, 10).unwrap().contains("éééé"));

    let mut foreign = OutputStash::with_session_id_and_durable_root("owner-b", root.clone());
    foreign
        .insert("tool-b".into(), "shell".into(), "foreign".into())
        .unwrap();
    assert_eq!(foreign.list_durable_outputs().unwrap().len(), 1);
    assert!(foreign.view("tool-a", 0, 10).is_err());

    let pointer = parse_durable_pointer(
        &std::fs::read_to_string(owner_id_pointer_path(&root, "owner-a", "tool-a")).unwrap(),
    )
    .unwrap();
    std::fs::remove_file(blob_path(&root, &pointer.content_hash)).unwrap();
    assert!(
        reopened
            .insert_with_metadata("tool-a".into(), "shell".into(), "éééé".into(), details)
            .is_err()
    );
}

#[test]
fn owner_qualified_durable_identity_allows_same_tool_use_id() {
    let root = gc_root("owner-qualified-tool-use-id");
    let mut session_a = OutputStash::with_session_id_and_durable_root("session-a", root.clone());
    let mut session_b = OutputStash::with_session_id_and_durable_root("session-b", root.clone());

    session_a
        .insert(
            "call-1".into(),
            "shell".into(),
            "output from session A\n".into(),
        )
        .unwrap();
    session_b
        .insert(
            "call-1".into(),
            "shell".into(),
            "output from session B\n".into(),
        )
        .unwrap();

    assert_eq!(session_a.list_durable_outputs().unwrap().len(), 1);
    assert_eq!(session_b.list_durable_outputs().unwrap().len(), 1);
    assert_eq!(
        session_a.list_durable_outputs().unwrap()[0].tool_use_id,
        "call-1"
    );
    assert_eq!(
        session_b.list_durable_outputs().unwrap()[0].tool_use_id,
        "call-1"
    );

    // Reopen with no in-memory entries: each trusted owner resolves only its
    // own owner-qualified record despite the shared tool-use ID.
    drop(session_a);
    drop(session_b);
    let reopened_a = OutputStash::with_session_id_and_durable_root("session-a", root.clone());
    let reopened_b = OutputStash::with_session_id_and_durable_root("session-b", root);
    assert!(
        reopened_a
            .view("call-1", 0, 10)
            .unwrap()
            .contains("session A")
    );
    assert!(
        reopened_b
            .view("call-1", 0, 10)
            .unwrap()
            .contains("session B")
    );
}

#[test]
fn routed_output_list_reopens_complete_truncated_and_partial_spill_records() {
    let root = gc_root("routed-reload-retrieval");
    let owner_session = "trusted-reload-session";
    let records = [
        (
            "complete-output",
            "complete stored bytes\n",
            DurableOutputDetails {
                turn: 3,
                result_kind: "tool_result".into(),
                original_chars: "complete stored bytes\n".chars().count(),
                stored_chars: "complete stored bytes\n".chars().count(),
                completeness: "complete".into(),
            },
        ),
        (
            "truncated-output",
            "truncated stored bytes\n",
            DurableOutputDetails {
                turn: 4,
                result_kind: "shell_stdout".into(),
                original_chars: 80,
                stored_chars: "truncated stored bytes\n".chars().count(),
                completeness: "truncated".into(),
            },
        ),
        (
            "partial-spill-output",
            "partial spill stored bytes\n",
            DurableOutputDetails {
                turn: 5,
                result_kind: "shell_stdout".into(),
                original_chars: 120,
                stored_chars: "partial spill stored bytes\n".chars().count(),
                completeness: "partial-spill".into(),
            },
        ),
    ];

    let mut original = OutputStash::with_session_id_and_durable_root(owner_session, root.clone());
    for (id, stored, details) in &records {
        original
            .insert_with_metadata(
                (*id).into(),
                "shell".into(),
                (*stored).into(),
                details.clone(),
            )
            .unwrap();
    }
    // A pointer without readable stored bytes is not an authoritative result.
    original
        .insert(
            "missing-blob-output".into(),
            "shell".into(),
            "missing blob bytes\n".into(),
        )
        .unwrap();
    original.clear();
    assert!(original.entries.is_empty());
    let missing_pointer = parse_durable_pointer(
        &std::fs::read_to_string(owner_id_pointer_path(
            &root,
            owner_session,
            "missing-blob-output",
        ))
        .unwrap(),
    )
    .unwrap();
    std::fs::remove_file(blob_path(&root, &missing_pointer.content_hash)).unwrap();
    drop(original);

    // A process-style reopen has no in-memory entries. Exercise the real routed
    // output_list and output_view paths rather than the underlying helpers.
    let reopened = Mutex::new(OutputStash::with_session_id_and_durable_root(
        owner_session,
        root.clone(),
    ));
    let listed = serde_json::from_str::<Vec<serde_json::Value>>(
        &handle_stash_tool(&reopened, "output_list", None).expect("routed list after reopen"),
    )
    .expect("output_list JSON metadata");
    assert_eq!(listed.len(), records.len());
    assert!(!listed.iter().any(|metadata| {
        metadata["tool_use_id"] == serde_json::Value::String("missing-blob-output".into())
    }));

    for (id, stored, details) in &records {
        let metadata = listed
            .iter()
            .find(|metadata| metadata["tool_use_id"] == serde_json::Value::String((*id).into()))
            .expect("every durable record is listed");
        assert_eq!(metadata["owner_session_id"], owner_session);
        assert_eq!(metadata["turn"], details.turn);
        assert_eq!(metadata["original_chars"], details.original_chars);
        assert_eq!(metadata["stored_chars"], details.stored_chars);
        assert_eq!(metadata["completeness"], details.completeness);

        let args = serde_json::json!({"tool_use_id": id})
            .as_object()
            .unwrap()
            .clone();
        let viewed = handle_stash_tool(&reopened, "output_view", Some(&args))
            .expect("every listed pointer is viewable after reopen");
        assert!(viewed.contains(stored), "view lost stored bytes for {id}");
    }

    // A second trusted session knows the exact IDs but cannot discover or view
    // the first session's durable outputs.
    let foreign = Mutex::new(OutputStash::with_session_id_and_durable_root(
        "other-trusted-session",
        root,
    ));
    assert_eq!(
        handle_stash_tool(&foreign, "output_list", None).unwrap(),
        "[]"
    );
    for (id, _, _) in &records {
        let args = serde_json::json!({"tool_use_id": id})
            .as_object()
            .unwrap()
            .clone();
        assert!(handle_stash_tool(&foreign, "output_view", Some(&args)).is_err());
    }
}

#[test]
fn routed_view_reads_v1_and_legacy_remains_compatible_but_unlisted() {
    let root = gc_root("routed-legacy-v1-compatibility");
    let owner_session = "trusted-compat-session";
    let legacy_id = "legacy-routed-output";
    let v1_id = "v1-routed-output";
    let legacy_body = "legacy stored bytes\n";
    let v1_body = "v1 stored bytes\n";
    let legacy_hash = write_gc_blob(&root, legacy_body);
    let v1_hash = write_gc_blob(&root, v1_body);
    let ids = root.join("ids");
    atomic_write(
        &ids,
        &id_pointer_path(&root, legacy_id),
        format!("shell\t{legacy_hash}").as_bytes(),
    )
    .unwrap();
    atomic_write(
        &ids,
        &id_pointer_path(&root, v1_id),
        DurablePointerRecord::new_v1("shell", &v1_hash, Some(owner_session), 1)
            .serialize()
            .as_bytes(),
    )
    .unwrap();

    let stash = Mutex::new(OutputStash::with_session_id_and_durable_root(
        owner_session,
        root.clone(),
    ));
    // Historic records cannot enter authoritative output_list because neither
    // contains v2 ownership and completeness metadata. A v1 record with a
    // matching owner remains available through the routed trusted-session view.
    assert_eq!(
        handle_stash_tool(&stash, "output_list", None).unwrap(),
        "[]"
    );
    let v1_args = serde_json::json!({"tool_use_id": v1_id})
        .as_object()
        .unwrap()
        .clone();
    assert!(
        handle_stash_tool(&stash, "output_view", Some(&v1_args))
            .expect("v1 pointer remains readable for its trusted owner")
            .contains(v1_body)
    );

    // Unknown-owner legacy records remain readable through the explicit
    // compatibility path, but are intentionally denied to a trusted v2 session.
    assert_eq!(
        durable_read_at(&root, legacy_id, None).unwrap(),
        ("shell".into(), legacy_body.into())
    );
    let legacy_args = serde_json::json!({"tool_use_id": legacy_id})
        .as_object()
        .unwrap()
        .clone();
    assert!(handle_stash_tool(&stash, "output_view", Some(&legacy_args)).is_err());
}
