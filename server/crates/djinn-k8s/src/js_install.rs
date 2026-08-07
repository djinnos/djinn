//! Shared JavaScript dependency-install preamble for the warm and SCIP Pods.
//!
//! Both Pods clone the project and then need its JS dependencies on disk before
//! the real work starts: scip-typescript resolves `tsconfig` `extends` through
//! `node_modules`, and the install is also what deposits packages into the
//! shared content-addressed package-manager store on `/cache`.
//!
//! This module exists because that preamble used to be a string literal
//! duplicated in `warm_job.rs` and `scip_job.rs`, and it made two assumptions
//! that do not hold for a monorepo whose JS does not live at the repo root:
//!
//! 1. **Root-only.** It ran `cd "$project_root"` and looked for a lockfile
//!    there. A repo whose JS workspace is `ui/` was never installed at all.
//! 2. **First-match-wins across package managers.** The chain tested
//!    `pnpm-lock.yaml`, then `yarn.lock`, then `package-lock.json` — but at a
//!    single directory. A root holding a `package-lock.json` for unrelated
//!    reasons (e.g. vendoring a CLI) silently shadowed the pnpm workspace one
//!    directory below, so the npm branch ran and the pnpm store stayed cold.
//!
//! Both assumptions were live in djinn's own repo: the root has a
//! `package-lock.json` whose entire contents are two utility dependencies,
//! while the real pnpm workspace is `ui/`. The result was that `ui/` was never
//! installed by any warm cycle, so every task-run Pod that needed it paid a
//! full cold install — and, when the shared store held a broken package-manager
//! entry, hung until its session deadline with no diagnostic.
//!
//! The renderer here fixes the discovery (install every *declared* JS workspace
//! in its own root) and adds two bounds that keep a broken toolchain from
//! silently consuming a whole Job deadline. See [`js_install_preamble`].

use djinn_stack::environment::EnvironmentConfig;

/// Wall-clock bound for a single workspace's dependency install.
///
/// A cold install of a large workspace is minutes, not seconds, so this is
/// generous. Its purpose is not to make installs fast — it is to convert an
/// *unbounded* failure into a bounded, reported one.
///
/// This bound is the direct lesson of a three-week outage: a corrupted entry in
/// the shared package-manager store left `pnpm` exec'ing its own path in an
/// infinite loop. With no bound, every affected Pod spun at ~85% CPU until its
/// deadline killed it — producing no output, no artifacts, and no error, which
/// is the worst possible failure signature to debug. With a bound the same
/// corruption costs one workspace this many seconds and prints a loud message.
pub const JS_INSTALL_TIMEOUT_SECONDS: u32 = 900;

/// Wall-clock bound for the package-manager preflight (`--version`).
///
/// A working package manager answers this in well under a second. Anything
/// slower is a broken toolchain, and we want to know that *before* committing
/// [`JS_INSTALL_TIMEOUT_SECONDS`] to an install that cannot succeed.
pub const JS_PREFLIGHT_TIMEOUT_SECONDS: u32 = 60;

/// Workspace `language` values that denote a JavaScript/TypeScript toolchain.
///
/// Matched case-insensitively. `node` is what the environment-config UI emits;
/// the rest are accepted so a hand-edited config still resolves.
const JS_LANGUAGES: &[&str] = &["node", "javascript", "typescript", "js", "ts"];

