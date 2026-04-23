//! Core detection pass.
//!
//! The public entrypoint is [`detect`]: given a path to a mirror
//! directory (bare git repo or plain directory tree), enumerate files,
//! classify by extension + filename, parse the handful of manifests we
//! care about, and emit a [`Stack`].
//!
//! Performance budget: the plan allows up to 1s for a 50k-file repo.
//! We hit that by (a) enumerating via `git ls-tree -r HEAD` or a
//! filesystem walk with git-ignore-style skips, not a content scan;
//! (b) reading bodies only for a small, enumerated set of manifests.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;

use crate::frameworks::{self, framework_for_dep};
use crate::languages::LanguageTable;
use crate::manifests;
use crate::schema::{LanguageStat, ManifestSignals, Runtimes, Stack, StackWorkspace};
use crate::test_runners::{self, NEXTEST_CONFIG_PATHS, runner_for_dep};

/// Files whose names are significant regardless of extension.
const MANIFEST_FILENAMES: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "rust-toolchain.toml",
    "pyproject.toml",
    "go.mod",
    "Gemfile",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "pnpm-workspace.yaml",
    "turbo.json",
];

/// Directories we never descend into during the filesystem walk.
/// (The git path skips these implicitly — tracked files only.)
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "venv",
    ".next",
    ".turbo",
    ".cache",
    "vendor",
];

/// Public entrypoint. `mirror_path` may be either a bare git mirror
/// (`.git` layout) or a plain working-tree directory. We prefer git
/// enumeration when HEAD is resolvable.
pub async fn detect(mirror_path: &Path) -> Result<Stack> {
    let path = mirror_path.to_path_buf();
    tokio::task::spawn_blocking(move || detect_blocking(&path))
        .await
        .context("detect spawn_blocking join")?
}

/// Blocking variant exposed for callers that already own a
/// `spawn_blocking` thread (e.g. the mirror-fetcher).
pub fn detect_blocking(mirror_path: &Path) -> Result<Stack> {
    let files = enumerate(mirror_path)?;
    let bodies = read_manifest_bodies(mirror_path, &files)?;
    let mut stack = build_stack(&files, &bodies);
    stack.workspaces = discover_workspaces_blocking(mirror_path, &files);
    Ok(stack)
}

/// One entry per tracked file: path (forward-slash, relative to root)
/// + size in bytes.
#[derive(Debug, Clone)]
struct FileEntry {
    path: String,
    size: u64,
}

fn enumerate(root: &Path) -> Result<Vec<FileEntry>> {
    match enumerate_git(root) {
        Ok(entries) if !entries.is_empty() => Ok(entries),
        _ => enumerate_fs(root),
    }
}

/// `git ls-tree -r HEAD` via git2. Works on both bare mirrors and
/// working-tree repos. Returns `Ok(vec![])` when HEAD is unresolvable
/// (fresh clone with no commits); caller falls back to filesystem.
fn enumerate_git(root: &Path) -> Result<Vec<FileEntry>> {
    let repo = git2::Repository::open(root).or_else(|_| git2::Repository::open_bare(root))?;
    let head = match repo.head() {
        Ok(h) => h,
        Err(_) => return Ok(Vec::new()),
    };
    let tree = head.peel_to_tree()?;

    let mut out = Vec::new();
    tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
        let name = match entry.name() {
            Some(n) => n,
            None => return git2::TreeWalkResult::Ok,
        };
        if entry.kind() == Some(git2::ObjectType::Blob) {
            let full = if dir.is_empty() {
                name.to_string()
            } else {
                format!("{dir}{name}")
            };
            // Blob size lookup — cheap, already in the pack/loose object header.
            let size = repo
                .find_blob(entry.id())
                .ok()
                .map(|b| b.size() as u64)
                .unwrap_or(0);
            out.push(FileEntry { path: full, size });
        }
        git2::TreeWalkResult::Ok
    })?;
    Ok(out)
}

