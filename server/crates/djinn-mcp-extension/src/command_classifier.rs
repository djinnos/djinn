//! Production-oriented destructive shell command classifier.
//!
//! This module is the **production** counterpart of the fail-closed
//! [`crate::command_validator::validate_read_only_command`].  Where the
//! read-only validator is reject-unknown (anything not on an explicit
//! allowlist is rejected), the production classifier is
//! **allow-by-default**: ordinary build, read, and test commands pass
//! through without restriction.  Only commands that match explicit
//! destructive patterns produce a `HardDeny` or `SoftGate` outcome.

use std::fmt;

use crate::command_validator::{SQL_MUTATION_KEYWORDS, split_pipeline, tokenize};

/// Stable class identifiers for soft-gate categories produced by the
/// production classifier.
///
/// These map 1:1 to `DestructiveClass` constants in
/// `djinn-agent::file_time::destructive_class`:
///
/// | `ShellDestructiveClass`     | `destructive_class::` constant / label               |
/// |-----------------------------|------------------------------------------------------|
/// | `WorktreeLocalFileMutation` | `WORKTREE_LOCAL_FILE_MUTATION` (`"destructive.worktree_local_file_mutation"`) |
/// | `VcsSoftGate`               | `VCS_SOFT_GATE` (`"destructive.vcs_soft_gate"`)      |
/// | `DbSoftGate`                | `DB_SOFT_GATE` (`"destructive.db_soft_gate"`)        |
///
/// Callers in `djinn-agent` should convert via the `as_str()` label
/// (or a dedicated mapping function) rather than importing this type
/// across crate boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShellDestructiveClass {
    /// Worktree-local file mutation (e.g. `rm`, `mv`, `mkdir`, `touch`,
    /// `truncate`, `sed -i`, output redirection to a relative path).
    WorktreeLocalFileMutation,
    /// Soft-gated VCS mutation (reserved for future carve-outs).
    VcsSoftGate,
    /// Soft-gated database mutation (reserved for future carve-outs).
    DbSoftGate,
}

impl ShellDestructiveClass {
    /// Return the stable label that matches the corresponding
    /// `DestructiveClass` constant in `djinn-agent::file_time::destructive_class`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::WorktreeLocalFileMutation => "destructive.worktree_local_file_mutation",
            Self::VcsSoftGate => "destructive.vcs_soft_gate",
            Self::DbSoftGate => "destructive.db_soft_gate",
        }
    }
}

impl fmt::Display for ShellDestructiveClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Decision returned by [`classify_destructive_shell_command`].
///
/// Unlike the fail-closed [`crate::command_validator::validate_read_only_command`] (which rejects
/// anything not on an explicit allowlist), the production classifier is
/// **allow-by-default**: ordinary build/read/test commands pass through
/// without restriction.  Only commands that match explicit destructive
/// patterns produce a `HardDeny` or `SoftGate` outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellDestructiveDecision {
    /// The command does not match any destructive pattern.
    Allow,
    /// The command is unconditionally forbidden.  No session-level
    /// override or retry can lift this restriction.
    HardDeny {
        /// Human-readable explanation of why the command is forbidden.
        reason: String,
    },
    /// The command is gated: the worker must produce a FORCE plan
    /// describing what will be mutated and a rollback strategy before
    /// the command is permitted.  Once the class has been soft-forced in
    /// a session, subsequent commands in the same class proceed without
    /// re-prompting.
    SoftGate {
        /// The soft-gate category this command belongs to.
        class: ShellDestructiveClass,
        /// Human-readable explanation of what the command mutates.
        reason: String,
    },
}

impl fmt::Display for ShellDestructiveDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allow => write!(f, "allowed"),
            Self::HardDeny { reason } => write!(f, "hard-deny: {reason}"),
            Self::SoftGate { class, reason } => {
                write!(f, "soft-gate({class}): {reason}")
            }
        }
    }
}