/// Return the declared JS workspace roots, relative to the project root.
///
/// Order is preserved from the config so the rendered script is deterministic
/// (the Job spec is compared across reconciles; a reordered script would look
/// like a spec change). Roots are de-duplicated and normalised: a leading
/// `./` is stripped and a root of `.` means the project root itself.
///
/// Returns empty when no config is present or no workspace declares a JS
/// language — callers then fall back to root-only detection, which preserves
/// the previous behaviour for projects that have not been reseeded.
pub fn js_workspace_roots(config: Option<&EnvironmentConfig>) -> Vec<String> {
    let Some(config) = config else {
        return Vec::new();
    };
    let mut roots: Vec<String> = Vec::new();
    for workspace in &config.workspaces {
        if !JS_LANGUAGES
            .iter()
            .any(|lang| workspace.language.eq_ignore_ascii_case(lang))
        {
            continue;
        }
        let root = normalise_root(&workspace.root);
        // Reject anything that could escape the project root once interpolated
        // into the Pod script. The config is operator-supplied, but this script
        // runs as a shell command, so the renderer refuses rather than trusts.
        if !is_safe_relative_root(&root) {
            continue;
        }
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

/// Strip a leading `./` and any trailing `/` so `ui`, `./ui` and `ui/` agree.
fn normalise_root(root: &str) -> String {
    let trimmed = root.trim();
    let trimmed = trimmed.strip_prefix("./").unwrap_or(trimmed);
    let trimmed = trimmed.trim_end_matches('/');
    if trimmed.is_empty() {
        ".".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Reject absolute paths, parent-directory escapes, and shell metacharacters.
fn is_safe_relative_root(root: &str) -> bool {
    if root == "." {
        return true;
    }
    if root.starts_with('/') || root.starts_with('~') {
        return false;
    }
    if root.split('/').any(|segment| segment == "..") {
        return false;
    }
    // The root is interpolated into a double-quoted shell word. Refuse anything
    // that could break out of it or expand.
    !root.chars().any(|c| {
        matches!(
            c,
            '"' | '\'' | '$' | '`' | '\\' | '\n' | '\r' | ';' | '&' | '|'
        )
    })
}

/// Render the JS dependency-install preamble for a warm or SCIP Pod script.
///
/// `roots` is normally [`js_workspace_roots`]; when it is empty the renderer
/// falls back to the project root alone, which is exactly the previous
/// behaviour for projects with no declared JS workspace.
///
/// The rendered script, per workspace root:
///
/// * skips the root when the directory does not exist (a config can name a
///   workspace that a given revision does not have yet);
/// * picks the package manager from the lockfile **in that root**, so a
///   sibling root's lockfile can no longer shadow it;
/// * preflights the package manager with `--version` under
///   [`JS_PREFLIGHT_TIMEOUT_SECONDS`], and skips the install with a loud
///   message if the toolchain is broken;
/// * bounds the install itself with [`JS_INSTALL_TIMEOUT_SECONDS`] and reports
///   a distinct message on timeout.
///
/// Every failure is non-fatal (`|| true` semantics via explicit status checks):
/// a JS install failure must not abort a warm whose Rust/Python/Go indexers
/// would still succeed. That was true of the original preamble and is preserved
/// here — but a failure is now *reported* instead of silent.
pub fn js_install_preamble(project_root: &str, roots: &[String]) -> String {
    let effective: Vec<String> = if roots.is_empty() {
        vec![".".to_string()]
    } else {
        roots.to_vec()
    };

    let mut script = String::new();
    script.push_str(&format!(
        r#"
# ---------------------------------------------------------------------------
# JS dependency install (see djinn_k8s::js_install).
#
# Installs each DECLARED JS workspace in its own root. This used to `cd` to the
# project root and test lockfiles there, which silently skipped every monorepo
# whose JS lives in a subdirectory, and let an unrelated root lockfile select
# the wrong package manager.
#
# Bounded on purpose: a corrupt shared package-manager store can leave the
# package manager spinning forever, and an unbounded install turns that into a
# silent Pod-deadline kill with no diagnostic.
# ---------------------------------------------------------------------------
djinn_js_install_one() {{
  _root="$1"
  _dir="{project_root}"
  if [ "$_root" != "." ]; then
    _dir="{project_root}/$_root"
  fi
  if [ ! -d "$_dir" ]; then
    echo "djinn-js-install: skip '$_root' (no such directory at this revision)"
    return 0
  fi
  cd "$_dir" || return 0

  _pm=""
  _install=""
  if [ -f pnpm-lock.yaml ]; then
    _pm="pnpm"; _install="install --frozen-lockfile"
  elif [ -f yarn.lock ]; then
    _pm="yarn"; _install="install --frozen-lockfile"
  elif [ -f package-lock.json ]; then
    _pm="npm"; _install="ci"
  else
    echo "djinn-js-install: skip '$_root' (no lockfile)"
    return 0
  fi

  # corepack materialises the `packageManager`-pinned version. Best effort: a
  # repo without a pin still runs the image's package manager.
  corepack enable >/dev/null 2>&1 || true

  # Preflight. A working package manager answers --version instantly; a broken
  # one (e.g. a corrupt store entry that exec's itself) never returns. Catch
  # that here, cheaply, instead of burning the install budget on it.
  if ! timeout {preflight}s "$_pm" --version >/dev/null 2>&1; then
    echo "djinn-js-install: FAILED '$_root' — '$_pm --version' did not answer in {preflight}s;" \
         "the package manager or its shared store is broken. Skipping install."
    return 0
  fi

  echo "djinn-js-install: installing '$_root' with $_pm ($(timeout {preflight}s "$_pm" --version 2>/dev/null))"
  if timeout {install}s "$_pm" $_install; then
    echo "djinn-js-install: ok '$_root'"
  else
    _status=$?
    if [ "$_status" -eq 124 ]; then
      echo "djinn-js-install: TIMEOUT '$_root' after {install}s with $_pm"
    else
      echo "djinn-js-install: failed '$_root' (exit $_status) with $_pm — continuing"
    fi
  fi
  return 0
}}
"#,
        project_root = project_root,
        preflight = JS_PREFLIGHT_TIMEOUT_SECONDS,
        install = JS_INSTALL_TIMEOUT_SECONDS,
    ));

    for root in &effective {
        script.push_str(&format!("djinn_js_install_one \"{root}\"\n"));
    }
    // The helper `cd`s per workspace; restore the project root for whatever the
    // caller does next (both callers `exec` a binary that expects to be there).
    script.push_str(&format!("cd \"{project_root}\"\n"));
    script
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_stack::environment::{EnvironmentConfig, Workspace};

    fn workspace(root: &str, language: &str) -> Workspace {
        Workspace {
            slug: None,
            name: None,
            tags: Vec::new(),
            root: root.to_string(),
            language: language.to_string(),
            toolchain: None,
            version: None,
            package_manager: None,
            cargo_features: Vec::new(),
            cargo_all_features: false,
        }
    }

    fn config_with(workspaces: Vec<Workspace>) -> EnvironmentConfig {
        EnvironmentConfig {
            workspaces,
            ..Default::default()
        }
    }

    /// The regression this module exists for: djinn's own shape. A Rust
    /// workspace at `server`, JS at `ui` and `website`, and a root
    /// `package-lock.json` that used to capture the whole install.
    #[test]
    fn discovers_js_workspaces_below_the_repo_root() {
        let config = config_with(vec![
            workspace("server", "rust"),
            workspace("website", "node"),
            workspace("ui", "node"),
        ]);
        assert_eq!(
            js_workspace_roots(Some(&config)),
            vec!["website".to_string(), "ui".to_string()],
            "both node workspaces must be discovered, and the rust one skipped"
        );
    }

    #[test]
    fn no_config_falls_back_to_root_only() {
        assert!(js_workspace_roots(None).is_empty());
        let script = js_install_preamble("/workspace/p", &[]);
        assert!(
            script.contains("djinn_js_install_one \".\""),
            "empty roots must preserve the previous root-only behaviour:\n{script}"
        );
    }

    #[test]
    fn accepts_alternate_js_language_spellings_case_insensitively() {
        let config = config_with(vec![
            workspace("a", "Node"),
            workspace("b", "TypeScript"),
            workspace("c", "js"),
            workspace("d", "python"),
        ]);
        assert_eq!(
            js_workspace_roots(Some(&config)),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn normalises_and_dedupes_roots() {
        let config = config_with(vec![
            workspace("./ui", "node"),
            workspace("ui/", "node"),
            workspace("ui", "node"),
        ]);
        assert_eq!(js_workspace_roots(Some(&config)), vec!["ui".to_string()]);
    }

    #[test]
    fn rejects_roots_that_could_escape_or_inject() {
        let config = config_with(vec![
            workspace("/etc", "node"),
            workspace("../../etc", "node"),
            workspace("ui\"; rm -rf /", "node"),
            workspace("ui$(whoami)", "node"),
            workspace("~/evil", "node"),
            workspace("safe", "node"),
        ]);
        assert_eq!(
            js_workspace_roots(Some(&config)),
            vec!["safe".to_string()],
            "only the safe relative root survives"
        );
    }

    #[test]
    fn each_root_detects_its_own_lockfile_rather_than_the_repo_roots() {
        let script = js_install_preamble("/workspace/p", &["ui".to_string()]);
        // The lockfile tests must run AFTER cd-ing into the workspace dir, so
        // that a root-level lockfile cannot select the package manager.
        let cd_at = script.find("cd \"$_dir\"").expect("cds into workspace dir");
        let pnpm_at = script
            .find("-f pnpm-lock.yaml")
            .expect("tests pnpm lockfile");
        assert!(
            cd_at < pnpm_at,
            "lockfile detection must happen inside the workspace root, not before it"
        );
    }

    #[test]
    fn every_install_is_bounded_and_preflighted() {
        let script = js_install_preamble("/workspace/p", &["ui".to_string()]);
        assert!(
            script.contains(&format!("timeout {JS_INSTALL_TIMEOUT_SECONDS}s")),
            "the install itself must be bounded"
        );
        assert!(
            script.contains(&format!("timeout {JS_PREFLIGHT_TIMEOUT_SECONDS}s")),
            "the package manager must be preflighted"
        );
        assert!(
            script.contains("did not answer"),
            "a broken package manager must be reported, not silently skipped"
        );
    }

    #[test]
    fn renders_one_call_per_declared_root_and_returns_to_the_project_root() {
        let script =
            js_install_preamble("/workspace/p", &["ui".to_string(), "website".to_string()]);
        assert!(script.contains("djinn_js_install_one \"ui\""));
        assert!(script.contains("djinn_js_install_one \"website\""));
        assert!(
            script.trim_end().ends_with("cd \"/workspace/p\""),
            "must leave the shell in the project root for the exec that follows"
        );
    }

    #[test]
    fn npm_uses_ci_and_pnpm_yarn_use_frozen_lockfile() {
        let script = js_install_preamble("/workspace/p", &["ui".to_string()]);
        assert!(script.contains(r#"_pm="npm"; _install="ci""#));
        assert!(script.contains(r#"_pm="pnpm"; _install="install --frozen-lockfile""#));
        assert!(script.contains(r#"_pm="yarn"; _install="install --frozen-lockfile""#));
    }
}
