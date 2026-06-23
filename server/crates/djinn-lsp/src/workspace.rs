use std::path::{Path, PathBuf};

/// How to pick the project root among ancestor directories containing a
/// root marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RootStrategy {
    /// Nearest ancestor containing a root marker (classic LSP behavior).
    Nearest,
    /// Topmost ancestor whose `Cargo.toml` declares a `[workspace]` table.
    /// In a cargo workspace the nearest `Cargo.toml` is the member crate's
    /// manifest — rooting there spawns one rust-analyzer per crate touched,
    /// and every instance indexes the whole workspace anyway (~5GB RSS and
    /// minutes of CPU each). Rooting at the workspace gives one shared
    /// instance. Falls back to the nearest marker for standalone crates.
    CargoWorkspace,
}

pub(super) fn find_root(
    path: &Path,
    worktree: &Path,
    sentinels: &[&str],
    strategy: RootStrategy,
) -> Option<PathBuf> {
    // Ancestor dirs containing a sentinel, nearest first, bounded by worktree.
    let mut hits: Vec<PathBuf> = Vec::new();
    let mut cur = path.parent()?.to_path_buf();
    loop {
        if sentinels.iter().any(|s| cur.join(s).exists()) {
            hits.push(cur.clone());
        }
        if cur == worktree || !cur.pop() {
            break;
        }
    }

    if hits.is_empty() {
        return Some(worktree.to_path_buf());
    }

    match strategy {
        RootStrategy::Nearest => hits.first().cloned(),
        RootStrategy::CargoWorkspace => hits
            .iter()
            .rev()
            .find(|dir| cargo_toml_declares_workspace(dir))
            .or_else(|| hits.first())
            .cloned(),
    }
}

fn cargo_toml_declares_workspace(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("Cargo.toml"))
        .map(|s| {
            s.lines().any(|l| {
                let t = l.trim();
                t.starts_with("[workspace]") || t.starts_with("[workspace.")
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_root_finds_cargo_toml() {
        let worktree = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let file = worktree.join("src/lib.rs");
        let root = find_root(&file, &worktree, &["Cargo.toml"], RootStrategy::Nearest);
        assert_eq!(root, Some(worktree));
    }

    #[test]
    fn find_root_falls_back_to_worktree() {
        let worktree = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let file = worktree.join("src/lib.rs");
        let root = find_root(
            &file,
            &worktree,
            &["nonexistent_marker.xyz"],
            RootStrategy::Nearest,
        );
        assert_eq!(root, Some(worktree));
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn cargo_workspace_prefers_workspace_root_over_member_crate() {
        let tmp = crate::test_helpers::test_tempdir("djinn-lsp-root-");
        let worktree = tmp.path();
        write(
            &worktree.join("server/Cargo.toml"),
            "[workspace]\nmembers = [\"crates/foo\"]\n",
        );
        write(
            &worktree.join("server/crates/foo/Cargo.toml"),
            "[package]\nname = \"foo\"\n",
        );
        let file = worktree.join("server/crates/foo/src/lib.rs");
        write(&file, "");

        let root = find_root(
            &file,
            worktree,
            &["Cargo.toml"],
            RootStrategy::CargoWorkspace,
        );
        assert_eq!(root, Some(worktree.join("server")));

        // Nearest strategy still picks the member crate.
        let nearest = find_root(&file, worktree, &["Cargo.toml"], RootStrategy::Nearest);
        assert_eq!(nearest, Some(worktree.join("server/crates/foo")));
    }

    #[test]
    fn cargo_workspace_detects_dotted_workspace_table() {
        let tmp = crate::test_helpers::test_tempdir("djinn-lsp-root-");
        let worktree = tmp.path();
        write(
            &worktree.join("Cargo.toml"),
            "[workspace.dependencies]\nserde = \"1\"\n",
        );
        write(
            &worktree.join("crates/foo/Cargo.toml"),
            "[package]\nname = \"foo\"\n",
        );
        let file = worktree.join("crates/foo/src/lib.rs");
        write(&file, "");

        let root = find_root(
            &file,
            worktree,
            &["Cargo.toml"],
            RootStrategy::CargoWorkspace,
        );
        assert_eq!(root, Some(worktree.to_path_buf()));
    }

    #[test]
    fn cargo_workspace_falls_back_to_nearest_for_standalone_crate() {
        let tmp = crate::test_helpers::test_tempdir("djinn-lsp-root-");
        let worktree = tmp.path();
        write(
            &worktree.join("app/Cargo.toml"),
            "[package]\nname = \"app\"\n",
        );
        let file = worktree.join("app/src/main.rs");
        write(&file, "");

        let root = find_root(
            &file,
            worktree,
            &["Cargo.toml"],
            RootStrategy::CargoWorkspace,
        );
        assert_eq!(root, Some(worktree.join("app")));
    }
}
