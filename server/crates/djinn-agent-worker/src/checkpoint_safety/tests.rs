use super::*;

// ── Classification tests ────────────────────────────────────────────

#[test]
fn classify_tracked_modified_file_is_eligible() {
    let config = CheckpointSafetyConfig::default();
    let result = classify_path("src/main.rs", "M", Some(1024), &config);
    assert_eq!(result.classification, FileClassification::Tracked);
    assert!(result.exclusion_reason.is_none());
}

#[test]
fn classify_untracked_source_file_is_eligible() {
    let config = CheckpointSafetyConfig::default();
    let result = classify_path("src/new_module.rs", "??", Some(512), &config);
    assert_eq!(result.classification, FileClassification::Untracked);
    assert!(result.exclusion_reason.is_none());
}

#[test]
fn classify_target_dir_is_excluded_as_generated() {
    let config = CheckpointSafetyConfig::default();
    let result = classify_path("target/debug/deps/libfoo.rlib", "??", Some(1024), &config);
    assert_eq!(result.classification, FileClassification::Generated);
    assert!(result.exclusion_reason.is_some());
}

#[test]
fn classify_node_modules_is_excluded_as_generated() {
    let config = CheckpointSafetyConfig::default();
    let result = classify_path("node_modules/react/index.js", "??", Some(1024), &config);
    assert_eq!(result.classification, FileClassification::Generated);
    assert!(result.exclusion_reason.is_some());
}

#[test]
fn classify_log_file_is_excluded_by_pattern() {
    let config = CheckpointSafetyConfig::default();
    let result = classify_path("app.log", "??", Some(1024), &config);
    assert_eq!(result.classification, FileClassification::Generated);
    assert_eq!(
        result.exclusion_reason,
        Some(ExclusionReason::GeneratedPath)
    );
}

#[test]
fn classify_nested_log_file_is_excluded_by_pattern() {
    let config = CheckpointSafetyConfig::default();
    let result = classify_path("logs/app.log", "??", Some(1024), &config);
    assert_eq!(result.classification, FileClassification::Generated);
}

#[test]
fn classify_coverage_dir_is_excluded() {
    let config = CheckpointSafetyConfig::default();
    let result = classify_path("coverage/lcov.info", "??", Some(1024), &config);
    assert_eq!(result.classification, FileClassification::Generated);
}

#[test]
fn classify_pyc_file_is_excluded_by_extension() {
    let config = CheckpointSafetyConfig::default();
    let result = classify_path("app/models.pyc", "??", Some(1024), &config);
    assert_eq!(result.classification, FileClassification::Generated);
    assert!(result.exclusion_reason.is_some());
}

#[test]
fn classify_object_file_is_excluded_by_extension() {
    let config = CheckpointSafetyConfig::default();
    let result = classify_path("build/foo.o", "??", Some(1024), &config);
    assert_eq!(result.classification, FileClassification::Generated);
}

#[test]
fn classify_large_binary_is_excluded() {
    let config = CheckpointSafetyConfig::default();
    let large_size = config.large_binary_threshold + 1;
    let result = classify_path("data/model.bin", "??", Some(large_size), &config);
    assert_eq!(result.classification, FileClassification::Generated);
    assert_eq!(
        result.exclusion_reason,
        Some(ExclusionReason::LargeBinary {
            size_bytes: large_size
        })
    );
}

#[test]
fn classify_file_at_threshold_is_excluded() {
    let config = CheckpointSafetyConfig::default();
    let result = classify_path(
        "data/model.bin",
        "??",
        Some(config.large_binary_threshold),
        &config,
    );
    assert_eq!(result.classification, FileClassification::Generated);
}

#[test]
fn classify_file_below_threshold_is_eligible() {
    let config = CheckpointSafetyConfig::default();
    let result = classify_path(
        "data/model.bin",
        "??",
        Some(config.large_binary_threshold - 1),
        &config,
    );
    assert_eq!(result.classification, FileClassification::Untracked);
    assert!(result.exclusion_reason.is_none());
}

#[test]
fn classify_git_ignored_file_is_excluded() {
    let config = CheckpointSafetyConfig::default();
    let result = classify_path("secret.tmp", "!!", Some(100), &config);
    assert_eq!(result.classification, FileClassification::Ignored);
    assert_eq!(result.exclusion_reason, Some(ExclusionReason::GitIgnored));
}

