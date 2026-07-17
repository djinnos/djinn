//! Mirror-backed cross-repo read/search.
//!
//! Every task-run pod (and the server) has a `/mirror` PVC of **bare** git
//! mirrors for every registered project. Reading or grepping another repo does
//! NOT require a working checkout — git can serve file content and grep results
//! straight from a bare repo against a ref. This module wraps those plumbing
//! calls so `read(project=…)` and `code_search` can reach any registered repo
//! with zero clones. Heavy/interactive work (`shell(project=…)`) lazily
//! materializes a working tree elsewhere; this module never clones.

use std::path::PathBuf;

use djinn_git::{GitError, run_git_command};

/// Bare-mirror root on this host: `DJINN_MIRROR_ROOT` (the pod's `/mirror`
/// mount) when set, else the workspace default (`DJINN_HOME/mirrors` or
/// `~/.djinn/mirrors`). Mirrors the resolution `supervisor_impl/stage.rs` uses.
pub fn mirror_root() -> PathBuf {
    if let Ok(root) = std::env::var("DJINN_MIRROR_ROOT")
        && !root.is_empty()
    {
        return PathBuf::from(root);
    }
    djinn_workspace::mirrors_root()
}

/// Path to a project's bare mirror: `{mirror_root}/{project_id}.git`.
pub fn mirror_path(project_id: &str) -> PathBuf {
    mirror_root().join(format!("{project_id}.git"))
}

/// True when a project's bare mirror exists on this host.
pub fn mirror_exists(project_id: &str) -> bool {
    mirror_path(project_id).exists()
}

/// A single `code_search` hit.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GrepHit {
    pub file: String,
    pub line: u64,
    pub text: String,
}

/// Read a file from a project's bare mirror at `git_ref` (no working clone).
/// `git_ref` is typically the project's default branch (or `HEAD`).
pub async fn read_file(project_id: &str, git_ref: &str, path: &str) -> Result<String, String> {
    read_file_at(&mirror_path(project_id), git_ref, path).await
}

async fn read_file_at(
    mirror: &std::path::Path,
    git_ref: &str,
    path: &str,
) -> Result<String, String> {
    if !mirror.exists() {
        return Err(format!(
            "no mirror at {} — project may not be registered or not yet fetched",
            mirror.display()
        ));
    }
    let spec = format!("{git_ref}:{path}");
    match run_git_command(mirror.to_path_buf(), vec!["show".into(), spec]).await {
        Ok(out) => Ok(out.stdout),
        Err(GitError::CommandFailed { stderr, .. }) => {
            Err(format!("{path} not found at {git_ref}: {}", stderr.trim()))
        }
        Err(e) => Err(format!("git show failed: {e}")),
    }
}

/// `git grep` a project's bare mirror at `git_ref`. Returns at most
/// `max_results` hits (callers should note when truncated). Zero clones.
pub async fn grep(
    project_id: &str,
    git_ref: &str,
    query: &str,
    path: Option<&str>,
    ignore_case: bool,
    max_results: usize,
) -> Result<Vec<GrepHit>, String> {
    grep_at(
        &mirror_path(project_id),
        git_ref,
        query,
        path,
        ignore_case,
        max_results,
    )
    .await
}

async fn grep_at(
    mirror: &std::path::Path,
    git_ref: &str,
    query: &str,
    path: Option<&str>,
    ignore_case: bool,
    max_results: usize,
) -> Result<Vec<GrepHit>, String> {
    if !mirror.exists() {
        return Err(format!("no mirror at {}", mirror.display()));
    }
    // `git grep -n [-i] -e <query> <ref> [-- <path>]` against a bare repo emits
    // `<ref>:<file>:<line>:<text>`. We strip the ref prefix when parsing.
    let mut args: Vec<String> = vec!["grep".into(), "-n".into(), "-I".into()];
    if ignore_case {
        args.push("-i".into());
    }
    args.push("-e".into());
    args.push(query.to_string());
    args.push(git_ref.to_string());
    if let Some(p) = path.filter(|p| !p.is_empty()) {
        args.push("--".into());
        args.push(p.to_string());
    }

    let stdout = match run_git_command(mirror.to_path_buf(), args).await {
        Ok(out) => out.stdout,
        // git grep exits 1 with no output when there are simply no matches.
        Err(GitError::CommandFailed { code: 1, .. }) => return Ok(Vec::new()),
        Err(e) => return Err(format!("git grep failed: {e}")),
    };

    let prefix = format!("{git_ref}:");
    let hits = stdout
        .lines()
        .filter_map(|raw| parse_grep_line(raw, &prefix))
        .take(max_results)
        .collect();
    Ok(hits)
}