/// Classify a shell command for production worker use.
///
/// This classifier is **allow-by-default**: only commands that match
/// explicit destructive patterns produce a [`ShellDestructiveDecision`]
/// other than `Allow`.  This is the opposite of
/// [`crate::command_validator::validate_read_only_command`], which is fail-closed (reject-unknown).
///
/// # Hard-deny categories
///
/// - `git reset --hard`, `git clean`, `git stash` mutation forms
/// - Force-push (`git push --force/-f`) and remote config mutation
/// - Package installs/publishes (`cargo install`, `pip install`,
///   `npm install`, `apt install`, etc.)
/// - Network mutation forms (`curl -X POST/-d/--data`, `wget --post-data`)
/// - DB DDL/DML (`DROP TABLE`, `DELETE FROM`, `TRUNCATE`, `ALTER`,
///   `INSERT`, `UPDATE` through DB CLIs)
/// - Commands targeting protected paths (`.git/`, `..`, absolute paths,
///   `.djinn-read-sources`, durable data files)
///
/// # Soft-gate categories
///
/// - `WorktreeLocalFileMutation`: lower-risk local worktree mutations
///   scoped to relative, non-protected paths (`rm`, `mv`, `mkdir`,
///   `touch`, `truncate`, `sed -i`, `chmod`, `ln`, output redirection)
pub fn classify_destructive_shell_command(command: &str) -> ShellDestructiveDecision {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return ShellDestructiveDecision::Allow;
    }

    // Check for output redirects — may be HardDeny (protected target)
    // or SoftGate (safe relative target).
    if let Some(decision) = classify_redirects(trimmed) {
        return decision;
    }

    // Split on command chains (&&, ||, ;) and then on pipes.
    let chain_parts = split_chains(trimmed);
    let mut worst = ShellDestructiveDecision::Allow;
    for part in &chain_parts {
        let segments = split_pipeline(part.trim());
        for segment in &segments {
            let seg_decision = classify_segment(segment.trim());
            worst = most_restrictive(worst, seg_decision);
        }
    }
    worst
}

// ── Internal classification helpers ──────────────────────────────────────

/// Return the more restrictive of two decisions.
fn most_restrictive(
    a: ShellDestructiveDecision,
    b: ShellDestructiveDecision,
) -> ShellDestructiveDecision {
    match (&a, &b) {
        // HardDeny always wins.
        (ShellDestructiveDecision::HardDeny { .. }, _) => a,
        (_, ShellDestructiveDecision::HardDeny { .. }) => b,
        // SoftGate beats Allow.
        (ShellDestructiveDecision::SoftGate { .. }, _) => a,
        (_, ShellDestructiveDecision::SoftGate { .. }) => b,
        // Both Allow.
        _ => ShellDestructiveDecision::Allow,
    }
}