#[test]
fn classify_dotenv_file_is_not_excluded_but_will_be_blocked() {
    // `.env` is not in the generated patterns — it's a real config file
    // that should be blocked by the safety scan, not silently excluded.
    let config = CheckpointSafetyConfig::default();
    let result = classify_path(".env", "??", Some(100), &config);
    // It might match `.cache/` prefix? No. Let's check: `.env` doesn't
    // match any excluded pattern, so it should be Untracked (eligible
    // for staging, but will be caught by scan_path_for_blocks).
    assert_eq!(result.classification, FileClassification::Untracked);
    assert!(result.exclusion_reason.is_none());
}

// ── Path pattern tests ──────────────────────────────────────────────

#[test]
fn is_generated_path_matches_target_prefix() {
    let config = CheckpointSafetyConfig::default();
    assert!(is_generated_path("target/debug/foo", &config));
    assert!(is_generated_path("target/foo", &config));
}

#[test]
fn is_generated_path_matches_nested_target() {
    let config = CheckpointSafetyConfig::default();
    assert!(is_generated_path("workspace/target/debug/foo", &config));
}

#[test]
fn is_generated_path_matches_log_glob() {
    let config = CheckpointSafetyConfig::default();
    assert!(is_generated_path("app.log", &config));
    assert!(is_generated_path("logs/debug.log", &config));
}

#[test]
fn is_generated_path_does_not_match_source() {
    let config = CheckpointSafetyConfig::default();
    assert!(!is_generated_path("src/main.rs", &config));
    assert!(!is_generated_path("README.md", &config));
}

#[test]
fn has_generated_dir_component_detects_nested() {
    let config = CheckpointSafetyConfig::default();
    assert!(has_generated_dir_component("foo/node_modules/bar", &config));
    assert!(has_generated_dir_component("node_modules/bar", &config));
    assert!(!has_generated_dir_component("src/main.rs", &config));
}

#[test]
fn has_generated_extension_handles_dotfiles() {
    let config = CheckpointSafetyConfig::default();
    // `.gitignore` should not match the `ignore` extension (it's a dotfile).
    assert!(!has_generated_extension(".gitignore", &config));
    // But `.pyc` should match even without a directory.
    assert!(has_generated_extension("foo.pyc", &config));
}

// ── Secret scanning tests ───────────────────────────────────────────

#[test]
fn scan_content_detects_aws_key() {
    let config = CheckpointSafetyConfig::default();
    let content = "aws_key = AKIAIOSFODNN7EXAMPLE\n";
    let findings = scan_content_for_secrets("config.txt", content, &config);
    assert!(!findings.is_empty());
    assert_eq!(findings[0].kind, SafetyFindingKind::SecretContent);
    assert_eq!(findings[0].line, 1);
    // The snippet must be redacted.
    assert!(
        findings[0]
            .snippet
            .as_ref()
            .unwrap()
            .contains("***REDACTED***"),
        "snippet must be redacted"
    );
    assert!(
        !findings[0]
            .snippet
            .as_ref()
            .unwrap()
            .contains("AKIAIOSFODNN7EXAMPLE"),
        "redacted snippet must not contain the actual key"
    );
}

#[test]
fn scan_content_detects_private_key() {
    let config = CheckpointSafetyConfig::default();
    let content = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAI...\n-----END RSA PRIVATE KEY-----\n";
    let findings = scan_content_for_secrets("key.pem", content, &config);
    assert!(!findings.is_empty());
    assert_eq!(findings[0].line, 1);
}

#[test]
fn scan_content_detects_github_token() {
    let config = CheckpointSafetyConfig::default();
    let content = "token = ghp_1234567890abcdefghijklmnopqrstuvwxyz\n";
    let findings = scan_content_for_secrets("config.txt", content, &config);
    assert!(!findings.is_empty());
}

#[test]
fn scan_content_detects_generic_password() {
    let config = CheckpointSafetyConfig::default();
    let content = "password = supersecret123\n";
    let findings = scan_content_for_secrets("config.txt", content, &config);
    assert!(!findings.is_empty());
    assert!(
        findings[0]
            .snippet
            .as_ref()
            .unwrap()
            .contains("***REDACTED***"),
        "password must be redacted"
    );
}

