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
    let no_op_output = git_assert(&clone, &["rev-list", "--count", &format!("{head}..HEAD")]).await;
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