fn enumerate_fs(root: &Path) -> Result<Vec<FileEntry>> {
    let mut out = Vec::new();
    walk_fs(root, root, &mut out)?;
    Ok(out)
}

fn walk_fs(root: &Path, dir: &Path, out: &mut Vec<FileEntry>) -> Result<()> {
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(f) => f,
            Err(_) => continue,
        };
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if ft.is_dir() {
            if SKIP_DIRS.iter().any(|s| *s == name_str) {
                continue;
            }
            walk_fs(root, &path, out)?;
        } else if ft.is_file() {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| name_str.to_string());
            out.push(FileEntry { path: rel, size });
        }
    }
    Ok(())
}

/// Read bodies for the handful of manifests we parse.
fn read_manifest_bodies(
    root: &Path,
    files: &[FileEntry],
) -> Result<HashMap<String, String>> {
    let wanted: BTreeSet<&str> = MANIFEST_FILENAMES.iter().copied().collect();
    let mut out = HashMap::new();

    // Git-backed path (bare or working-tree): read from HEAD so we
    // don't depend on a checkout.
    let git_bodies = read_manifest_bodies_git(root, &wanted).ok();

    for entry in files {
        if !is_interesting_manifest(&entry.path, &wanted) {
            continue;
        }
        if let Some(body) = git_bodies
            .as_ref()
            .and_then(|m: &HashMap<String, String>| m.get(&entry.path).cloned())
        {
            out.insert(entry.path.clone(), body);
            continue;
        }
        let fs_path: PathBuf = root.join(&entry.path);
        if let Ok(body) = std::fs::read_to_string(&fs_path) {
            out.insert(entry.path.clone(), body);
        }
    }
    Ok(out)
}

fn read_manifest_bodies_git(root: &Path, wanted: &BTreeSet<&str>) -> Result<HashMap<String, String>> {
    let repo = git2::Repository::open(root).or_else(|_| git2::Repository::open_bare(root))?;
    let tree = repo.head()?.peel_to_tree()?;
    let mut out = HashMap::new();
    tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
        if entry.kind() != Some(git2::ObjectType::Blob) {
            return git2::TreeWalkResult::Ok;
        }
        let name = match entry.name() {
            Some(n) => n,
            None => return git2::TreeWalkResult::Ok,
        };
        let full = if dir.is_empty() { name.to_string() } else { format!("{dir}{name}") };
        if !is_interesting_manifest(&full, wanted) {
            return git2::TreeWalkResult::Ok;
        }
        if let Ok(blob) = repo.find_blob(entry.id())
            && let Ok(body) = std::str::from_utf8(blob.content())
        {
            out.insert(full, body.to_string());
        }
        git2::TreeWalkResult::Ok
    })?;
    Ok(out)
}

fn is_interesting_manifest(path: &str, wanted: &BTreeSet<&str>) -> bool {
    // Exact match on the well-known manifest filenames, plus
    // `.config/nextest.toml`.
    if wanted.contains(path) {
        return true;
    }
    if NEXTEST_CONFIG_PATHS.contains(&path) {
        return true;
    }
    // Root-level manifests only: skip nested `node_modules/.../package.json`
    // etc. Walk-enumeration already skips node_modules, but the git
    // path doesn't, so we gate explicitly on file basename + position.
    let basename = path.rsplit('/').next().unwrap_or(path);
    if wanted.contains(basename) {
        // Only treat as manifest if it sits at the repository root
        // (no slashes in the path).
        let depth = path.matches('/').count();
        if depth == 0 {
            return true;
        }
    }
    false
}