#[test]
fn scan_content_detects_connection_string() {
    let config = CheckpointSafetyConfig::default();
    let content = "DATABASE_URL=postgres://user:secretpass@localhost:5432/db\n";
    let findings = scan_content_for_secrets("config.txt", content, &config);
    assert!(!findings.is_empty());
}

#[test]
fn scan_content_does_not_flag_normal_code() {
    let config = CheckpointSafetyConfig::default();
    let content = "fn main() {\n    println!(\"hello world\");\n}\n";
    let findings = scan_content_for_secrets("src/main.rs", content, &config);
    assert!(
        findings.is_empty(),
        "normal source code should not trigger secret findings"
    );
}

#[test]
fn scan_content_does_not_flag_word_password_in_comment() {
    let config = CheckpointSafetyConfig::default();
    let content = "// This function handles password reset logic\nfn reset() {}\n";
    let findings = scan_content_for_secrets("src/auth.rs", content, &config);
    // "password reset logic" doesn't have an assignment with a value,
    // so it should not trigger (the pattern requires a value after =).
    assert!(
        findings.is_empty(),
        "the word 'password' in a comment without a value should not trigger"
    );
}

#[test]
fn scan_path_blocks_env_file() {
    let config = CheckpointSafetyConfig::default();
    let findings = scan_path_for_blocks(".env", &config);
    assert!(!findings.is_empty());
    assert_eq!(findings[0].kind, SafetyFindingKind::BlockedPath);
}

#[test]
fn scan_path_blocks_credentials_file() {
    let config = CheckpointSafetyConfig::default();
    let findings = scan_path_for_blocks("config/credentials.yml", &config);
    assert!(!findings.is_empty());
}

#[test]
fn scan_path_blocks_rsa_key() {
    let config = CheckpointSafetyConfig::default();
    let findings = scan_path_for_blocks("~/.ssh/id_rsa", &config);
    assert!(!findings.is_empty());
}

#[test]
fn scan_path_does_not_block_source() {
    let config = CheckpointSafetyConfig::default();
    let findings = scan_path_for_blocks("src/main.rs", &config);
    assert!(findings.is_empty());
}

// ── Fingerprint tests ───────────────────────────────────────────────

#[test]
fn diff_fingerprint_is_deterministic() {
    let a = diff_fingerprint("hello world");
    let b = diff_fingerprint("hello world");
    assert_eq!(a, b);
}

#[test]
fn diff_fingerprint_changes_with_content() {
    let a = diff_fingerprint("hello world");
    let b = diff_fingerprint("hello World");
    assert_ne!(a, b);
}

#[test]
fn path_set_fingerprint_is_order_independent() {
    let a = path_set_fingerprint(&["c", "a", "b"]);
    let b = path_set_fingerprint(&["a", "b", "c"]);
    assert_eq!(a, b);
}

// ── Porcelain parsing tests ─────────────────────────────────────────

#[test]
fn parse_porcelain_modified_file() {
    let (status, path) = parse_porcelain_line(" M src/main.rs").unwrap();
    assert_eq!(status, " M");
    assert_eq!(path, "src/main.rs");
}

#[test]
fn parse_porcelain_untracked_file() {
    let (status, path) = parse_porcelain_line("?? src/new.rs").unwrap();
    assert_eq!(status, "??");
    assert_eq!(path, "src/new.rs");
}

#[test]
fn parse_porcelain_ignored_file() {
    let (status, path) = parse_porcelain_line("!! secret.tmp").unwrap();
    assert_eq!(status, "!!");
    assert_eq!(path, "secret.tmp");
}

#[test]
fn parse_porcelain_quoted_path() {
    let (status, path) = parse_porcelain_line("?? \"my file.txt\"").unwrap();
    assert_eq!(status, "??");
    assert_eq!(path, "my file.txt");
}

#[test]
fn parse_porcelain_empty_line_returns_none() {
    assert!(parse_porcelain_line("").is_none());
}

// ── Integration tests with real git repos ───────────────────────────

/// Helper: run git in a directory (same pattern as main.rs tests).
fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Set up a minimal git repo with an initial commit on `main`.
fn setup_repo() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let p = dir.path();
    git(p, &["init", "-b", "main"]);
    std::fs::write(p.join("base.txt"), "base\n").unwrap();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-m", "base"]);
    dir
}

