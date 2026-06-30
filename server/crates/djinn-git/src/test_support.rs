#![allow(dead_code)]

use std::{ffi::OsStr, path::Path};

use tempfile::TempDir;

use crate::actor::GitActorHandle;

/// Isolated git repository fixture for tests.
///
/// The fixture owns the temporary directories backing both the local worktree and
/// an optional bare remote so paths remain valid for the full test duration.
pub(crate) struct TestRepoFixture {
    pub(crate) local: TempDir,
    pub(crate) remote: Option<TempDir>,
}

impl TestRepoFixture {
    pub(crate) fn path(&self) -> &Path {
        self.local.path()
    }

    pub(crate) fn remote_path(&self) -> Option<&Path> {
        self.remote.as_ref().map(TempDir::path)
    }

    pub(crate) fn spawn_handle(&self) -> GitActorHandle {
        GitActorHandle::spawn(self.path().to_path_buf()).expect("failed to spawn git actor")
    }
}

/// Run `git <args>` in `repo_path`, panicking with full command output on failure.
pub(crate) fn git<I, S>(repo_path: &Path, args: I) -> std::process::Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<std::ffi::OsString> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect();

    let output = std::process::Command::new("git")
        .args(&args)
        .current_dir(repo_path)
        .output()
        .unwrap_or_else(|err| {
            panic!(
                "failed to run git {:?} in {}: {err}",
                args,
                repo_path.display()
            )
        });

    assert!(
        output.status.success(),
        "git {:?} failed in {} with status {:?}\nstdout:\n{}\nstderr:\n{}",
        args,
        repo_path.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    output
}

/// Create a local repo with an initial commit on an explicitly-renamed `main` branch.
pub(crate) fn init_repo_with_main_commit() -> TestRepoFixture {
    let local = tempfile::tempdir().expect("create local temp dir");
    git(local.path(), ["init"]);
    configure_local_identity(local.path());
    write_and_commit(local.path(), "README.md", "hello\n", "init");
    git(local.path(), ["branch", "-m", "main"]);

    TestRepoFixture {
        local,
        remote: None,
    }
}

/// Create a local repo on `main`, attach a local bare `origin`, and push `main`.
pub(crate) fn init_repo_with_bare_origin() -> TestRepoFixture {
    let remote = tempfile::tempdir().expect("create remote temp dir");
    git(remote.path(), ["init", "--bare"]);

    let fixture = init_repo_with_main_commit();
    let remote_path = remote
        .path()
        .to_str()
        .expect("remote temp path should be valid UTF-8");
    git(fixture.path(), ["remote", "add", "origin", remote_path]);
    git(fixture.path(), ["push", "-u", "origin", "main"]);

    TestRepoFixture {
        local: fixture.local,
        remote: Some(remote),
    }
}

/// Configure local-only git identity and signing settings required for CI commits.
pub(crate) fn configure_local_identity(repo_path: &Path) {
    for (key, value) in [
        ("user.email", "test@test.com"),
        ("user.name", "Test User"),
        ("commit.gpgsign", "false"),
    ] {
        git(repo_path, ["config", "--local", key, value]);
    }
}

/// Write a file, stage it, and commit it using the fixture's local git config.
pub(crate) fn write_and_commit(
    repo_path: &Path,
    relative_path: &str,
    contents: &str,
    message: &str,
) {
    let path = repo_path.join(relative_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent directories for committed file");
    }
    std::fs::write(&path, contents).expect("write committed file");
    git(repo_path, ["add", relative_path]);
    git(repo_path, ["commit", "-m", message]);
}

/// Check out an existing branch or create it from `start_point` when provided.
pub(crate) fn checkout_branch(repo_path: &Path, branch: &str, start_point: Option<&str>) {
    match start_point {
        Some(start_point) => {
            git(repo_path, ["checkout", "-b", branch, start_point]);
        }
        None => {
            git(repo_path, ["checkout", branch]);
        }
    }
}