fn build_stack(files: &[FileEntry], bodies: &HashMap<String, String>) -> Stack {
    let (languages, primary) = tally_languages(files);
    let signals = manifest_signals(files, bodies);

    // Package-manager detection.
    let mut pms: Vec<String> = Vec::new();
    let mut monorepo: Vec<String> = Vec::new();
    let mut test_runners: Vec<&'static str> = Vec::new();
    let mut framework_slugs: Vec<&'static str> = Vec::new();
    let mut runtimes = Runtimes::default();

    // package.json
    if let Some(body) = bodies.get("package.json") {
        let info = manifests::parse_package_json(body);
        if let Some(pm) = info.package_manager.clone() {
            pms.push(pm);
        } else {
            // Lockfile-based fallback when `packageManager` field is absent.
            let has_pnpm_lock = files.iter().any(|f| f.path == "pnpm-lock.yaml");
            let has_yarn_lock = files.iter().any(|f| f.path == "yarn.lock");
            let has_bun_lock = files.iter().any(|f| f.path == "bun.lockb" || f.path == "bun.lock");
            let has_npm_lock = files.iter().any(|f| f.path == "package-lock.json");
            if has_pnpm_lock {
                pms.push("pnpm".into());
            } else if has_yarn_lock {
                pms.push("yarn".into());
            } else if has_bun_lock {
                pms.push("bun".into());
            } else {
                // `has_npm_lock` or no lockfile — either way npm is
                // the reasonable default.
                let _ = has_npm_lock;
                pms.push("npm".into());
            }
        }
        if info.has_workspaces {
            monorepo.push("npm-workspaces".into());
        }
        if let Some(node) = info.node_engine {
            runtimes.node = Some(node);
        }
        for dep in &info.dep_names {
            if let Some(fw) = framework_for_dep(dep) {
                framework_slugs.push(fw);
            }
            if let Some(runner) = runner_for_dep(dep) {
                test_runners.push(runner);
            }
        }
    }

    // Cargo.toml
    if signals.has_cargo_toml {
        let cargo_body = bodies.get("Cargo.toml").map(String::as_str).unwrap_or("");
        let toolchain_body = bodies.get("rust-toolchain.toml").map(String::as_str);
        let info = manifests::parse_cargo_toml(cargo_body, toolchain_body);
        if !pms.contains(&"cargo".to_string()) {
            pms.push("cargo".into());
        }
        if info.is_workspace {
            monorepo.push("cargo-workspace".into());
        }
        if let Some(rust) = info.rust_version {
            runtimes.rust = Some(rust);
        }
        // nextest config presence
        if files
            .iter()
            .any(|f| NEXTEST_CONFIG_PATHS.iter().any(|p| *p == f.path))
        {
            test_runners.push("nextest");
        }
    }

    // pyproject.toml
    if signals.has_pyproject_toml {
        let body = bodies.get("pyproject.toml").map(String::as_str).unwrap_or("");
        let info = manifests::parse_pyproject(body);
        if let Some(pm) = info.package_manager {
            if !pms.contains(&pm) {
                pms.push(pm);
            }
        } else if !pms.iter().any(|p| matches!(p.as_str(), "uv" | "poetry" | "pdm" | "pip")) {
            pms.push("pip".into());
        }
        if let Some(py) = info.python_version {
            runtimes.python = Some(py);
        }
        for dep in &info.dep_names {
            if let Some(fw) = framework_for_dep(dep) {
                framework_slugs.push(fw);
            }
            if let Some(runner) = runner_for_dep(dep) {
                test_runners.push(runner);
            }
        }
        // Pytest config in pyproject is a strong signal even without a runtime dep.
        if body.contains("[tool.pytest") && !test_runners.contains(&"pytest") {
            test_runners.push("pytest");
        }
    }

    // go.mod
    if signals.has_go_mod {
        let body = bodies.get("go.mod").map(String::as_str).unwrap_or("");
        let info = manifests::parse_go_mod(body);
        if !pms.contains(&"go-mod".to_string()) {
            pms.push("go-mod".into());
        }
        if let Some(go) = info.go_version {
            runtimes.go = Some(go);
        }
        test_runners.push("go-test");
        // go.work signals a go workspace (monorepo).
        if files.iter().any(|f| f.path == "go.work") {
            monorepo.push("go-workspace".into());
        }
    }

    // Gemfile
    if files.iter().any(|f| f.path == "Gemfile") {
        let body = bodies.get("Gemfile").map(String::as_str).unwrap_or("");
        let info = manifests::parse_gemfile(body);
        if !pms.contains(&"bundler".to_string()) {
            pms.push("bundler".into());
        }
        for gem in &info.gems {
            if let Some(fw) = framework_for_dep(gem) {
                framework_slugs.push(fw);
            }
            if let Some(runner) = runner_for_dep(gem) {
                test_runners.push(runner);
            }
        }
    }

    // Java (pom.xml / build.gradle*).
    let has_pom = files.iter().any(|f| f.path == "pom.xml");
    let has_gradle = files
        .iter()
        .any(|f| f.path == "build.gradle" || f.path == "build.gradle.kts");
    if has_pom || has_gradle {
        if has_pom && !pms.contains(&"maven".to_string()) {
            pms.push("maven".into());
        }
        if has_gradle && !pms.contains(&"gradle".to_string()) {
            pms.push("gradle".into());
        }
        let combined: String = bodies
            .iter()
            .filter(|(k, _)| {
                matches!(
                    k.as_str(),
                    "pom.xml" | "build.gradle" | "build.gradle.kts"
                )
            })
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let info = manifests::parse_pom(&combined);
        if info.has_spring {
            framework_slugs.push("spring");
        }
        if info.has_junit {
            test_runners.push("junit");
        }
    }

    // Monorepo tooling signals.
    if signals.has_pnpm_workspace {
        monorepo.push("pnpm-workspaces".into());
    }
    if signals.has_turbo_json {
        monorepo.push("turbo".into());
    }
    if files.iter().any(|f| f.path == "nx.json") {
        monorepo.push("nx".into());
    }
    if files.iter().any(|f| f.path == "lerna.json") {
        monorepo.push("lerna".into());
    }

    // Canonicalise the collected slug lists.
    let package_managers = dedup_sorted(pms);
    let monorepo_tools = dedup_sorted(monorepo);
    let frameworks = frameworks::canonicalize(framework_slugs);
    let test_runners = test_runners::canonicalize(test_runners);
    let is_monorepo = !monorepo_tools.is_empty();

    Stack {
        detected_at: Utc::now(),
        languages,
        primary_language: primary,
        package_managers,
        monorepo_tools,
        is_monorepo,
        test_runners,
        frameworks,
        runtimes,
        manifest_signals: signals,
        // `workspaces` is populated by `detect_blocking` via
        // `discover_workspaces_blocking` after `build_stack` returns —
        // kept separate so unit tests of `build_stack` don't need a
        // real filesystem root.
        workspaces: Vec::new(),
    }
}