/// Write a file, creating parent directories as needed.
fn write_file(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, content).unwrap();
}

/// Write binary content to a file, creating parent directories as needed.
fn write_bytes(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, content).unwrap();
}

#[tokio::test]
async fn scan_clean_worktree_returns_empty_result() {
    let dir = setup_repo();
    let config = CheckpointSafetyConfig::default();
    let result = scan_worktree(dir.path(), "main", &config)
        .await
        .expect("scan");
    assert!(!result.had_changes);
    assert!(result.staged.is_empty());
    assert!(result.excluded.is_empty());
    assert!(result.blocked.is_empty());
    assert!(result.fingerprints.head_sha.is_some());
}

#[tokio::test]
async fn scan_classifies_source_change_as_staged() {
    let dir = setup_repo();
    let p = dir.path();
    // Modify a tracked file + add a new untracked source file.
    std::fs::write(p.join("base.txt"), "modified\n").unwrap();
    write_file(p, "src/new.rs", "fn main() {}\n");

    let config = CheckpointSafetyConfig::default();
    let result = scan_worktree(p, "main", &config).await.expect("scan");

    assert!(result.had_changes);
    assert!(result.staged.contains(&"base.txt".to_string()));
    assert!(result.staged.contains(&"src/new.rs".to_string()));
    assert!(result.blocked.is_empty());
    assert!(result.fingerprints.worktree_diff.is_some());
}

#[tokio::test]
async fn scan_excludes_generated_paths() {
    let dir = setup_repo();
    let p = dir.path();
    // Create generated/cache files alongside real source.
    write_file(p, "src/main.rs", "fn main() {}\n");
    std::fs::create_dir_all(p.join("target/debug")).unwrap();
    std::fs::write(p.join("target/debug/deps.lib"), "binary junk").unwrap();
    std::fs::create_dir_all(p.join("node_modules/react")).unwrap();
    std::fs::write(
        p.join("node_modules/react/index.js"),
        "module.exports = {};\n",
    )
    .unwrap();
    std::fs::write(p.join("app.log"), "[INFO] something\n").unwrap();

    let config = CheckpointSafetyConfig::default();
    let result = scan_worktree(p, "main", &config).await.expect("scan");

    // Source is staged.
    assert!(result.staged.contains(&"src/main.rs".to_string()));
    // Generated files are excluded.
    let excluded_paths: Vec<&str> = result.excluded.iter().map(|e| e.path.as_str()).collect();
    assert!(
        excluded_paths.iter().any(|p| p.starts_with("target/")),
        "target/ must be excluded: {excluded_paths:?}"
    );
    assert!(
        excluded_paths
            .iter()
            .any(|p| p.starts_with("node_modules/")),
        "node_modules/ must be excluded: {excluded_paths:?}"
    );
    assert!(
        excluded_paths.contains(&"app.log"),
        "app.log must be excluded: {excluded_paths:?}"
    );
    // None of the generated files leak into staged.
    assert!(
        !result.staged.iter().any(|s| s.starts_with("target/")),
        "target/ must not be staged"
    );
    assert!(
        !result.staged.iter().any(|s| s.starts_with("node_modules/")),
        "node_modules/ must not be staged"
    );
}

#[tokio::test]
async fn scan_blocks_secret_content() {
    let dir = setup_repo();
    let p = dir.path();
    // A file with a secret-like value.
    std::fs::write(p.join("config.txt"), "api_key = AKIAIOSFODNN7EXAMPLE\n").unwrap();

    let config = CheckpointSafetyConfig::default();
    let result = scan_worktree(p, "main", &config).await.expect("scan");

    // The file is staged (it's a real source-like file, not generated)...
    assert!(result.staged.contains(&"config.txt".to_string()));
    // ...but the safety scan found a secret.
    assert!(!result.blocked.is_empty(), "secret must be blocked");
    assert!(
        result
            .blocked
            .iter()
            .any(|f| f.path == "config.txt" && f.kind == SafetyFindingKind::SecretContent),
        "must have a secret content finding for config.txt"
    );
}

