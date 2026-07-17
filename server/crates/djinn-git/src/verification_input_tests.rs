use super::*;
use crate::test_support::{
    TestRepoFixture, configure_local_identity, git, init_repo_with_main_commit, write_and_commit,
};
use tempfile::TempDir;
fn write(repo_path: &Path, relative_path: &str, contents: &[u8]) {
    let path = repo_path.join(relative_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent directory");
    }
    std::fs::write(&path, contents).expect("write fixture file");
}
fn write_str(repo_path: &Path, relative_path: &str, contents: &str) {
    write(repo_path, relative_path, contents.as_bytes());
}
async fn fingerprint(repo_path: &Path) -> VerificationInputFingerprint {
    compute_verification_input_fingerprint(repo_path)
        .await
        .expect("compute fingerprint")
}
fn digest(f: VerificationInputFingerprint) -> VerificationInputDigestV1 {
    match f {
        VerificationInputFingerprint::Available(d) => d,
        VerificationInputFingerprint::Unavailable(reason) => {
            panic!("expected available fingerprint, got unavailable: {reason}")
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_repo_produces_available_deterministic_digest() {
    let fixture = init_repo_with_main_commit();
    let first = digest(fingerprint(fixture.path()).await);
    let second = digest(fingerprint(fixture.path()).await);
    assert_eq!(first.version, VERIFICATION_INPUT_FINGERPRINT_VERSION_V1);
    assert_eq!(first.fingerprint.len(), 64);
    assert!(
        first.fingerprint.chars().all(|c| c.is_ascii_hexdigit()),
        "fingerprint should be lowercase hex"
    );
    assert_eq!(first.fingerprint, second.fingerprint);
    assert!(first.merge_base.is_some());
    assert!(!first.head.is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tracked_text_edit_changes_digest() {
    let fixture = init_repo_with_main_commit();
    let before = digest(fingerprint(fixture.path()).await);
    write_str(fixture.path(), "README.md", "hello\nchanged\n");
    let after = digest(fingerprint(fixture.path()).await);
    assert_ne!(
        before.fingerprint, after.fingerprint,
        "dirty tracked edit must change digest"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tracked_executable_mode_change_alters_digest() {
    let fixture = init_repo_with_main_commit();
    write_str(fixture.path(), "script.sh", "echo hello\n");
    git(fixture.path(), ["add", "script.sh"]);
    git(fixture.path(), ["commit", "-m", "add script"]);
    let before = digest(fingerprint(fixture.path()).await);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = fixture.path().join("script.sh");
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    let after = digest(fingerprint(fixture.path()).await);
    #[cfg(unix)]
    {
        assert_ne!(
            before.fingerprint, after.fingerprint,
            "executable-bit change must alter digest on unix"
        );
    }
    #[cfg(not(unix))]
    {
        let _ = before;
        let _ = after;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_index_change_alters_digest() {
    let fixture = init_repo_with_main_commit();
    write_str(fixture.path(), "README.md", "hello\nv2\n");
    let unstaged = digest(fingerprint(fixture.path()).await);
    git(fixture.path(), ["add", "README.md"]);
    let staged = digest(fingerprint(fixture.path()).await);
    assert_ne!(
        unstaged.fingerprint, staged.fingerprint,
        "staging changes the index blob SHA and must alter the digest"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignored_generated_config_alters_digest() {
    let fixture = init_repo_with_main_commit();
    write_str(fixture.path(), ".gitignore", "*.gen\n");
    git(fixture.path(), ["add", ".gitignore"]);
    git(fixture.path(), ["commit", "-m", "ignore generated"]);
    write_str(fixture.path(), "config.gen", "v1\n");
    let before = digest(fingerprint(fixture.path()).await);
    write_str(fixture.path(), "config.gen", "v2\n");
    let after = digest(fingerprint(fixture.path()).await);
    assert_ne!(
        before.fingerprint, after.fingerprint,
        "ignored file content change must alter digest"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn untracked_binary_content_alters_digest() {
    let fixture = init_repo_with_main_commit();
    write(fixture.path(), "data.bin", &[0x00, 0x01, 0xFF, 0xFE]);
    let before = digest(fingerprint(fixture.path()).await);
    write(fixture.path(), "data.bin", &[0x00, 0x02, 0xFF, 0xFE]);
    let after = digest(fingerprint(fixture.path()).await);
    assert_ne!(
        before.fingerprint, after.fingerprint,
        "untracked binary content change must alter digest"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nul_and_non_utf8_bytes_are_hashed() {
    let fixture = init_repo_with_main_commit();
    write(fixture.path(), "blob.dat", &[b'a', 0x00, b'b', 0xC3, 0x28]);
    let before = digest(fingerprint(fixture.path()).await);
    write(fixture.path(), "blob.dat", &[b'a', 0x00, b'c', 0xC3, 0x28]);
    let after = digest(fingerprint(fixture.path()).await);
    assert_ne!(before.fingerprint, after.fingerprint);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symlink_target_change_alters_digest() {
    let fixture = init_repo_with_main_commit();
    write_str(fixture.path(), "target_a.txt", "a\n");
    write_str(fixture.path(), "target_b.txt", "b\n");
    std::os::unix::fs::symlink("target_a.txt", fixture.path().join("link"))
        .expect("create symlink");
    let before = digest(fingerprint(fixture.path()).await);
    std::fs::remove_file(fixture.path().join("link")).unwrap();
    std::os::unix::fs::symlink("target_b.txt", fixture.path().join("link"))
        .expect("recreate symlink");
    let after = digest(fingerprint(fixture.path()).await);
    assert_ne!(
        before.fingerprint, after.fingerprint,
        "symlink target change must alter digest"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn tracked_symlink_produces_available_digest_and_alters_on_change() {
    let fixture = init_repo_with_main_commit();
    write_str(fixture.path(), "target_a.txt", "a\n");
    write_str(fixture.path(), "target_b.txt", "b\n");
    std::os::unix::fs::symlink("target_a.txt", fixture.path().join("tracked_link"))
        .expect("create symlink");
    git(fixture.path(), ["add", "tracked_link"]);
    git(fixture.path(), ["commit", "-m", "add tracked symlink"]);
    let before = match fingerprint(fixture.path()).await {
        VerificationInputFingerprint::Available(d) => d,
        VerificationInputFingerprint::Unavailable(reason) => {
            panic!("tracked symlink should produce Available, got: {reason}")
        }
    };
    std::fs::remove_file(fixture.path().join("tracked_link")).unwrap();
    std::os::unix::fs::symlink("target_b.txt", fixture.path().join("tracked_link"))
        .expect("recreate tracked symlink");
    let after = digest(fingerprint(fixture.path()).await);
    assert_ne!(
        before.fingerprint, after.fingerprint,
        "tracked symlink target change must alter digest"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn non_utf8_pathname_is_preserved_and_alters_digest() {
    let fixture = init_repo_with_main_commit();
    let non_utf8_name: &[u8] = b"bad\xffname.txt";
    {
        use std::os::unix::ffi::OsStrExt;
        let os_name = std::ffi::OsStr::from_bytes(non_utf8_name);
        let path = fixture.path().join(os_name);
        std::fs::write(&path, b"content\n").expect("write non-utf8 named file");
    }
    let before = digest(fingerprint(fixture.path()).await);
    {
        use std::os::unix::ffi::OsStrExt;
        let os_name = std::ffi::OsStr::from_bytes(non_utf8_name);
        let path = fixture.path().join(os_name);
        std::fs::write(&path, b"changed\n").expect("rewrite non-utf8 named file");
    }
    let after = digest(fingerprint(fixture.path()).await);
    assert_ne!(
        before.fingerprint, after.fingerprint,
        "content change under a non-UTF-8 path must alter digest"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn untracked_and_ignored_are_both_included() {
    let fixture = init_repo_with_main_commit();
    write_str(fixture.path(), ".gitignore", "*.ignored\n");
    git(fixture.path(), ["add", ".gitignore"]);
    git(fixture.path(), ["commit", "-m", "add gitignore"]);
    write_str(fixture.path(), "untracked.txt", "u\n");
    write_str(fixture.path(), "generated.ignored", "i\n");
    let before = digest(fingerprint(fixture.path()).await);
    write_str(fixture.path(), "generated.ignored", "i2\n");
    let after_ignored = digest(fingerprint(fixture.path()).await);
    assert_ne!(before.fingerprint, after_ignored.fingerprint);
    write_str(fixture.path(), "generated.ignored", "i\n");
    write_str(fixture.path(), "untracked.txt", "u2\n");
    let after_untracked = digest(fingerprint(fixture.path()).await);
    assert_ne!(before.fingerprint, after_untracked.fingerprint);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn path_ordering_is_bytewise_and_deterministic() {
    let fixture = init_repo_with_main_commit();
    write_str(fixture.path(), "zeta.txt", "z\n");
    write_str(fixture.path(), "alpha.txt", "a\n");
    write_str(fixture.path(), "mid.txt", "m\n");
    let first = digest(fingerprint(fixture.path()).await);
    std::fs::remove_file(fixture.path().join("zeta.txt")).unwrap();
    std::fs::remove_file(fixture.path().join("alpha.txt")).unwrap();
    std::fs::remove_file(fixture.path().join("mid.txt")).unwrap();
    write_str(fixture.path(), "alpha.txt", "a\n");
    write_str(fixture.path(), "mid.txt", "m\n");
    write_str(fixture.path(), "zeta.txt", "z\n");
    let second = digest(fingerprint(fixture.path()).await);
    assert_eq!(
        first.fingerprint, second.fingerprint,
        "creation order must not affect digest — paths are sorted bytewise"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn fifo_at_tracked_path_makes_identity_unavailable() {
    let fixture = init_repo_with_main_commit();
    write_str(fixture.path(), "pipe.txt", "regular\n");
    git(fixture.path(), ["add", "pipe.txt"]);
    git(fixture.path(), ["commit", "-m", "add pipe"]);
    std::fs::remove_file(fixture.path().join("pipe.txt")).unwrap();
    let result = std::process::Command::new("mkfifo")
        .arg(fixture.path().join("pipe.txt"))
        .status()
        .expect("mkfifo");
    assert!(result.success(), "mkfifo should succeed");
    let result = fingerprint(fixture.path()).await;
    assert!(
        result.is_unavailable(),
        "FIFO at tracked path should make identity unavailable, got: {result:?}"
    );
    match result.unavailable_reason().unwrap() {
        VerificationInputUnavailable::UnsupportedSpecialFile { path, kind } => {
            assert_eq!(path, "pipe.txt");
            assert_eq!(kind, "fifo");
        }
        other => panic!("expected UnsupportedSpecialFile, got {other:?}"),
    }
}
async fn configured_fingerprint(
    repo_path: &Path,
    config: &VerificationInputFingerprintConfig,
) -> VerificationInputFingerprint {
    compute_verification_input_fingerprint_with_config(repo_path, config)
        .await
        .expect("compute configured fingerprint")
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_external_content_change_alters_digest() {
    let fixture = init_repo_with_main_commit();
    let external = tempfile::tempdir().expect("create external mount");
    write_str(external.path(), "toolchain/version.txt", "v1\n");
    let mut config = VerificationInputFingerprintConfig::default();
    config.manifest.read_only_external_inputs.push(
        djinn_core::canonical_verify::DeclaredExternalInputV1 {
            id: "toolchain".to_string(),
            locator: "host://toolchain".to_string(),
        },
    );
    config.external_inputs.push(ResolvedExternalInputV1 {
        id: "toolchain".to_string(),
        path: external.path().to_path_buf(),
    });
    let before = digest(configured_fingerprint(fixture.path(), &config).await);
    write_str(external.path(), "toolchain/version.txt", "v2\n");
    let after = digest(configured_fingerprint(fixture.path(), &config).await);
    assert_ne!(before.fingerprint, after.fingerprint);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_output_only_files_are_removed_and_excluded_from_digest() {
    let fixture = init_repo_with_main_commit();
    let mut config = VerificationInputFingerprintConfig::default();
    config.manifest.output_only_globs.push("out/**".to_string());
    write_str(fixture.path(), "out/result.txt", "first generated result\n");
    let first = digest(configured_fingerprint(fixture.path(), &config).await);
    assert!(
        !fixture.path().join("out/result.txt").exists(),
        "configured output-only file must be removed before hashing"
    );
    write_str(
        fixture.path(),
        "out/result.txt",
        "different generated result\n",
    );
    let second = digest(configured_fingerprint(fixture.path(), &config).await);
    assert!(
        !fixture.path().join("out/result.txt").exists(),
        "recreated output-only file must be removed before hashing"
    );
    assert_eq!(first.fingerprint, second.fingerprint);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_overlapping_output_only_globs_fail_before_cleanup() {
    let fixture = init_repo_with_main_commit();
    let output = fixture.path().join("out/result.txt");
    write_str(fixture.path(), "out/result.txt", "must not be deleted\n");
    let mut config = VerificationInputFingerprintConfig::default();
    config
        .manifest
        .output_only_globs
        .extend(["out/**".to_string(), "out/*.txt".to_string()]);
    let result = configured_fingerprint(fixture.path(), &config).await;
    assert!(matches!(
        result,
        VerificationInputFingerprint::Unavailable(
            VerificationInputUnavailable::MalformedManifest { .. }
        )
    ));
    assert!(
        output.exists(),
        "ambiguous declaration must fail before cleanup"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unresolved_base_ref_makes_identity_unavailable() {
    let fixture = init_repo_with_main_commit();
    let result = compute_verification_input_fingerprint_with_config(
        fixture.path(),
        &VerificationInputFingerprintConfig::new("nonexistent-branch"),
    )
    .await
    .expect("no infra error");
    assert!(result.is_unavailable());
    assert!(matches!(
        result.unavailable_reason(),
        Some(VerificationInputUnavailable::UnresolvedBaseRef { .. })
    ));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_untracked_entry_is_traversal_race() {
    let fixture = init_repo_with_main_commit();
    write_str(fixture.path(), "ephemeral.txt", "gone soon\n");
    std::fs::remove_file(fixture.path().join("ephemeral.txt")).unwrap();
    let anchor = PermittedRootAnchor::capture(fixture.path(), b".").expect("anchor fixture");
    let result = classify_worktree_entry(&anchor, b"ephemeral.txt", true);
    assert!(matches!(
        result,
        Err(VerificationInputUnavailable::MissingExtraEntry { .. })
    ));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleted_tracked_file_is_valid_missing_state() {
    let fixture = init_repo_with_main_commit();
    write_str(fixture.path(), "temp.txt", "temp\n");
    git(fixture.path(), ["add", "temp.txt"]);
    git(fixture.path(), ["commit", "-m", "add temp"]);
    std::fs::remove_file(fixture.path().join("temp.txt")).unwrap();
    let result = fingerprint(fixture.path()).await;
    assert!(
        result.is_available(),
        "deleted tracked file should produce Available with TYPE_MISSING, got: {result:?}"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verification_does_not_depend_on_submission_diff() {
    let fixture = init_repo_with_main_commit();
    write_str(fixture.path(), "extra.txt", "x\n");
    let result = fingerprint(fixture.path()).await;
    assert!(result.is_available());
    assert!(result.fingerprint().is_some());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canonical_stream_has_stable_magic_header() {
    let fixture = init_repo_with_main_commit();
    let head = try_rev_parse(fixture.path(), "HEAD")
        .await
        .unwrap()
        .unwrap();
    let resolved_base = resolve_base_ref(fixture.path(), "main")
        .await
        .unwrap()
        .unwrap();
    let merge_base = try_merge_base(fixture.path(), &resolved_base)
        .await
        .unwrap()
        .unwrap();
    let index_output = git_binary_stdout(
        fixture.path(),
        vec!["ls-files".into(), "-s".into(), "-z".into()],
    )
    .await
    .unwrap();
    let mut index_entries = parse_index_entries(&index_output);
    index_entries.sort_by(|a, b| a.path.cmp(&b.path));
    let mut stream = CanonicalStream::new();
    stream.write_header();
    stream.write_refs(&merge_base, &head);
    stream.write_index_entries(&index_entries);
    stream.write_worktree_states(&[]);
    stream.write_worktree_states(&[]);
    let bytes = stream.finalize();
    let magic_len = u64::from_le_bytes(bytes[0..8].try_into().unwrap()) as usize;
    assert_eq!(magic_len, STREAM_MAGIC.len());
    assert_eq!(&bytes[8..8 + magic_len], STREAM_MAGIC);
    let offset = 8 + magic_len;
    let tag_len = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()) as usize;
    assert_eq!(tag_len, STREAM_VERSION_TAG.len());
    assert_eq!(&bytes[offset + 8..offset + 8 + tag_len], STREAM_VERSION_TAG);
    let offset = offset + 8 + tag_len;
    let version = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
    assert_eq!(version, VERIFICATION_INPUT_FINGERPRINT_VERSION_V1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_untracked_file_alters_digest() {
    let fixture = init_repo_with_main_commit();
    let before = digest(fingerprint(fixture.path()).await);
    write_str(fixture.path(), "new.txt", "new\n");
    let after = digest(fingerprint(fixture.path()).await);
    assert_ne!(before.fingerprint, after.fingerprint);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_change_alters_digest() {
    let fixture = init_repo_with_main_commit();
    let before = digest(fingerprint(fixture.path()).await);
    write_and_commit(fixture.path(), "src/new.rs", "pub fn f() {}\n", "add code");
    let after = digest(fingerprint(fixture.path()).await);
    assert_ne!(before.fingerprint, after.fingerprint);
}

/// Create a real Git submodule fixture: an outer repo with a checked-out
/// inner submodule at `sub_path` containing one committed file.
#[allow(dead_code)]
struct SubmoduleFixture {
    outer: TestRepoFixture,
    inner: TempDir,
}

fn git_with_file_protocol<I, S>(repo_path: &Path, args: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let args: Vec<std::ffi::OsString> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect();
    let output = std::process::Command::new("git")
        .args(["-c", "protocol.file.allow=always"])
        .args(&args)
        .current_dir(repo_path)
        .output()
        .unwrap_or_else(|err| {
            panic!(
                "failed to run git -c protocol.file.allow=always {args:?} in {}: {err}",
                repo_path.display()
            )
        });
    assert!(
        output.status.success(),
        "git -c protocol.file.allow=always {:?} failed in {} with status {:?}\nstdout:\n{}\nstderr:\n{}",
        args,
        repo_path.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn make_submodule_fixture(sub_path: &str) -> SubmoduleFixture {
    let outer = init_repo_with_main_commit();
    let inner = tempfile::tempdir().expect("create inner temp dir");
    git(inner.path(), ["init"]);
    configure_local_identity(inner.path());
    write_str(inner.path(), "README.md", "submodule\n");
    git(inner.path(), ["add", "README.md"]);
    git(inner.path(), ["commit", "-m", "inner init"]);
    git(inner.path(), ["branch", "-m", "main"]);
    git_with_file_protocol(
        outer.path(),
        ["submodule", "add", inner.path().to_str().unwrap(), sub_path],
    );
    git(outer.path(), ["commit", "-m", "add submodule"]);
    SubmoduleFixture { outer, inner }
}

fn make_nested_submodule_fixture(outer_sub: &str, inner_sub: &str) -> SubmoduleFixture {
    let outer = init_repo_with_main_commit();
    git(outer.path(), ["config", "protocol.file.allow", "always"]);
    let inner = tempfile::tempdir().expect("create inner temp dir");
    git(inner.path(), ["init"]);
    configure_local_identity(inner.path());
    write_str(inner.path(), "README.md", "inner module\n");
    git(inner.path(), ["add", "README.md"]);
    git(inner.path(), ["commit", "-m", "inner init"]);
    git(inner.path(), ["branch", "-m", "main"]);
    let nested = tempfile::tempdir().expect("create nested temp dir");
    git(nested.path(), ["init"]);
    configure_local_identity(nested.path());
    write_str(nested.path(), "nested.txt", "nested module\n");
    git(nested.path(), ["add", "nested.txt"]);
    git(nested.path(), ["commit", "-m", "nested init"]);
    git(nested.path(), ["branch", "-m", "main"]);
    git_with_file_protocol(
        inner.path(),
        [
            "submodule",
            "add",
            nested.path().to_str().unwrap(),
            inner_sub,
        ],
    );
    git(inner.path(), ["commit", "-m", "add nested submodule"]);
    git_with_file_protocol(
        outer.path(),
        [
            "submodule",
            "add",
            inner.path().to_str().unwrap(),
            outer_sub,
        ],
    );
    git_with_file_protocol(
        outer.path(),
        ["submodule", "update", "--init", "--recursive"],
    );
    git(outer.path(), ["commit", "-m", "add submodule"]);
    SubmoduleFixture { outer, inner }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_submodule_produces_stable_available_digest() {
    let fixture = make_submodule_fixture("vendor");
    let first = match fingerprint(fixture.outer.path()).await {
        VerificationInputFingerprint::Available(d) => d,
        VerificationInputFingerprint::Unavailable(reason) => {
            panic!("clean submodule should produce Available, got: {reason}")
        }
    };
    let second = digest(fingerprint(fixture.outer.path()).await);
    assert_eq!(
        first.fingerprint, second.fingerprint,
        "clean submodule repo should be deterministic"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submodule_local_dirtiness_changes_top_level_digest() {
    let fixture = make_submodule_fixture("vendor");
    let before = digest(fingerprint(fixture.outer.path()).await);
    write_str(
        &fixture.outer.path().join("vendor"),
        "dirty.txt",
        "local change\n",
    );
    let after = digest(fingerprint(fixture.outer.path()).await);
    assert_ne!(
        before.fingerprint, after.fingerprint,
        "submodule-local dirtiness must change the top-level digest"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submodule_tracked_file_change_alters_digest() {
    let fixture = make_submodule_fixture("vendor");
    let before = digest(fingerprint(fixture.outer.path()).await);
    write_str(
        &fixture.outer.path().join("vendor"),
        "README.md",
        "changed\n",
    );
    let after = digest(fingerprint(fixture.outer.path()).await);
    assert_ne!(
        before.fingerprint, after.fingerprint,
        "modifying a tracked file inside a submodule must alter the top-level digest"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_submodule_dirtiness_changes_top_level_digest() {
    let fixture = make_nested_submodule_fixture("vendor", "nested");
    let before = match fingerprint(fixture.outer.path()).await {
        VerificationInputFingerprint::Available(d) => d,
        VerificationInputFingerprint::Unavailable(reason) => {
            panic!("clean nested submodule should produce Available, got: {reason}")
        }
    };
    let nested_path = fixture.outer.path().join("vendor").join("nested");
    write_str(&nested_path, "dirty.txt", "nested local change\n");
    let after = digest(fingerprint(fixture.outer.path()).await);
    assert_ne!(
        before.fingerprint, after.fingerprint,
        "nested-submodule dirtiness must change the top-level digest"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_submodule_content_fails_closed() {
    let fixture = make_submodule_fixture("vendor");
    let sub_path = fixture.outer.path().join("vendor");
    std::fs::remove_dir_all(&sub_path).expect("remove submodule checkout");
    let result = fingerprint(fixture.outer.path()).await;
    assert!(
        result.is_unavailable(),
        "missing submodule checkout should make identity unavailable, got: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uninitialized_submodule_fails_closed() {
    let fixture = make_submodule_fixture("vendor");
    git(
        fixture.outer.path(),
        ["submodule", "deinit", "-f", "vendor"],
    );
    let result = fingerprint(fixture.outer.path()).await;
    assert!(
        result.is_unavailable(),
        "uninitialized submodule should make identity unavailable, got: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submodule_detached_head_mismatch_fails_closed() {
    let fixture = make_submodule_fixture("vendor");
    let sub_path = fixture.outer.path().join("vendor");
    configure_local_identity(&sub_path);
    write_str(&sub_path, "new_branch_file.txt", "branch\n");
    git(&sub_path, ["checkout", "-b", "other-branch"]);
    git(&sub_path, ["add", "new_branch_file.txt"]);
    git(&sub_path, ["commit", "-m", "other branch commit"]);
    let result = fingerprint(fixture.outer.path()).await;
    assert!(
        result.is_unavailable(),
        "submodule HEAD mismatch should make identity unavailable, got: {result:?}"
    );
}

#[cfg(unix)]
fn assert_unavailable(result: VerificationInputFingerprint) {
    assert!(
        result.is_unavailable(),
        "unstable traversal must never produce Available, got: {result:?}"
    );
}

// The mutation seam is process-global test infrastructure. Hold this guard
// from installation through traversal and clearing so parallel Tokio tests
// cannot exchange callbacks.
#[cfg(unix)]
static READ_MUTATION_TEST_SERIAL_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
    std::sync::OnceLock::new();

#[cfg(unix)]
fn lock_read_mutation_test() -> std::sync::MutexGuard<'static, ()> {
    READ_MUTATION_TEST_SERIAL_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .expect("read mutation test serial lock")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn configured_public_traversal_rejects_read_boundary_replacement() {
    let _hook_guard = lock_read_mutation_test();
    let fixture = init_repo_with_main_commit();
    let path = fixture.path().join("README.md");
    set_test_read_mutation_hook(Some(std::sync::Arc::new({
        let path = path.clone();
        move |candidate| {
            if candidate == path {
                std::fs::remove_file(&path).expect("remove inspected file");
                std::fs::write(&path, b"replacement\n").expect("replace inspected file");
            }
        }
    })));
    let result = configured_fingerprint(
        fixture.path(),
        &VerificationInputFingerprintConfig::default(),
    )
    .await;
    set_test_read_mutation_hook(None);
    assert_unavailable(result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn configured_submodule_traversal_rejects_root_replacement() {
    let _hook_guard = lock_read_mutation_test();
    let fixture = make_submodule_fixture("vendor");
    let submodule = fixture.outer.path().join("vendor");
    let inspected = submodule.join("README.md");
    let original = fixture.outer.path().join("vendor-approved-original");
    let outside = tempfile::tempdir().expect("create outside tree");
    write_str(outside.path(), "README.md", "outside input\n");
    let outside_path = outside.path().to_path_buf();
    set_test_read_mutation_hook(Some(std::sync::Arc::new(move |candidate| {
        if candidate == inspected {
            std::fs::rename(&submodule, &original).expect("rename approved submodule");
            std::os::unix::fs::symlink(&outside_path, &submodule)
                .expect("replace submodule with outside symlink");
        }
    })));
    let result = configured_fingerprint(
        fixture.outer.path(),
        &VerificationInputFingerprintConfig::default(),
    )
    .await;
    set_test_read_mutation_hook(None);
    assert_unavailable(result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn configured_external_traversal_rejects_mount_root_replacement() {
    let _hook_guard = lock_read_mutation_test();
    let fixture = init_repo_with_main_commit();
    let mounts = tempfile::tempdir().expect("create mount parent");
    let mount = mounts.path().join("input");
    std::fs::create_dir(&mount).expect("create mount");
    write_str(&mount, "input.txt", "approved input\n");
    let outside = tempfile::tempdir().expect("create outside tree");
    write_str(outside.path(), "input.txt", "outside input\n");
    let mut config = VerificationInputFingerprintConfig::default();
    config.manifest.read_only_external_inputs.push(
        djinn_core::canonical_verify::DeclaredExternalInputV1 {
            id: "external".into(),
            locator: "host://external".into(),
        },
    );
    config.external_inputs.push(ResolvedExternalInputV1 {
        id: "external".into(),
        path: mount.clone(),
    });
    let inspected = mount.join("input.txt");
    let original = mounts.path().join("approved-original");
    let outside_path = outside.path().to_path_buf();
    set_test_read_mutation_hook(Some(std::sync::Arc::new(move |candidate| {
        if candidate == inspected {
            std::fs::rename(&mount, &original).expect("rename approved mount");
            std::os::unix::fs::symlink(&outside_path, &mount)
                .expect("replace mount with outside symlink");
        }
    })));
    let result = configured_fingerprint(fixture.path(), &config).await;
    set_test_read_mutation_hook(None);
    assert_unavailable(result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn configured_public_traversal_rejects_read_boundary_content_mutation() {
    let _hook_guard = lock_read_mutation_test();
    let fixture = init_repo_with_main_commit();
    let path = fixture.path().join("README.md");
    set_test_read_mutation_hook(Some(std::sync::Arc::new({
        let path = path.clone();
        move |candidate| {
            if candidate == path {
                std::fs::write(&path, b"content changed at read boundary\n")
                    .expect("mutate inspected file");
            }
        }
    })));
    let result = configured_fingerprint(
        fixture.path(),
        &VerificationInputFingerprintConfig::default(),
    )
    .await;
    set_test_read_mutation_hook(None);
    assert_unavailable(result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn configured_external_traversal_rejects_read_boundary_type_mutation() {
    let _hook_guard = lock_read_mutation_test();
    let fixture = init_repo_with_main_commit();
    let external = tempfile::tempdir().expect("create external mount");
    write_str(external.path(), "input.txt", "input\n");
    let mut config = VerificationInputFingerprintConfig::default();
    config.manifest.read_only_external_inputs.push(
        djinn_core::canonical_verify::DeclaredExternalInputV1 {
            id: "external".into(),
            locator: "host://external".into(),
        },
    );
    config.external_inputs.push(ResolvedExternalInputV1 {
        id: "external".into(),
        path: external.path().into(),
    });
    let path = external.path().join("input.txt");
    set_test_read_mutation_hook(Some(std::sync::Arc::new(move |candidate| {
        if candidate == path {
            std::fs::remove_file(&path).expect("remove inspected external file");
            std::fs::create_dir(&path).expect("replace external file with directory");
        }
    })));
    let result = configured_fingerprint(fixture.path(), &config).await;
    set_test_read_mutation_hook(None);
    assert_unavailable(result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn configured_submodule_traversal_rejects_read_boundary_disappearance() {
    let _hook_guard = lock_read_mutation_test();
    let fixture = make_submodule_fixture("vendor");
    let path = fixture.outer.path().join("vendor/README.md");
    set_test_read_mutation_hook(Some(std::sync::Arc::new(move |candidate| {
        if candidate == path {
            std::fs::remove_file(&path).expect("remove inspected submodule file");
        }
    })));
    let result = configured_fingerprint(
        fixture.outer.path(),
        &VerificationInputFingerprintConfig::default(),
    )
    .await;
    set_test_read_mutation_hook(None);
    assert_unavailable(result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn symlink_escape_and_replacement_fail_closed() {
    let _hook_guard = lock_read_mutation_test();
    let fixture = init_repo_with_main_commit();
    std::os::unix::fs::symlink("/etc/passwd", fixture.path().join("escape"))
        .expect("create escaping symlink");
    assert_unavailable(
        configured_fingerprint(
            fixture.path(),
            &VerificationInputFingerprintConfig::default(),
        )
        .await,
    );
    std::fs::remove_file(fixture.path().join("escape")).expect("remove escaping symlink");
    write_str(fixture.path(), "one", "one\n");
    write_str(fixture.path(), "two", "two\n");
    let path = fixture.path().join("link");
    std::os::unix::fs::symlink("one", &path).expect("create symlink");
    set_test_read_mutation_hook(Some(std::sync::Arc::new(move |candidate| {
        if candidate == path {
            std::fs::remove_file(&path).expect("remove inspected symlink");
            std::os::unix::fs::symlink("two", &path).expect("replace inspected symlink");
        }
    })));
    let result = configured_fingerprint(
        fixture.path(),
        &VerificationInputFingerprintConfig::default(),
    )
    .await;
    set_test_read_mutation_hook(None);
    assert_unavailable(result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn symlink_escape_in_external_and_submodule_fails_closed() {
    let fixture = make_submodule_fixture("vendor");
    std::os::unix::fs::symlink("/etc/passwd", fixture.outer.path().join("vendor/escape"))
        .expect("create escaping submodule symlink");
    assert_unavailable(
        configured_fingerprint(
            fixture.outer.path(),
            &VerificationInputFingerprintConfig::default(),
        )
        .await,
    );
    std::fs::remove_file(fixture.outer.path().join("vendor/escape"))
        .expect("remove escaping submodule symlink");
    let external = tempfile::tempdir().expect("create external mount");
    std::os::unix::fs::symlink("/etc/passwd", external.path().join("escape"))
        .expect("create escaping external symlink");
    let mut config = VerificationInputFingerprintConfig::default();
    config.manifest.read_only_external_inputs.push(
        djinn_core::canonical_verify::DeclaredExternalInputV1 {
            id: "external".into(),
            locator: "host://external".into(),
        },
    );
    config.external_inputs.push(ResolvedExternalInputV1 {
        id: "external".into(),
        path: external.path().into(),
    });
    assert_unavailable(configured_fingerprint(fixture.outer.path(), &config).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn unreadable_untracked_file_makes_identity_unavailable_when_enforced() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = init_repo_with_main_commit();
    let path = fixture.path().join("private.txt");
    write_str(fixture.path(), "private.txt", "private\n");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
        .expect("make file unreadable");
    let enforced = std::fs::File::open(&path).is_err();
    let result = configured_fingerprint(
        fixture.path(),
        &VerificationInputFingerprintConfig::default(),
    )
    .await;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("restore file permissions");
    if enforced {
        assert_unavailable(result);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn intermediate_symlink_escape_fails_closed_for_repo_and_external_walks() {
    let fixture = init_repo_with_main_commit();
    write_str(fixture.path(), "nested/input.txt", "tracked input\n");
    git(fixture.path(), ["add", "nested/input.txt"]);
    git(fixture.path(), ["commit", "-m", "add nested input"]);
    let outside = tempfile::tempdir().expect("create outside directory");
    write_str(outside.path(), "input.txt", "outside input\n");
    std::fs::remove_dir_all(fixture.path().join("nested")).expect("remove nested directory");
    std::os::unix::fs::symlink(outside.path(), fixture.path().join("nested"))
        .expect("replace nested directory with escaping link");
    assert_unavailable(
        configured_fingerprint(
            fixture.path(),
            &VerificationInputFingerprintConfig::default(),
        )
        .await,
    );
    std::fs::remove_file(fixture.path().join("nested")).expect("remove escaping repo link");
    write_str(fixture.path(), "nested/input.txt", "tracked input\n");

    let external = tempfile::tempdir().expect("create external mount");
    write_str(external.path(), "nested/input.txt", "external input\n");
    std::fs::remove_dir_all(external.path().join("nested")).expect("remove external nested dir");
    std::os::unix::fs::symlink(outside.path(), external.path().join("nested"))
        .expect("replace external nested dir with escaping link");
    let mut config = VerificationInputFingerprintConfig::default();
    config.manifest.read_only_external_inputs.push(
        djinn_core::canonical_verify::DeclaredExternalInputV1 {
            id: "external".into(),
            locator: "host://external".into(),
        },
    );
    config.external_inputs.push(ResolvedExternalInputV1 {
        id: "external".into(),
        path: external.path().into(),
    });
    assert_unavailable(configured_fingerprint(fixture.path(), &config).await);
}

fn complete_config(
    first_id: &str,
    first_path: &Path,
    second_id: &str,
    second_path: &Path,
) -> VerificationInputFingerprintConfig {
    let mut config = VerificationInputFingerprintConfig::default();
    for (id, locator, path) in [
        (first_id, format!("host://{first_id}"), first_path),
        (second_id, format!("host://{second_id}"), second_path),
    ] {
        config.manifest.read_only_external_inputs.push(
            djinn_core::canonical_verify::DeclaredExternalInputV1 {
                id: id.to_owned(),
                locator,
            },
        );
        config.external_inputs.push(ResolvedExternalInputV1 {
            id: id.to_owned(),
            path: path.to_path_buf(),
        });
    }
    config
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_complete_v1_stream_frames_gitlinks_and_externals() {
    let fixture = make_nested_submodule_fixture("vendor", "nested");
    let mounts = tempfile::tempdir().expect("create external mount parent");
    let alpha = mounts.path().join("alpha");
    let zeta = mounts.path().join("zeta");
    write_str(&alpha, "a/first.txt", "alpha-first\n");
    write_str(&alpha, "z/last.txt", "alpha-last\n");
    write_str(&zeta, "a/first.txt", "zeta-first\n");
    write_str(&zeta, "z/last.txt", "zeta-last\n");
    let config = complete_config("alpha", &alpha, "zeta", &zeta);

    // The configured public API is the subject under test. Rebuild its complete
    // byte stream independently below as a framing golden: this pins the two
    // gitlink/external sections that the historical header-only check omitted.
    let actual = digest(configured_fingerprint(fixture.outer.path(), &config).await);

    let head = try_rev_parse(fixture.outer.path(), "HEAD")
        .await
        .unwrap()
        .unwrap();
    let resolved_base = resolve_base_ref(fixture.outer.path(), "main")
        .await
        .unwrap()
        .unwrap();
    let merge_base = try_merge_base(fixture.outer.path(), &resolved_base)
        .await
        .unwrap()
        .unwrap();
    let index_output = git_binary_stdout(
        fixture.outer.path(),
        vec!["ls-files".into(), "-s".into(), "-z".into()],
    )
    .await
    .unwrap();
    let mut index_entries = parse_index_entries(&index_output);
    let anchor = PermittedRootAnchor::capture(fixture.outer.path(), b".").unwrap();
    let mut tracked_states = Vec::new();
    let mut gitlink_states = Vec::new();
    for entry in &index_entries {
        if entry.mode == MODE_GITLINK_TAG {
            gitlink_states.push(
                collect_gitlink_state(&anchor, &entry.path, &entry.blob_sha)
                    .await
                    .unwrap(),
            );
        } else {
            tracked_states.push(classify_worktree_entry(&anchor, &entry.path, false).unwrap());
        }
    }
    let mut extra_states = Vec::new();
    for path in collect_extra_paths(fixture.outer.path()).await.unwrap() {
        extra_states.push(classify_worktree_entry(&anchor, &path, true).unwrap());
    }
    index_entries.sort_by(|a, b| a.path.cmp(&b.path));
    tracked_states.sort_by(|a, b| a.path.cmp(&b.path));
    gitlink_states.sort_by(|a, b| a.path.cmp(&b.path));
    extra_states.sort_by(|a, b| a.path.cmp(&b.path));
    let external_states = collect_external_states(&config).unwrap();
    assert_eq!(
        gitlink_states.len(),
        1,
        "fixture must frame its outer gitlink"
    );
    assert!(
        gitlink_states[0]
            .submodule_stream
            .windows(b"nested".len())
            .any(|window| window == b"nested"),
        "outer gitlink payload must contain the nested gitlink stream"
    );
    assert_eq!(
        external_states.len(),
        4,
        "fixture must frame both external mounts"
    );

    fn field(bytes: &mut Vec<u8>, value: &[u8]) {
        bytes.extend_from_slice(&(value.len() as u64).to_le_bytes());
        bytes.extend_from_slice(value);
    }
    fn states(bytes: &mut Vec<u8>, values: &[WorktreeState]) {
        bytes.extend_from_slice(&(values.len() as u64).to_le_bytes());
        for value in values {
            field(bytes, &value.path);
            field(bytes, value.type_tag);
            field(bytes, value.mode_tag);
            field(bytes, &value.content);
        }
    }

    let mut golden = Vec::new();
    field(&mut golden, STREAM_MAGIC);
    field(&mut golden, STREAM_VERSION_TAG);
    golden.extend_from_slice(&VERIFICATION_INPUT_FINGERPRINT_VERSION_V1.to_le_bytes());
    field(&mut golden, merge_base.as_bytes());
    field(&mut golden, head.as_bytes());
    golden.extend_from_slice(&(index_entries.len() as u64).to_le_bytes());
    for entry in &index_entries {
        field(&mut golden, &entry.path);
        field(&mut golden, &entry.mode);
        golden.extend_from_slice(&entry.stage.to_le_bytes());
        field(&mut golden, entry.blob_sha.as_bytes());
    }
    states(&mut golden, &tracked_states);
    golden.extend_from_slice(&(gitlink_states.len() as u64).to_le_bytes());
    for gitlink in &gitlink_states {
        field(&mut golden, &gitlink.path);
        field(&mut golden, gitlink.committed_sha.as_bytes());
        field(&mut golden, &gitlink.submodule_stream);
    }
    states(&mut golden, &extra_states);
    golden.extend_from_slice(&(external_states.len() as u64).to_le_bytes());
    for external in &external_states {
        field(&mut golden, &external.id);
        field(&mut golden, &external.locator);
        field(&mut golden, &external.path);
        field(&mut golden, external.state.type_tag);
        field(&mut golden, external.state.mode_tag);
        field(&mut golden, &external.state.content);
    }
    assert_eq!(actual.canonical_stream_len, golden.len() as u64);
    assert_eq!(actual.fingerprint, sha256_hex(&golden));

    let submission_before = crate::compute_submission_diff_fingerprint(fixture.outer.path())
        .await
        .expect("compute submission fingerprint before external mutation");
    write_str(&alpha, "a/first.txt", "alpha-mutated\n");
    let changed = digest(configured_fingerprint(fixture.outer.path(), &config).await);
    let submission_after = crate::compute_submission_diff_fingerprint(fixture.outer.path())
        .await
        .expect("compute submission fingerprint after external mutation");
    assert_ne!(actual.fingerprint, changed.fingerprint);
    assert_eq!(
        submission_before, submission_after,
        "external manifest inputs must not change submission-diff fingerprint behavior"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_complete_v1_matrix_is_canonical_and_mutation_sensitive() {
    let fixture = make_nested_submodule_fixture("vendor", "nested");
    write_str(fixture.outer.path(), ".gitignore", "*.generated\n");
    git(fixture.outer.path(), ["add", ".gitignore"]);
    git(
        fixture.outer.path(),
        ["commit", "-m", "ignore generated input"],
    );
    write_str(fixture.outer.path(), "input.generated", "generated-v1\n");
    write(fixture.outer.path(), "input.bin", &[0, 1, 255, 254]);

    let mounts = tempfile::tempdir().expect("create external mount parent");
    let alpha = mounts.path().join("alpha");
    let zeta = mounts.path().join("zeta");
    std::fs::create_dir_all(&alpha).expect("create alpha mount");
    std::fs::create_dir_all(&zeta).expect("create zeta mount");
    // Deliberately create each tree in reverse bytewise order.
    write_str(&alpha, "z/last.txt", "alpha-last\n");
    write_str(&alpha, "a/first.txt", "alpha-first\n");
    write_str(&zeta, "z/last.txt", "zeta-last\n");
    write_str(&zeta, "a/first.txt", "zeta-first\n");

    let reversed_mounts = tempfile::tempdir().expect("create reverse external mount parent");
    let reverse_zeta = reversed_mounts.path().join("zeta");
    let reverse_alpha = reversed_mounts.path().join("alpha");
    // This equivalent fixture reverses both mount creation and child insertion.
    std::fs::create_dir_all(&reverse_zeta).expect("create reverse zeta mount");
    std::fs::create_dir_all(&reverse_alpha).expect("create reverse alpha mount");
    write_str(&reverse_zeta, "a/first.txt", "zeta-first\n");
    write_str(&reverse_zeta, "z/last.txt", "zeta-last\n");
    write_str(&reverse_alpha, "a/first.txt", "alpha-first\n");
    write_str(&reverse_alpha, "z/last.txt", "alpha-last\n");

    let ordered = complete_config("alpha", &alpha, "zeta", &zeta);
    let reversed = complete_config("zeta", &zeta, "alpha", &alpha);
    let reverse_tree = complete_config("zeta", &reverse_zeta, "alpha", &reverse_alpha);
    let baseline = digest(configured_fingerprint(fixture.outer.path(), &ordered).await);
    let reordered = digest(configured_fingerprint(fixture.outer.path(), &reversed).await);
    let reverse_tree_digest =
        digest(configured_fingerprint(fixture.outer.path(), &reverse_tree).await);
    assert_eq!(
        baseline.fingerprint, reordered.fingerprint,
        "manifest declaration and external enumeration order must not affect the complete V1 stream"
    );
    assert_eq!(
        baseline.fingerprint, reverse_tree_digest.fingerprint,
        "external mount and child creation order must not affect the complete V1 stream"
    );

    write_str(fixture.outer.path(), "input.generated", "generated-v2\n");
    assert_ne!(
        baseline.fingerprint,
        digest(configured_fingerprint(fixture.outer.path(), &ordered).await).fingerprint,
        "ignored generated input must affect the configured public API digest"
    );
    write_str(fixture.outer.path(), "input.generated", "generated-v1\n");

    write(fixture.outer.path(), "input.bin", &[0, 2, 255, 254]);
    assert_ne!(
        baseline.fingerprint,
        digest(configured_fingerprint(fixture.outer.path(), &ordered).await).fingerprint,
        "untracked binary bytes must affect the configured public API digest"
    );
    write(fixture.outer.path(), "input.bin", &[0, 1, 255, 254]);

    write_str(
        &fixture.outer.path().join("vendor"),
        "local.txt",
        "submodule dirty\n",
    );
    assert_ne!(
        baseline.fingerprint,
        digest(configured_fingerprint(fixture.outer.path(), &ordered).await).fingerprint,
        "submodule dirtiness must affect the configured public API digest"
    );
    std::fs::remove_file(fixture.outer.path().join("vendor/local.txt")).expect("restore submodule");

    write_str(
        &fixture.outer.path().join("vendor/nested"),
        "local.txt",
        "nested submodule dirty\n",
    );
    assert_ne!(
        baseline.fingerprint,
        digest(configured_fingerprint(fixture.outer.path(), &ordered).await).fingerprint,
        "nested-submodule dirtiness must affect the configured public API digest"
    );
    std::fs::remove_file(fixture.outer.path().join("vendor/nested/local.txt"))
        .expect("restore nested submodule");

    for (mount, path, changed) in [
        (&alpha, "a/first.txt", "alpha changed\n"),
        (&zeta, "a/first.txt", "zeta changed\n"),
    ] {
        write_str(mount, path, changed);
        assert_ne!(
            baseline.fingerprint,
            digest(configured_fingerprint(fixture.outer.path(), &ordered).await).fingerprint,
            "each declared external mount must affect the configured public API digest"
        );
        write_str(
            mount,
            path,
            if mount == &alpha {
                "alpha-first\n"
            } else {
                "zeta-first\n"
            },
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_output_only_absence_and_invalid_declarations_are_stable() {
    let fixture = init_repo_with_main_commit();
    let mut output_config = VerificationInputFingerprintConfig::default();
    output_config
        .manifest
        .output_only_globs
        .push("generated/**".into());
    assert!(
        !fixture.path().join("generated/result.txt").exists(),
        "validated generated outputs must begin absent before F0"
    );
    let absent = digest(configured_fingerprint(fixture.path(), &output_config).await);
    write_str(
        fixture.path(),
        "generated/result.txt",
        "output from prior pass\n",
    );
    let cleaned = digest(configured_fingerprint(fixture.path(), &output_config).await);
    assert_eq!(absent.fingerprint, cleaned.fingerprint);
    assert!(!fixture.path().join("generated/result.txt").exists());

    let mut overlap = VerificationInputFingerprintConfig::default();
    overlap.manifest.repo_paths.push("src".into());
    overlap.manifest.output_only_globs.push("src".into());
    let first_overlap = configured_fingerprint(fixture.path(), &overlap).await;
    let second_overlap = configured_fingerprint(fixture.path(), &overlap).await;
    assert!(first_overlap.is_unavailable());
    assert_eq!(
        first_overlap, second_overlap,
        "overlap failure must be stable"
    );

    let mut escape = VerificationInputFingerprintConfig::default();
    escape
        .manifest
        .output_only_globs
        .push("../generated/**".into());
    let first_escape = configured_fingerprint(fixture.path(), &escape).await;
    let second_escape = configured_fingerprint(fixture.path(), &escape).await;
    assert!(first_escape.is_unavailable());
    assert_eq!(
        first_escape, second_escape,
        "escaping failure must be stable"
    );
}
