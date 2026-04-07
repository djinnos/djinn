use std::path::{Path, PathBuf};

pub(super) fn find_root(path: &Path, worktree: &Path, sentinels: &[&str]) -> Option<PathBuf> {
    let mut cur = path.parent()?.to_path_buf();
    loop {
        for sentinel in sentinels {
            if cur.join(sentinel).exists() {
                return Some(cur.clone());
            }
        }
        if cur == worktree {
            return Some(worktree.to_path_buf());
        }
        if !cur.pop() {
            return Some(worktree.to_path_buf());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_root_finds_cargo_toml() {
        let worktree = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let file = worktree.join("src/agent/lsp.rs");
        let root = find_root(&file, &worktree, &["Cargo.toml"]);
        assert_eq!(root, Some(worktree));
    }

    #[test]
    fn find_root_falls_back_to_worktree() {
        let worktree = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let file = worktree.join("src/agent/lsp.rs");
        let root = find_root(&file, &worktree, &["nonexistent_marker.xyz"]);
        assert_eq!(root, Some(worktree));
    }
}