fn manifest_signals(files: &[FileEntry], _bodies: &HashMap<String, String>) -> ManifestSignals {
    let has = |needle: &str| files.iter().any(|f| f.path == needle);
    ManifestSignals {
        has_package_json: has("package.json"),
        has_cargo_toml: has("Cargo.toml"),
        has_pyproject_toml: has("pyproject.toml"),
        has_go_mod: has("go.mod"),
        has_pnpm_workspace: has("pnpm-workspace.yaml"),
        has_turbo_json: has("turbo.json"),
    }
}

fn tally_languages(files: &[FileEntry]) -> (Vec<LanguageStat>, Option<String>) {
    use crate::languages::LanguageKind;

    let table = LanguageTable::global();
    let mut totals: BTreeMap<String, u64> = BTreeMap::new();
    for entry in files {
        let Some(lang) = table.classify(&entry.path) else { continue };
        // Only `programming` + `markup` count toward the language
        // byte-share — data files (JSON/YAML/TOML) and prose drown out
        // the signal otherwise.
        if !matches!(lang.kind, LanguageKind::Programming | LanguageKind::Markup) {
            continue;
        }
        *totals.entry(lang.name.clone()).or_insert(0) += entry.size.max(1);
    }
    let total: u64 = totals.values().sum();
    if total == 0 {
        return (Vec::new(), None);
    }
    let mut stats: Vec<LanguageStat> = totals
        .into_iter()
        .map(|(name, bytes)| LanguageStat {
            name,
            bytes,
            pct: round2((bytes as f64) * 100.0 / (total as f64)),
        })
        .collect();
    stats.sort_by(|a, b| {
        b.bytes
            .cmp(&a.bytes)
            .then_with(|| a.name.cmp(&b.name))
    });
    let primary = stats.first().map(|s| s.name.clone());
    (stats, primary)
}