/// Materialize an immutable owner-project read-source cache from a bare mirror.
///
/// A checkout is built in a sibling staging directory, detached at `git_ref`,
/// then atomically published. Existing destinations are never reset, cleaned,
/// or replaced: partial, dirty, or unknown state fails closed and remains
/// visible for operator inspection.
pub async fn materialize_read_source(
    project_id: &str,
    git_ref: &str,
    dest: &std::path::Path,
) -> Result<(), String> {
    if dest.exists() {
        return Err(format!(
            "read-source destination already exists and will not be replaced: {}",
            dest.display()
        ));
    }
    let mirror = mirror_path(project_id);
    if !mirror.exists() {
        return Err(format!(
            "no mirror for project {project_id} on this host — cannot check it out"
        ));
    }
    let parent = dest
        .parent()
        .ok_or_else(|| format!("read-source destination has no parent: {}", dest.display()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|e| format!("create read-source parent failed: {e}"))?;
    let staging = parent.join(format!(
        ".{}.staging-{}",
        dest.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("read-source"),
        uuid::Uuid::now_v7()
    ));
    let args: Vec<String> = vec![
        "clone".into(),
        "--local".into(),
        "--shared".into(),
        "--no-checkout".into(),
        mirror.display().to_string(),
        staging.display().to_string(),
    ];
    run_git_command(mirror_root(), args)
        .await
        .map_err(|e| format!("git clone --local failed: {e}"))?;
    if let Err(e) = run_git_command(
        staging.clone(),
        vec!["checkout".into(), "--detach".into(), git_ref.to_string()],
    )
    .await
    {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return Err(format!("git checkout --detach failed: {e}"));
    }
    tokio::fs::rename(&staging, dest)
        .await
        .map_err(|e| format!("atomic read-source publish failed: {e}"))
}

/// Parse one `git grep -n` line of the form `<ref>:<file>:<line>:<text>`.
fn parse_grep_line(raw: &str, ref_prefix: &str) -> Option<GrepHit> {
    let rest = raw.strip_prefix(ref_prefix)?;
    // `<file>:<line>:<text>` — file paths can contain ':' but the line number
    // is the first all-digit field, so split from the left on the first two
    // colons and validate the middle is numeric.
    let (file, after) = rest.split_once(':')?;
    let (line_s, text) = after.split_once(':')?;
    let line: u64 = line_s.parse().ok()?;
    Some(GrepHit {
        file: file.to_string(),
        line,
        text: text.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_git::run_git_command;
    use std::path::Path;

    #[test]
    fn parse_grep_line_strips_ref_and_parses_lineno() {
        let hit = parse_grep_line(
            "main:src/a.rs:42:    let x = NewGrpcService::new();",
            "main:",
        )
        .expect("should parse");
        assert_eq!(hit.file, "src/a.rs");
        assert_eq!(hit.line, 42);
        assert!(hit.text.contains("NewGrpcService"));
        // Wrong ref prefix / malformed lines are skipped.
        assert!(parse_grep_line("other:src/a.rs:1:x", "main:").is_none());
        assert!(parse_grep_line("main:noline", "main:").is_none());
    }

    async fn seed_bare_mirror(dir: &Path) -> std::path::PathBuf {
        // Build a normal repo with a commit, then `clone --bare` it so the
        // mirror has real refs/heads/main — exactly the shape /mirror holds.
        let work = dir.join("work");
        let bare = dir.join("proj.git");
        std::fs::create_dir_all(&work).unwrap();
        let g = |args: Vec<String>, cwd: std::path::PathBuf| async move {
            run_git_command(cwd, args).await.unwrap();
        };
        g(
            vec!["init".into(), "-b".into(), "main".into(), ".".into()],
            work.clone(),
        )
        .await;
        g(
            vec!["config".into(), "user.email".into(), "t@t".into()],
            work.clone(),
        )
        .await;
        g(
            vec!["config".into(), "user.name".into(), "t".into()],
            work.clone(),
        )
        .await;
        std::fs::create_dir_all(work.join("src")).unwrap();
        std::fs::write(
            work.join("src/a.rs"),
            "fn main() { NewGrpcService::new(); }\n",
        )
        .unwrap();
        g(vec!["add".into(), "-A".into()], work.clone()).await;
        g(
            vec!["commit".into(), "-m".into(), "init".into()],
            work.clone(),
        )
        .await;
        g(
            vec![
                "clone".into(),
                "--bare".into(),
                work.display().to_string(),
                bare.display().to_string(),
            ],
            dir.to_path_buf(),
        )
        .await;
        bare
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_and_grep_against_bare_mirror_no_checkout() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bare = seed_bare_mirror(tmp.path()).await;

        // read_file serves content straight from the bare repo.
        let body = read_file_at(&bare, "main", "src/a.rs").await.unwrap();
        assert!(body.contains("NewGrpcService"));

        // grep finds it with structured hits.
        let hits = grep_at(&bare, "main", "NewGrpcService", None, false, 100)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file, "src/a.rs");
        assert_eq!(hits[0].line, 1);

        // No matches → empty (not an error).
        let none = grep_at(&bare, "main", "ZZZ_nope", None, false, 100)
            .await
            .unwrap();
        assert!(none.is_empty());

        // Missing file → clean error.
        assert!(read_file_at(&bare, "main", "src/missing.rs").await.is_err());

        // Crucially, no working tree was ever created next to the bare repo.
        assert!(!bare.join("src").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ensure_worktree_lazily_checks_out_and_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bare = seed_bare_mirror(tmp.path()).await;
        let dest = tmp.path().join("checkout");

        // First call materializes a real working tree.
        ensure_worktree_at(&bare, "main", &dest).await.unwrap();
        assert!(dest.join(".git").exists());
        assert!(dest.join("src/a.rs").exists());

        // Second call is a no-op (cached) — does not error on the existing dir.
        ensure_worktree_at(&bare, "main", &dest).await.unwrap();
        assert!(dest.join("src/a.rs").exists());
    }

    /// Test seam: `ensure_worktree` resolves the mirror via env; this variant
    /// takes an explicit bare-repo path so tests avoid env races.
    async fn ensure_worktree_at(mirror: &Path, git_ref: &str, dest: &Path) -> Result<(), String> {
        if dest.join(".git").exists() {
            return Ok(());
        }
        let _ = tokio::fs::remove_dir_all(dest).await;
        let args = vec![
            "clone".into(),
            "--local".into(),
            "--shared".into(),
            "--branch".into(),
            git_ref.to_string(),
            mirror.display().to_string(),
            dest.display().to_string(),
        ];
        run_git_command(mirror.parent().unwrap().to_path_buf(), args)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}
