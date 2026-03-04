use std::path::{Component, Path, PathBuf};

pub fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn resolve_path(raw: &str, base: &Path) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        normalize_path(p)
    } else {
        normalize_path(&base.join(p))
    }
}

pub fn is_temp_path(path: &Path) -> bool {
    path.starts_with("/tmp") || path.starts_with("/var/tmp")
}

pub fn is_worktree_path(path: &Path) -> bool {
    let parts: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    parts
        .windows(2)
        .any(|w| w[0] == ".djinn" && w[1] == "worktrees")
}

pub fn enforce_workdir(workdir: &Path, external_dir: bool) -> Result<(), String> {
    if !workdir.exists() || !workdir.is_dir() {
        return Err(format!(
            "workdir does not exist or is not a directory: {}",
            workdir.display()
        ));
    }
    if external_dir {
        return Ok(());
    }
    if is_worktree_path(workdir) || is_temp_path(workdir) {
        return Ok(());
    }
    Err(format!(
        "workdir is outside task worktree: {}. Use external_dir=true only if intentionally accessing outside workspace.",
        workdir.display()
    ))
}

pub fn command_outside_workspace_paths(
    command: &str,
    workdir: &Path,
    external_dir: bool,
) -> Vec<PathBuf> {
    if external_dir {
        return Vec::new();
    }

    let mut out = Vec::new();
    for token in command.split_whitespace() {
        let tok = token
            .trim_matches('"')
            .trim_matches('\'')
            .trim_matches('`')
            .trim_end_matches(',')
            .trim_end_matches(';')
            .trim_end_matches(':');
        if tok.is_empty() {
            continue;
        }
        if tok == ".." || tok.starts_with("../") || tok.contains("/../") {
            out.push(resolve_path(tok, workdir));
            continue;
        }
        if tok.starts_with("~/") {
            out.push(PathBuf::from(tok));
            continue;
        }
        if tok.starts_with('/') {
            let p = resolve_path(tok, workdir);
            if !p.starts_with(workdir) && !is_temp_path(&p) {
                out.push(p);
            }
        }
    }
    out
}