fn round2(f: f64) -> f64 {
    (f * 100.0).round() / 100.0
}

fn dedup_sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

// ---- workspace discovery ------------------------------------------------

/// Skip any manifest whose path is under one of these directory segments.
/// The git enumeration doesn't automatically exclude `node_modules/` if
/// somebody accidentally committed it, so we belt-and-braces here.
const WORKSPACE_SKIP_SEGMENTS: &[&str] = &[
    "node_modules",
    "vendor",
    "target",
    "dist",
    "build",
    ".venv",
    "venv",
    ".git",
];

struct WorkspaceCandidate {
    path: String,
    language: &'static str,
}

/// Discover one `StackWorkspace` per distinct toolchain-scope in the
/// mirror. Called by `detect_blocking` after `build_stack`.
fn discover_workspaces_blocking(root: &Path, files: &[FileEntry]) -> Vec<StackWorkspace> {
    let candidates: Vec<WorkspaceCandidate> = files
        .iter()
        .filter_map(|entry| {
            workspace_manifest_language(&entry.path)
                .map(|lang| WorkspaceCandidate {
                    path: entry.path.clone(),
                    language: lang,
                })
        })
        .collect();

    let selected = dedup_shallowest_per_language(candidates);
    if selected.is_empty() {
        return Vec::new();
    }

    // Read the bodies we'll parse — selected manifests plus sibling
    // `rust-toolchain.toml` entries. Only nested paths land here; the
    // root-level manifest bodies are already in the main `bodies` map,
    // but since we want one consistent lookup we re-read from disk /
    // git here.
    let wanted_bodies = body_paths_for_workspaces(&selected, files);
    let bodies = read_paths(root, &wanted_bodies);

    selected
        .into_iter()
        .map(|c| build_workspace(c, &bodies))
        .collect()
}

fn workspace_manifest_language(path: &str) -> Option<&'static str> {
    for seg in WORKSPACE_SKIP_SEGMENTS {
        if path == *seg
            || path.starts_with(&format!("{seg}/"))
            || path.contains(&format!("/{seg}/"))
        {
            return None;
        }
    }
    let basename = path.rsplit('/').next().unwrap_or(path);
    match basename {
        "Cargo.toml" => Some("rust"),
        "package.json" => Some("node"),
        "pyproject.toml" => Some("python"),
        "go.mod" => Some("go"),
        "Gemfile" => Some("ruby"),
        "pom.xml" | "build.gradle" | "build.gradle.kts" => Some("java"),
        _ => None,
    }
}

fn dedup_shallowest_per_language(
    mut candidates: Vec<WorkspaceCandidate>,
) -> Vec<WorkspaceCandidate> {
    // Shallower paths first so ancestors are visited before descendants.
    // Tie-break alphabetically to make detection deterministic.
    candidates.sort_by(|a, b| {
        let depth_a = a.path.matches('/').count();
        let depth_b = b.path.matches('/').count();
        depth_a.cmp(&depth_b).then_with(|| a.path.cmp(&b.path))
    });

    let mut selected: Vec<WorkspaceCandidate> = Vec::new();
    for c in candidates {
        let dir = parent_dir(&c.path);
        let suppressed = selected.iter().any(|s| {
            s.language == c.language && is_ancestor_dir(&parent_dir(&s.path), dir)
        });
        if !suppressed {
            selected.push(c);
        }
    }
    selected
}

fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(idx) => &path[..idx],
        None => "",
    }
}