#[tokio::test]
async fn scan_blocks_env_file_by_path() {
    let dir = setup_repo();
    let p = dir.path();
    std::fs::write(p.join(".env"), "DATABASE_URL=postgres://localhost/db\n").unwrap();

    let config = CheckpointSafetyConfig::default();
    let result = scan_worktree(p, "main", &config).await.expect("scan");

    // `.env` is staged (not generated) but blocked by path.
    assert!(result.staged.contains(&".env".to_string()));
    assert!(
        result
            .blocked
            .iter()
            .any(|f| f.path == ".env" && f.kind == SafetyFindingKind::BlockedPath),
        "must have a blocked-path finding for .env"
    );
}

#[tokio::test]
async fn scan_excludes_large_binary() {
    let dir = setup_repo();
    let p = dir.path();
    // Create a file just above the threshold.
    let config = CheckpointSafetyConfig::default();
    let large_content = vec![0u8; (config.large_binary_threshold + 1) as usize];
    write_bytes(p, "data/model.bin", &large_content);

    let result = scan_worktree(p, "main", &config).await.expect("scan");

    assert!(
        !result.staged.contains(&"data/model.bin".to_string()),
        "large binary must not be staged"
    );
    assert!(
        result.excluded.iter().any(|e| e.path == "data/model.bin"
            && matches!(e.reason, ExclusionReason::LargeBinary { .. })),
        "large binary must be excluded with LargeBinary reason"
    );
}

#[tokio::test]
async fn scan_computes_fingerprints() {
    let dir = setup_repo();
    let p = dir.path();
    write_file(p, "src/main.rs", "fn main() {}\n");

    let config = CheckpointSafetyConfig::default();
    let result = scan_worktree(p, "main", &config).await.expect("scan");

    assert!(
        result.fingerprints.head_sha.is_some(),
        "head_sha must be set"
    );
    assert!(
        result.fingerprints.worktree_diff.is_some(),
        "worktree_diff fingerprint must be set for dirty tree"
    );
    assert!(
        result.fingerprints.staged_diff.is_some(),
        "staged_diff fingerprint must be set when files are staged"
    );
}

#[tokio::test]
async fn scan_is_deterministic() {
    let dir = setup_repo();
    let p = dir.path();
    write_file(p, "src/main.rs", "fn main() {}\n");
    write_file(p, "target/debug/foo", "junk");

    let config = CheckpointSafetyConfig::default();
    let result1 = scan_worktree(p, "main", &config).await.expect("scan 1");
    let result2 = scan_worktree(p, "main", &config).await.expect("scan 2");

    assert_eq!(result1.staged, result2.staged);
    assert_eq!(result1.excluded, result2.excluded);
    assert_eq!(result1.blocked, result2.blocked);
    assert_eq!(result1.fingerprints, result2.fingerprints);
}

#[tokio::test]
async fn scan_includes_gitignored_in_excluded_summary() {
    let dir = setup_repo();
    let p = dir.path();
    // Add a .gitignore that ignores a file, then create it.
    std::fs::write(p.join(".gitignore"), "*.tmp\n").unwrap();
    git(p, &["add", ".gitignore"]);
    git(p, &["commit", "-m", "add gitignore"]);
    std::fs::write(p.join("cache.tmp"), "ignored content\n").unwrap();

    let config = CheckpointSafetyConfig::default();
    let result = scan_worktree(p, "main", &config).await.expect("scan");

    // The ignored file appears in the excluded summary.
    assert!(
        result
            .excluded
            .iter()
            .any(|e| e.path == "cache.tmp" && e.reason == ExclusionReason::GitIgnored),
        "git-ignored file must appear in excluded summary: {:?}",
        result.excluded
    );
}

#[tokio::test]
async fn scan_with_custom_config_overrides_defaults() {
    let dir = setup_repo();
    let p = dir.path();
    write_file(p, "mybuild/output.txt", "build output\n");

    // Custom config that excludes `mybuild/`.
    let config = CheckpointSafetyConfig {
        excluded_path_patterns: vec!["mybuild/"],
        excluded_dir_components: vec!["mybuild"],
        ..Default::default()
    };
    let result = scan_worktree(p, "main", &config).await.expect("scan");

    assert!(
        result
            .excluded
            .iter()
            .any(|e| e.path.starts_with("mybuild/")),
        "custom config must exclude mybuild/"
    );
}