/// Split a command on `&&`, `||`, and `;` while respecting quotes.
fn split_chains(cmd: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;

    while i < chars.len() {
        match chars[i] {
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(chars[i]);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(chars[i]);
            }
            '&' if !in_single && !in_double && i + 1 < chars.len() && chars[i + 1] == '&' => {
                parts.push(current.clone());
                current.clear();
                i += 2;
                continue;
            }
            '|' if !in_single && !in_double && i + 1 < chars.len() && chars[i + 1] == '|' => {
                parts.push(current.clone());
                current.clear();
                i += 2;
                continue;
            }
            ';' if !in_single && !in_double => {
                parts.push(current.clone());
                current.clear();
                i += 1;
                continue;
            }
            _ => {
                current.push(chars[i]);
            }
        }
        i += 1;
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Check for output redirects and classify the target path.
fn classify_redirects(cmd: &str) -> Option<ShellDestructiveDecision> {
    let tokens = tokenize(cmd);
    for (i, token) in tokens.iter().enumerate() {
        // fd-to-fd redirection (e.g. 2>&1) — never file creation.
        if token.contains(">&") {
            continue;
        }
        // Standalone > or >>
        if (token == ">" || token == ">>") && i + 1 < tokens.len() {
            let target = &tokens[i + 1];
            return Some(classify_redirect_target(target));
        }
        // Combined: >file, >>file, N>file, N>>file, &>file
        if token.contains('>') && token.len() > 1 {
            let gt_pos = token.find('>').unwrap();
            let prefix: String = token[..gt_pos].chars().take_while(|c| *c != '>').collect();
            if prefix.is_empty() || prefix.chars().all(|c| c.is_ascii_digit() || c == '&') {
                let after = token[gt_pos + 1..]
                    .strip_prefix('>')
                    .unwrap_or(&token[gt_pos + 1..]);
                if !after.is_empty() {
                    return Some(classify_redirect_target(after));
                }
            }
        }
    }
    None
}

/// Classify a redirect target path.
fn classify_redirect_target(target: &str) -> ShellDestructiveDecision {
    if target == "/dev/null" || target == "&1" || target == "&2" {
        return ShellDestructiveDecision::Allow;
    }
    if path_is_protected(target) {
        ShellDestructiveDecision::HardDeny {
            reason: format!("redirect to protected path: {target}"),
        }
    } else {
        ShellDestructiveDecision::SoftGate {
            class: ShellDestructiveClass::WorktreeLocalFileMutation,
            reason: format!("redirect to {target}"),
        }
    }
}

/// Classify a single pipeline segment (no pipes).
fn classify_segment(segment: &str) -> ShellDestructiveDecision {
    let tokens = tokenize(segment);
    if tokens.is_empty() {
        return ShellDestructiveDecision::Allow;
    }

    // Skip leading env assignments (FOO=bar).
    let cmd_start = tokens
        .iter()
        .position(|t| !t.contains('=') || t.starts_with('='))
        .unwrap_or(0);

    if cmd_start >= tokens.len() {
        return ShellDestructiveDecision::Allow;
    }

    let first = &tokens[cmd_start];

    // sudo/doas: recurse into inner command.
    if first == "sudo" || first == "doas" {
        let inner_tokens: Vec<&str> = tokens[cmd_start + 1..].iter().map(|s| s.as_str()).collect();
        if inner_tokens.is_empty() {
            return ShellDestructiveDecision::Allow;
        }
        let inner_cmd = inner_tokens.join(" ");
        return classify_segment(&inner_cmd);
    }

    // env: skip to actual command.
    if first == "env" {
        let remaining: Vec<&str> = tokens[cmd_start + 1..].iter().map(|s| s.as_str()).collect();
        if remaining.is_empty() {
            return ShellDestructiveDecision::Allow;
        }
        let inner_cmd = remaining.join(" ");
        return classify_segment(&inner_cmd);
    }

    match first.as_str() {
        // ── VCS ────────────────────────────────────────────────────
        "git" => classify_git(&tokens[cmd_start..]),
        // ── Network ────────────────────────────────────────────────
        "curl" => classify_curl(&tokens[cmd_start..]),
        "wget" => classify_wget(&tokens[cmd_start..]),
        "ssh" | "scp" | "rsync" | "nc" | "ncat" | "netcat" => ShellDestructiveDecision::HardDeny {
            reason: format!("{first}: remote/network execution is hard-denied"),
        },
        // ── Package managers ───────────────────────────────────────
        "cargo" => classify_cargo(&tokens[cmd_start..]),
        "pip" | "pip3" => classify_pip(&tokens[cmd_start..]),
        "npm" | "pnpm" | "yarn" => classify_npm(&tokens[cmd_start..]),
        "apt" | "apt-get" | "yum" | "dnf" | "pacman" | "brew" | "apk" => {
            classify_system_pkg(&tokens[cmd_start..], first)
        }
        // ── Database CLIs ──────────────────────────────────────────
        "psql" | "mysql" | "sqlite3" => classify_db_cli(&tokens[cmd_start..]),
        "redis-cli" => classify_redis_cli(&tokens[cmd_start..]),
        // ── File mutation commands ──────────────────────────────────
        "rm" | "rmdir" => classify_file_mutation(&tokens[cmd_start..], first),
        "mv" | "cp" => classify_file_mutation(&tokens[cmd_start..], first),
        "mkdir" | "touch" | "ln" => classify_file_mutation(&tokens[cmd_start..], first),
        "chmod" | "chown" | "chgrp" => classify_file_mutation(&tokens[cmd_start..], first),
        "truncate" => classify_file_mutation(&tokens[cmd_start..], first),
        "dd" => ShellDestructiveDecision::HardDeny {
            reason: "dd performs raw block-level writes".into(),
        },
        "install" => ShellDestructiveDecision::HardDeny {
            reason: "install(1) writes files to system paths".into(),
        },
        "sed" => classify_sed(&tokens[cmd_start..]),
        "tee" => classify_tee(&tokens[cmd_start..]),
        // ── xargs: recurse into inner command ──────────────────────
        "xargs" => classify_xargs(&tokens[cmd_start..]),
        // ── Everything else: allow ─────────────────────────────────
        _ => ShellDestructiveDecision::Allow,
    }
}

// ── Command-specific classifiers ─────────────────────────────────────────

fn classify_git(tokens: &[String]) -> ShellDestructiveDecision {
    if tokens.len() < 2 {
        return ShellDestructiveDecision::Allow;
    }
    let subcmd = tokens[1].as_str();

    match subcmd {
        // git reset --hard destroys uncommitted changes.
        "reset" => {
            if tokens.iter().any(|t| t == "--hard") {
                ShellDestructiveDecision::HardDeny {
                    reason: "git reset --hard destroys uncommitted changes".into(),
                }
            } else {
                ShellDestructiveDecision::Allow
            }
        }
        // git clean deletes untracked files.
        "clean" => ShellDestructiveDecision::HardDeny {
            reason: "git clean deletes untracked files and directories".into(),
        },
        // git stash mutation forms.
        "stash" => {
            if tokens.len() >= 3 {
                match tokens[2].as_str() {
                    "list" | "show" | "diff" => ShellDestructiveDecision::Allow,
                    action => ShellDestructiveDecision::HardDeny {
                        reason: format!("git stash {action} mutates stash state"),
                    },
                }
            } else {
                // bare `git stash` = `git stash push`
                ShellDestructiveDecision::HardDeny {
                    reason: "git stash (push) mutates stash state".into(),
                }
            }
        }
        // Force push.
        "push" => {
            if tokens
                .iter()
                .any(|t| t == "--force" || t == "-f" || t.starts_with("--force"))
            {
                ShellDestructiveDecision::HardDeny {
                    reason: "force-push can overwrite remote history".into(),
                }
            } else {
                ShellDestructiveDecision::Allow
            }
        }
        // Remote config mutation.
        "remote" => {
            if tokens.len() >= 3 {
                match tokens[2].as_str() {
                    "add" | "remove" | "rm" | "rename" | "set-url" | "set-branches" => {
                        ShellDestructiveDecision::HardDeny {
                            reason: format!(
                                "git remote {} modifies remote configuration",
                                tokens[2]
                            ),
                        }
                    }
                    _ => ShellDestructiveDecision::Allow,
                }
            } else {
                ShellDestructiveDecision::Allow
            }
        }
        // Other git commands (commit, merge, rebase, checkout, etc.) are
        // normal worker operations — allow.
        _ => ShellDestructiveDecision::Allow,
    }
}

fn classify_curl(tokens: &[String]) -> ShellDestructiveDecision {
    let mut i = 1;
    while i < tokens.len() {
        match tokens[i].as_str() {
            "-X" | "--request" => {
                if i + 1 < tokens.len() {
                    let method = tokens[i + 1].to_uppercase();
                    if method != "GET" && method != "HEAD" {
                        return ShellDestructiveDecision::HardDeny {
                            reason: format!("curl -X {method} is a network mutation"),
                        };
                    }
                    i += 2;
                    continue;
                }
            }
            "-d" | "--data" | "--data-raw" | "--data-binary" | "--data-urlencode" | "--form"
            | "-F" => {
                return ShellDestructiveDecision::HardDeny {
                    reason: "curl with data payload is a network mutation".into(),
                };
            }
            "-T" | "--upload-file" => {
                return ShellDestructiveDecision::HardDeny {
                    reason: "curl --upload-file is a network mutation".into(),
                };
            }
            _ => {}
        }
        i += 1;
    }
    ShellDestructiveDecision::Allow
}

fn classify_wget(tokens: &[String]) -> ShellDestructiveDecision {
    for tok in &tokens[1..] {
        if tok.starts_with("--post-data") || tok.starts_with("--post-file") {
            return ShellDestructiveDecision::HardDeny {
                reason: "wget with POST data is a network mutation".into(),
            };
        }
        if tok == "--method=POST" || tok == "--method=PUT" || tok == "--method=PATCH" {
            return ShellDestructiveDecision::HardDeny {
                reason: format!("wget {tok} is a network mutation"),
            };
        }
    }
    ShellDestructiveDecision::Allow
}

fn classify_cargo(tokens: &[String]) -> ShellDestructiveDecision {
    if tokens.len() < 2 {
        return ShellDestructiveDecision::Allow;
    }
    let subcmd = match tokens[1].as_str() {
        "c" => "check",
        "b" => "build",
        "t" => "test",
        "r" => "run",
        other => other,
    };
    match subcmd {
        "install" | "uninstall" => ShellDestructiveDecision::HardDeny {
            reason: format!("cargo {subcmd} modifies the global toolchain"),
        },
        "publish" => ShellDestructiveDecision::HardDeny {
            reason: "cargo publish publishes a crate to a registry".into(),
        },
        "new" | "init" | "add" | "remove" | "update" | "login" | "owner" | "yank" | "package" => {
            ShellDestructiveDecision::HardDeny {
                reason: format!("cargo {subcmd} is a package/registry mutation"),
            }
        }
        _ => ShellDestructiveDecision::Allow,
    }
}

fn classify_pip(tokens: &[String]) -> ShellDestructiveDecision {
    if tokens.len() < 2 {
        return ShellDestructiveDecision::Allow;
    }
    match tokens[1].as_str() {
        "install" | "uninstall" | "download" => ShellDestructiveDecision::HardDeny {
            reason: format!("pip {} modifies installed packages", tokens[1]),
        },
        _ => ShellDestructiveDecision::Allow,
    }
}

fn classify_npm(tokens: &[String]) -> ShellDestructiveDecision {
    if tokens.len() < 2 {
        return ShellDestructiveDecision::Allow;
    }
    match tokens[1].as_str() {
        "install" | "add" | "remove" | "uninstall" | "update" | "link" | "unlink" => {
            ShellDestructiveDecision::HardDeny {
                reason: format!("npm {} modifies node_modules", tokens[1]),
            }
        }
        "publish" | "deploy" => ShellDestructiveDecision::HardDeny {
            reason: format!("npm {} publishes/deploys the package", tokens[1]),
        },
        _ => ShellDestructiveDecision::Allow,
    }
}

fn classify_system_pkg(tokens: &[String], cmd: &str) -> ShellDestructiveDecision {
    if tokens.len() < 2 {
        return ShellDestructiveDecision::Allow;
    }
    match tokens[1].as_str() {
        "install" | "remove" | "uninstall" | "purge" | "autoremove" | "upgrade"
        | "dist-upgrade" | "update" => ShellDestructiveDecision::HardDeny {
            reason: format!("{cmd} {} modifies system packages", tokens[1]),
        },
        _ => ShellDestructiveDecision::Allow,
    }
}

fn classify_db_cli(tokens: &[String]) -> ShellDestructiveDecision {
    let mut i = 1;
    while i < tokens.len() {
        let tok = &tokens[i];
        // -c/--command/-e/--execute flag with SQL content.
        if (tok == "-c" || tok == "--command" || tok == "-e" || tok == "--execute")
            && i + 1 < tokens.len()
        {
            if let Some(decision) = classify_sql(&tokens[i + 1]) {
                return decision;
            }
            i += 2;
            continue;
        }
        // Inline SQL without flag (e.g. `sqlite3 db.db "INSERT..."`)
        if !tok.starts_with('-')
            && let Some(decision) = classify_sql(tok)
        {
            return decision;
        }
        i += 1;
    }
    ShellDestructiveDecision::Allow
}

/// Check if a SQL string contains DDL/DML keywords.
fn classify_sql(sql: &str) -> Option<ShellDestructiveDecision> {
    let lower = sql.to_lowercase();
    let trimmed = lower.trim_start();
    for keyword in SQL_MUTATION_KEYWORDS {
        if trimmed.starts_with(keyword) || trimmed.contains(&format!(" {keyword} ")) {
            return Some(ShellDestructiveDecision::HardDeny {
                reason: format!("SQL {keyword} is a database mutation"),
            });
        }
    }
    None
}

fn classify_redis_cli(tokens: &[String]) -> ShellDestructiveDecision {
    let mut i = 1;
    while i < tokens.len() {
        let tok = &tokens[i];
        if tok == "-c" && i + 1 < tokens.len() {
            let cmd = tokens[i + 1].to_lowercase();
            if is_redis_write_command(&cmd) {
                return ShellDestructiveDecision::HardDeny {
                    reason: format!("redis-cli {cmd} is a database mutation"),
                };
            }
            i += 2;
            continue;
        }
        if !tok.starts_with('-') {
            let lower = tok.to_lowercase();
            if is_redis_write_command(&lower) {
                return ShellDestructiveDecision::HardDeny {
                    reason: format!("redis-cli {lower} is a database mutation"),
                };
            }
        }
        i += 1;
    }
    ShellDestructiveDecision::Allow
}

/// Classify a file-mutation command (`rm`, `mv`, `cp`, `mkdir`, `touch`,
/// `ln`, `chmod`, `chown`, `truncate`, etc.) based on target paths.
///
/// Commands targeting protected paths are `HardDeny`; commands with only
/// safe relative paths are `SoftGate(WorktreeLocalFileMutation)`.
fn classify_file_mutation(tokens: &[String], cmd_name: &str) -> ShellDestructiveDecision {
    // Collect non-flag arguments (the paths).
    let paths: Vec<&str> = tokens[1..]
        .iter()
        .filter(|t| !t.starts_with('-'))
        .map(|s| s.as_str())
        .collect();

    if paths.is_empty() {
        return ShellDestructiveDecision::Allow;
    }

    for path in &paths {
        if path_is_protected(path) {
            return ShellDestructiveDecision::HardDeny {
                reason: format!("{cmd_name} targets protected path: {path}"),
            };
        }
    }

    ShellDestructiveDecision::SoftGate {
        class: ShellDestructiveClass::WorktreeLocalFileMutation,
        reason: format!("{cmd_name} {}", paths.join(" ")),
    }
}

fn classify_sed(tokens: &[String]) -> ShellDestructiveDecision {
    let has_in_place = tokens.iter().any(|t| t == "-i" || t.starts_with("-i"));
    if !has_in_place {
        return ShellDestructiveDecision::Allow;
    }

    // sed -i targets the last non-flag argument (the file).
    let paths: Vec<&str> = tokens[1..]
        .iter()
        .filter(|t| !t.starts_with('-'))
        .map(|s| s.as_str())
        .collect();

    if let Some(path) = paths.last()
        && path_is_protected(path)
    {
        return ShellDestructiveDecision::HardDeny {
            reason: format!("sed -i targets protected path: {path}"),
        };
    }

    ShellDestructiveDecision::SoftGate {
        class: ShellDestructiveClass::WorktreeLocalFileMutation,
        reason: "sed -i performs in-place file editing".into(),
    }
}

fn classify_tee(tokens: &[String]) -> ShellDestructiveDecision {
    // tee with no file args is Allow (writes to stdout only).
    let file_args: Vec<&str> = tokens[1..]
        .iter()
        .filter(|t| !t.starts_with('-'))
        .map(|s| s.as_str())
        .collect();

    if file_args.is_empty() {
        return ShellDestructiveDecision::Allow;
    }

    for path in &file_args {
        if path_is_protected(path) {
            return ShellDestructiveDecision::HardDeny {
                reason: format!("tee targets protected path: {path}"),
            };
        }
    }

    ShellDestructiveDecision::SoftGate {
        class: ShellDestructiveClass::WorktreeLocalFileMutation,
        reason: format!("tee {}", file_args.join(" ")),
    }
}

fn classify_xargs(tokens: &[String]) -> ShellDestructiveDecision {
    if tokens.len() < 2 {
        return ShellDestructiveDecision::Allow;
    }
    let first_arg = &tokens[1];
    if first_arg.starts_with('-') {
        // xargs with only flags — defaults to echo (safe).
        return ShellDestructiveDecision::Allow;
    }
    // The first non-flag arg is the command to run.
    let inner: Vec<&str> = tokens[1..].iter().map(|s| s.as_str()).collect();
    let inner_cmd = inner.join(" ");
    let decision = classify_segment(&inner_cmd);
    // When xargs feeds paths via stdin to a file-mutation command that
    // has no explicit path arguments (e.g. `xargs rm`), the command is
    // still a worktree mutation — classify as SoftGate.
    if decision == ShellDestructiveDecision::Allow
        && is_file_mutation_command(first_arg)
        && inner.iter().skip(1).all(|t| t.starts_with('-'))
    {
        return ShellDestructiveDecision::SoftGate {
            class: ShellDestructiveClass::WorktreeLocalFileMutation,
            reason: format!("xargs {first_arg} mutates paths from stdin"),
        };
    }
    decision
}

/// Return `true` if the command is a known file-mutation tool that
/// operates on path arguments (and will therefore mutate files when
/// fed paths via stdin through `xargs`).
fn is_file_mutation_command(cmd: &str) -> bool {
    matches!(
        cmd,
        "rm" | "rmdir"
            | "mv"
            | "cp"
            | "chmod"
            | "chown"
            | "chgrp"
            | "ln"
            | "mkdir"
            | "touch"
            | "truncate"
            | "tee"
    )
}

// ── Path classification ─────────────────────────────────────────────────

/// Return `true` if a path is "protected" and mutations targeting it must
/// be `HardDeny` rather than `SoftGate`.
///
/// Protected paths include:
/// - Absolute paths (outside the worktree — we cannot verify containment)
/// - Parent-directory traversal (`..`)
/// - `.git/` directory and its contents
/// - `.djinn-read-sources/` (read-only sibling checkouts)
/// - Durable data paths (project metadata and lock files)
fn path_is_protected(path: &str) -> bool {
    // Absolute paths.
    if path.starts_with('/') {
        return true;
    }
    // Parent-directory traversal.
    if path == ".." || path.starts_with("../") || path.starts_with("..\\") || path.contains("/../")
    {
        return true;
    }
    // .git directory.
    if path == ".git"
        || path.starts_with(".git/")
        || path.starts_with(".git\\")
        || path.contains("/.git/")
    {
        return true;
    }
    // .djinn-read-sources (read-only sibling checkouts).
    if path.starts_with(".djinn-read-sources") || path.contains("/.djinn-read-sources") {
        return true;
    }
    // Durable data paths — project metadata that should never be casually
    // deleted or overwritten by a worker without explicit review.
    if is_durable_data_path(path) {
        return true;
    }
    false
}

/// Return `true` if the path's basename matches a well-known durable
/// project-data file that should not be soft-gated.
fn is_durable_data_path(path: &str) -> bool {
    let basename = path.rsplit('/').next().unwrap_or(path);
    matches!(
        basename,
        "Cargo.toml"
            | "Cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "pnpm-lock.yml"
            | ".env"
            | ".env.local"
            | ".env.production"
            | "docker-compose.yml"
            | "docker-compose.yaml"
            | "compose.yml"
            | "compose.yaml"
            | "Dockerfile"
            | "Makefile"
            | "CMakeLists.txt"
            | ".gitignore"
            | ".gitattributes"
            | ".dockerignore"
    )
}

// ── Redis write-command helper (shared with read-only validator) ────────

pub(crate) fn is_redis_write_command(cmd: &str) -> bool {
    matches!(
        cmd,
        "set"
            | "del"
            | "hset"
            | "hdel"
            | "lpush"
            | "rpush"
            | "lpop"
            | "rpop"
            | "sadd"
            | "srem"
            | "zadd"
            | "zrem"
            | "incr"
            | "decr"
            | "incrby"
            | "decrby"
            | "expire"
            | "persist"
            | "rename"
            | "flushdb"
            | "flushall"
            | "mset"
            | "append"
            | "bitfield"
            | "xadd"
            | "xdel"
            | "xtrim"
            | "unlink"
            | "move"
            | "swapdb"
            | "select"
    )
}

#[cfg(test)]
#[path = "command_classifier_tests.rs"]
mod tests;