/// `ancestor` is `candidate` itself or one of its prefix directories.
/// Both are "" for the repo root.
fn is_ancestor_dir(ancestor: &str, candidate: &str) -> bool {
    if ancestor.is_empty() {
        return true;
    }
    if ancestor == candidate {
        return true;
    }
    candidate.starts_with(&format!("{ancestor}/"))
}

fn body_paths_for_workspaces(
    selected: &[WorkspaceCandidate],
    files: &[FileEntry],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in selected {
        out.push(c.path.clone());
        // Rust — sibling `rust-toolchain.toml` is the authoritative pin.
        if c.language == "rust" {
            let sibling = join_dir(parent_dir(&c.path), "rust-toolchain.toml");
            if files.iter().any(|f| f.path == sibling) {
                out.push(sibling);
            }
        }
        // Node — sibling lockfiles help the package-manager fallback
        // when `packageManager` isn't pinned in package.json.
        if c.language == "node" {
            for lf in ["pnpm-lock.yaml", "yarn.lock", "bun.lockb", "bun.lock", "package-lock.json"] {
                let sibling = join_dir(parent_dir(&c.path), lf);
                if files.iter().any(|f| f.path == sibling) {
                    out.push(sibling);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn join_dir(dir: &str, basename: &str) -> String {
    if dir.is_empty() {
        basename.to_string()
    } else {
        format!("{dir}/{basename}")
    }
}

/// Read a batch of mirror-relative paths. Prefers `git` (reads from
/// HEAD, so we don't depend on a working-tree checkout) and falls back
/// to filesystem per-path. Missing files are silently omitted.
fn read_paths(root: &Path, paths: &[String]) -> HashMap<String, String> {
    if paths.is_empty() {
        return HashMap::new();
    }

    let wanted: BTreeSet<&str> = paths.iter().map(String::as_str).collect();
    let mut out = HashMap::new();

    if let Ok(repo) = git2::Repository::open(root).or_else(|_| git2::Repository::open_bare(root))
        && let Ok(head) = repo.head()
        && let Ok(tree) = head.peel_to_tree()
    {
        let _ = tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() != Some(git2::ObjectType::Blob) {
                return git2::TreeWalkResult::Ok;
            }
            let Some(name) = entry.name() else {
                return git2::TreeWalkResult::Ok;
            };
            let full = if dir.is_empty() {
                name.to_string()
            } else {
                format!("{dir}{name}")
            };
            if !wanted.contains(full.as_str()) {
                return git2::TreeWalkResult::Ok;
            }
            if let Ok(blob) = repo.find_blob(entry.id())
                && let Ok(body) = std::str::from_utf8(blob.content())
            {
                out.insert(full, body.to_string());
            }
            git2::TreeWalkResult::Ok
        });
    }

    for path in paths {
        if out.contains_key(path) {
            continue;
        }
        let fs_path = root.join(path);
        if let Ok(body) = std::fs::read_to_string(&fs_path) {
            out.insert(path.clone(), body);
        }
    }
    out
}

fn build_workspace(
    candidate: WorkspaceCandidate,
    bodies: &HashMap<String, String>,
) -> StackWorkspace {
    use crate::schema::StackWorkspace as Ws;

    let dir = parent_dir(&candidate.path).to_string();
    let body = bodies.get(&candidate.path).map(String::as_str).unwrap_or("");

    let (toolchain, package_manager) = match candidate.language {
        "rust" => {
            let sibling = join_dir(&dir, "rust-toolchain.toml");
            let toolchain_body = bodies.get(&sibling).map(String::as_str);
            let info = manifests::parse_cargo_toml(body, toolchain_body);
            (info.rust_version, None)
        }
        "node" => {
            let info = manifests::parse_package_json(body);
            let pm = info.package_manager.or_else(|| {
                // Lockfile fallback, mirroring build_stack's root-level logic.
                for (lf, slug) in [
                    ("pnpm-lock.yaml", "pnpm"),
                    ("yarn.lock", "yarn"),
                    ("bun.lockb", "bun"),
                    ("bun.lock", "bun"),
                    ("package-lock.json", "npm"),
                ] {
                    if bodies.contains_key(&join_dir(&dir, lf)) {
                        return Some(slug.to_string());
                    }
                }
                None
            });
            (info.node_engine, pm)
        }
        "python" => {
            let info = manifests::parse_pyproject(body);
            (info.python_version, info.package_manager)
        }
        "go" => {
            let info = manifests::parse_go_mod(body);
            (info.go_version, None)
        }
        _ => (None, None),
    };

    Ws {
        root: dir,
        language: candidate.language.to_string(),
        toolchain,
        package_manager,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(paths: &[(&str, u64)]) -> Vec<FileEntry> {
        paths
            .iter()
            .map(|(p, s)| FileEntry {
                path: (*p).to_string(),
                size: *s,
            })
            .collect()
    }

    #[test]
    fn tallies_and_sorts_language_bytes() {
        let files = entries(&[
            ("src/lib.rs", 1000),
            ("src/main.rs", 500),
            ("scripts/helper.py", 200),
        ]);
        let (langs, primary) = tally_languages(&files);
        assert_eq!(primary.as_deref(), Some("Rust"));
        assert_eq!(langs.len(), 2);
        assert_eq!(langs[0].name, "Rust");
        assert_eq!(langs[0].bytes, 1500);
    }

    #[test]
    fn data_files_are_excluded_from_language_byte_share() {
        let files = entries(&[
            ("src/lib.rs", 100),
            ("package-lock.json", 999_999),
        ]);
        let (langs, _) = tally_languages(&files);
        assert_eq!(langs.len(), 1);
        assert_eq!(langs[0].name, "Rust");
    }

    #[test]
    fn workspace_manifest_language_classifies_basenames() {
        assert_eq!(workspace_manifest_language("Cargo.toml"), Some("rust"));
        assert_eq!(
            workspace_manifest_language("server/Cargo.toml"),
            Some("rust")
        );
        assert_eq!(
            workspace_manifest_language("ui/package.json"),
            Some("node")
        );
        assert_eq!(workspace_manifest_language("go.mod"), Some("go"));
        assert_eq!(
            workspace_manifest_language("services/api/pyproject.toml"),
            Some("python")
        );
        assert_eq!(workspace_manifest_language("pom.xml"), Some("java"));
        assert_eq!(workspace_manifest_language("build.gradle.kts"), Some("java"));
        assert_eq!(workspace_manifest_language("Gemfile"), Some("ruby"));
        assert_eq!(workspace_manifest_language("README.md"), None);
        // Skip well-known vendored trees even when they contain manifests.
        assert_eq!(
            workspace_manifest_language("node_modules/react/package.json"),
            None
        );
        assert_eq!(
            workspace_manifest_language("vendor/github.com/pkg/errors/go.mod"),
            None
        );
        assert_eq!(
            workspace_manifest_language("ui/node_modules/x/package.json"),
            None
        );
    }

    fn candidate(path: &str, language: &'static str) -> WorkspaceCandidate {
        WorkspaceCandidate {
            path: path.to_string(),
            language,
        }
    }

    #[test]
    fn dedup_suppresses_nested_same_language_manifests() {
        // `server/Cargo.toml` should win — `server/crates/djinn-db/Cargo.toml`
        // is a member crate, not a distinct workspace.
        let input = vec![
            candidate("server/crates/djinn-db/Cargo.toml", "rust"),
            candidate("server/Cargo.toml", "rust"),
            candidate("server/crates/djinn-graph/Cargo.toml", "rust"),
        ];
        let out = dedup_shallowest_per_language(input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "server/Cargo.toml");
    }

    #[test]
    fn dedup_keeps_sibling_unrelated_workspaces() {
        // Two Rust workspaces that aren't nested — the motivating case
        // for the env-config refactor (different toolchains per dir).
        let input = vec![
            candidate("server/Cargo.toml", "rust"),
            candidate("tools/codegen/Cargo.toml", "rust"),
        ];
        let out = dedup_shallowest_per_language(input);
        let roots: Vec<&str> = out.iter().map(|c| c.path.as_str()).collect();
        assert!(roots.contains(&"server/Cargo.toml"));
        assert!(roots.contains(&"tools/codegen/Cargo.toml"));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn dedup_does_not_cross_languages() {
        // Rust workspace at `server/` should not suppress a Node
        // workspace at `server/ui/` or vice versa.
        let input = vec![
            candidate("server/Cargo.toml", "rust"),
            candidate("server/ui/package.json", "node"),
        ];
        let out = dedup_shallowest_per_language(input);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn is_ancestor_dir_handles_root_and_empty() {
        assert!(is_ancestor_dir("", "anything"));
        assert!(is_ancestor_dir("", ""));
        assert!(is_ancestor_dir("server", "server"));
        assert!(is_ancestor_dir("server", "server/crates/foo"));
        assert!(!is_ancestor_dir("server", "services"));
        assert!(!is_ancestor_dir("server", "servero"));
    }

    fn file_entries(paths: &[&str]) -> Vec<FileEntry> {
        paths
            .iter()
            .map(|p| FileEntry {
                path: (*p).to_string(),
                size: 100,
            })
            .collect()
    }

    #[test]
    fn build_workspace_reads_rust_toolchain_from_sibling() {
        let mut bodies = HashMap::new();
        bodies.insert("server/Cargo.toml".into(), "[workspace]\n".into());
        bodies.insert(
            "server/rust-toolchain.toml".into(),
            "[toolchain]\nchannel = \"1.85.0\"\n".into(),
        );
        let ws = build_workspace(candidate("server/Cargo.toml", "rust"), &bodies);
        assert_eq!(ws.root, "server");
        assert_eq!(ws.language, "rust");
        assert_eq!(ws.toolchain.as_deref(), Some("1.85.0"));
        assert!(ws.package_manager.is_none());
    }

    #[test]
    fn build_workspace_parses_node_engine_and_pm() {
        let mut bodies = HashMap::new();
        bodies.insert(
            "ui/package.json".into(),
            r#"{"packageManager":"pnpm@9","engines":{"node":">=22"}}"#.into(),
        );
        let ws = build_workspace(candidate("ui/package.json", "node"), &bodies);
        assert_eq!(ws.root, "ui");
        assert_eq!(ws.toolchain.as_deref(), Some("22"));
        assert_eq!(ws.package_manager.as_deref(), Some("pnpm"));
    }

    #[test]
    fn build_workspace_node_falls_back_to_lockfile() {
        let mut bodies = HashMap::new();
        bodies.insert("package.json".into(), r#"{"name":"x"}"#.into());
        bodies.insert("pnpm-lock.yaml".into(), "lockfileVersion: 9\n".into());
        let ws = build_workspace(candidate("package.json", "node"), &bodies);
        assert_eq!(ws.root, "");
        assert_eq!(ws.package_manager.as_deref(), Some("pnpm"));
    }

    #[test]
    fn body_paths_includes_rust_toolchain_sibling_only_when_present() {
        let files = file_entries(&[
            "server/Cargo.toml",
            "server/rust-toolchain.toml",
            "tools/codegen/Cargo.toml",
        ]);
        let selected = vec![
            candidate("server/Cargo.toml", "rust"),
            candidate("tools/codegen/Cargo.toml", "rust"),
        ];
        let paths = body_paths_for_workspaces(&selected, &files);
        assert!(paths.contains(&"server/Cargo.toml".to_string()));
        assert!(paths.contains(&"server/rust-toolchain.toml".to_string()));
        assert!(paths.contains(&"tools/codegen/Cargo.toml".to_string()));
        // `tools/codegen/rust-toolchain.toml` doesn't exist, so it must
        // not be requested.
        assert!(!paths.contains(&"tools/codegen/rust-toolchain.toml".to_string()));
    }
}
