//! Fail-closed read-only shell command validator for evidence-spike sessions.
//!
//! The [`validate_read_only_command`] function inspects a shell command string
//! and rejects any invocation that could mutate the filesystem, network, VCS
//! state, database state, or installed packages.  It is designed to be
//! unit-testable without actually executing commands: every violation class
//! has a dedicated test in the companion `#[cfg(test)]` module.
//!
//! # Design principles
//!
//! - **Fail-closed**: commands that cannot be confidently classified as
//!   read-only are rejected.  Unknown subcommands, unparseable pipelines,
//!   and unfamiliar tools all produce a violation.
//! - **No execution**: the validator inspects the command text only; it never
//!   runs a subprocess.
//! - **Defense-in-depth**: this validator is an additional gate beyond the
//!   tool-schema restriction (evidence-spike sessions already exclude `shell`
//!   from their tool surface).  If the schema restriction is ever loosened to
//!   allow a limited shell, this validator provides the second layer.

use std::fmt;

/// Classifies the kind of violation detected in a shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandViolation {
    /// Output redirection (`>`, `>>`, `| tee`, etc.).
    Redirect,
    /// Heredoc or here-string that writes to a file descriptor.
    HeredocWrite,
    /// File mutation: `rm`, `mv`, `chmod`, `chown`, `touch`, `mkdir`,
    /// `install`, `dd`, `truncate`, `ln -f`, etc.
    FileMutation,
    /// Package manager install/add command.
    PackageInstall,
    /// Network mutation: `curl -X POST`, `curl -d`, `wget --post-data`,
    /// `nc` send, `ssh` with remote command, etc.
    NetworkMutation,
    /// VCS mutation: `git commit`, `git push`, `git merge`, `git rebase`,
    /// `git reset`, `git clean`, `git stash`, etc.
    VcsMutation,
    /// Database or product mutation: SQL DML/DDL via CLI clients.
    DatabaseMutation,
    /// Suspicious command chain that could hide mutation
    /// (e.g. chaining with `&&`/`||`/`;` where a later segment mutates).
    SuspiciousChain,
    /// The command uses a tool or syntax the validator cannot confidently
    /// classify as read-only.
    UnknownTool,
}

impl fmt::Display for CommandViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Redirect => write!(f, "output redirection is not allowed"),
            Self::HeredocWrite => write!(f, "heredoc/here-string writes are not allowed"),
            Self::FileMutation => write!(f, "file mutation commands are not allowed"),
            Self::PackageInstall => write!(f, "package install commands are not allowed"),
            Self::NetworkMutation => write!(f, "network mutation commands are not allowed"),
            Self::VcsMutation => write!(f, "VCS mutation commands are not allowed"),
            Self::DatabaseMutation => write!(f, "database mutation commands are not allowed"),
            Self::SuspiciousChain => {
                write!(f, "suspicious command chains are not allowed")
            }
            Self::UnknownTool => write!(
                f,
                "command uses a tool that is not in the read-only allowlist"
            ),
        }
    }
}

// ── Allowlist ────────────────────────────────────────────────────────────────

/// Shell commands (first word / simple-command prefix) that are permitted
/// in read-only evidence-spike sessions.  Anything not in this list is
/// rejected by [`validate_read_only_command`].
///
/// This is intentionally short and explicit.  New read-only commands must
/// be added here deliberately.
const ALLOWED_COMMANDS: &[&str] = &[
    // ── File reading ──────────────────────────────────────────────────
    "cat",
    "head",
    "tail",
    "less",
    "more",
    "wc",
    "file",
    "stat",
    "du",
    "df",
    "ls",
    "dir",
    "tree",
    "find",
    // ── Search / filter ───────────────────────────────────────────────
    "grep",
    "rg",
    "ag",
    "ack",
    "egrep",
    "fgrep",
    "zgrep",
    "zfgrep",
    "sed", // reading/transforming to stdout (no -i)
    "awk", // reading/transforming to stdout
    "cut",
    "sort",
    "uniq",
    "tr",
    "tee", // handled specially: allowed only without file args
    "column",
    "comm",
    "diff",
    "jq",
    "xargs", // guarded: must not chain to a mutation command
    // ── Environment / diagnostics ─────────────────────────────────────
    "echo",
    "printf",
    "env",
    "printenv",
    "which",
    "type",
    "command",
    "whoami",
    "id",
    "uname",
    "hostname",
    "date",
    "uptime",
    "pwd",
    "realpath",
    "readlink",
    "basename",
    "dirname",
    // ── Process / system info ─────────────────────────────────────────
    "ps",
    "top",
    "htop",
    "free",
    "lsof",
    "pgrep",
    "kill", // signal only; cannot mutate files
    "pkill",
    // ── VCS read-only ─────────────────────────────────────────────────
    "git",
    // `git` is handled specially: only read-only sub-commands are allowed.
    // ── Network read-only ─────────────────────────────────────────────
    "curl",
    // `curl` is handled specially: -X POST/-d/--data are rejected.
    "wget",
    // `wget` is handled specially: --post-data/--method=POST are rejected.
    // ── Package manager read-only queries ─────────────────────────────
    "cargo",
    // `cargo` is handled specially: only read-only sub-commands allowed.
    "pip",
    "pip3",
    "npm",
    "pnpm",
    "yarn",
    "node",
    "deno",
    "bun",
    "python",
    "python3",
    "ruby",
    // ── Database read-only queries ────────────────────────────────────
    "psql",
    "mysql",
    "sqlite3",
    "redis-cli",
    // ── Misc read-only ────────────────────────────────────────────────
    "sha256sum",
    "sha1sum",
    "md5sum",
    "base64",
    "xxd",
    "hexdump",
    "od",
    "strings",
];

/// Git sub-commands that are allowed in read-only mode.
const ALLOWED_GIT_SUBCOMMANDS: &[&str] = &[
    "log",
    "show",
    "diff",
    "status",
    "branch", // listing only; no -d/-D
    "tag",    // listing only; no -d
    "describe",
    "rev-parse",
    "rev-list",
    "ls-files",
    "ls-tree",
    "ls-remote",
    "remote", // listing only; no add/remove/rename
    "blame",
    "shortlog",
    "count-objects",
    "config", // get/list only; no --set/--unset
    "grep",
    "stash", // `stash list`/`stash show` only
];

/// Git sub-commands that are ALWAYS mutation and must be rejected.
const DENIED_GIT_SUBCOMMANDS: &[&str] = &[
    "commit",
    "push",
    "pull",
    "merge",
    "rebase",
    "reset",
    "clean",
    "checkout",
    "cherry-pick",
    "revert",
    "apply",
    "am",
    "submodule",
    "worktree",
    "gc",
    "prune",
    "reflog", // can trigger auto-gc
    "notes",
    "replace",
    "filter-branch",
    "fast-import",
    "fast-export",
];

/// Cargo sub-commands that are allowed in read-only mode.
const ALLOWED_CARGO_SUBCOMMANDS: &[&str] = &[
    "check",
    "build",
    "test",
    "bench",
    "clippy",
    "fmt",
    "doc",
    "tree",
    "metadata",
    "locate-project",
    "version",
    "search",
    "help",
    "info",
];

/// Cargo sub-commands that are ALWAYS mutation and must be rejected.
const DENIED_CARGO_SUBCOMMANDS: &[&str] = &[
    "install",
    "uninstall",
    "new",
    "init",
    "add",
    "remove",
    "update",
    "publish",
    "login",
    "owner",
    "yank",
    "package",
];

/// SQL keywords that indicate DML/DDL mutation when used as a command
/// prefix (e.g. `psql -c "DELETE ..."`).
const SQL_MUTATION_KEYWORDS: &[&str] = &[
    "insert", "update", "delete", "drop", "create", "alter", "truncate", "grant", "revoke",
    "rename", "replace", "merge", "upsert",
];

// ── Validator ───────────────────────────────────────────────────────────────

/// Validate that a shell command is safe for a read-only evidence-spike
/// session.  Returns `Ok(())` if the command is allowed, or `Err` with
/// the list of violations detected.
///
/// The validator is fail-closed: commands that cannot be confidently
/// classified as read-only are rejected.
pub fn validate_read_only_command(cmd: &str) -> Result<(), Vec<CommandViolation>> {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    let mut violations = Vec::new();

    // Check for redirects anywhere in the command.
    check_redirects(trimmed, &mut violations);

    // Check for heredoc writes.
    check_heredoc_writes(trimmed, &mut violations);

    // Split into pipeline segments and check each.
    // We split on `|` (pipe) but NOT `||` (logical OR, handled as chain).
    let segments = split_pipeline(trimmed);
    for segment in &segments {
        check_segment(segment.trim(), &mut violations);
    }

    // Check for suspicious command chains.
    check_chains(trimmed, &mut violations);

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// Check for output redirection operators.
fn check_redirects(cmd: &str, violations: &mut Vec<CommandViolation>) {
    // Split into tokens, being careful about quoted strings.
    let tokens = tokenize(cmd);
    for (i, token) in tokens.iter().enumerate() {
        // `>` or `>>` at the start or standalone
        if token == ">" || token == ">>" {
            violations.push(CommandViolation::Redirect);
            return;
        }
        // `1>`, `2>`, `&>` patterns (standalone like `2>` or `2>file`)
        if token.contains('>') && token.len() > 1 {
            // Check for patterns: N>path, N>>path, &>path, >path, >>path
            // where the prefix before > is only digits or &
            let prefix: String = token.chars().take_while(|c| *c != '>').collect();
            if prefix.is_empty() || prefix.chars().all(|c| c.is_ascii_digit() || c == '&') {
                // Only flag if there's actually a > in the token
                if token.contains('>') {
                    violations.push(CommandViolation::Redirect);
                    return;
                }
            }
        }
        // `tee` with a file argument (tee normally writes to file + stdout)
        if token == "tee" && i + 1 < tokens.len() {
            let next = &tokens[i + 1];
            if !next.starts_with('-') {
                // `tee filename` writes to a file
                violations.push(CommandViolation::Redirect);
                return;
            }
        }
    }

    // Also check for `>` in raw text that might be missed by tokenizer
    // (e.g. unquoted `>` in complex expressions).
    // We already handle via tokenizer, but catch edge cases.
    if cmd.contains(">&") {
        violations.push(CommandViolation::Redirect);
    }
}

/// Check for heredoc/here-string write patterns.
fn check_heredoc_writes(cmd: &str, violations: &mut Vec<CommandViolation>) {
    // `<<` (heredoc) or `<<<` (here-string) — these are typically used for
    // input, but `<<EOF >file` or `cat <<EOF > file` are write patterns.
    // Since we already check redirects, heredoc + redirect is caught.
    // But `cat <<EOF | tee file` is also a write pattern.
    //
    // For safety, reject any command with `<<` or `<<<` unless it's clearly
    // read-only (heredoc to stdin only, no redirect/tee).
    if cmd.contains("<<<") {
        // Here-string: `cat <<< "text"` is read-only input, but
        // `tee file <<< "text"` writes.  Conservative: reject all.
        violations.push(CommandViolation::HeredocWrite);
    }
    // `<<` without `<<-` is a heredoc; allow it only if no redirect follows.
    // Since redirects are already caught, we only need to flag `<<` when
    // combined with tee or other write patterns.
    if cmd.contains("<<") && !cmd.contains("<<<") {
        // Check if heredoc is used with a write-capable command.
        let lower = cmd.to_lowercase();
        if lower.contains("<<eof") || lower.contains("<< eot") || lower.contains("<<eot") {
            // common heredoc markers — conservative rejection
            violations.push(CommandViolation::HeredocWrite);
        }
    }
}

/// Split a command into pipeline segments on `|` (but not `||`).
fn split_pipeline(cmd: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while i < chars.len() {
        match chars[i] {
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
                current.push(chars[i]);
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
                current.push(chars[i]);
            }
            '|' if !in_single_quote && !in_double_quote => {
                if i + 1 < chars.len() && chars[i + 1] == '|' {
                    // `||` — logical OR, not a pipe. Keep in same segment.
                    current.push(chars[i]);
                    current.push(chars[i + 1]);
                    i += 2;
                    continue;
                } else {
                    segments.push(current.clone());
                    current.clear();
                }
            }
            _ => {
                current.push(chars[i]);
            }
        }
        i += 1;
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// Simple tokenizer that respects single and double quotes.
fn tokenize(cmd: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while i < chars.len() {
        match chars[i] {
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
                // Don't include the quote character in the token
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
                // Don't include the quote character in the token
            }
            ' ' | '\t' if !in_single_quote && !in_double_quote => {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            }
            _ => {
                current.push(chars[i]);
            }
        }
        i += 1;
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Check a single pipeline segment (no pipes) for violations.
fn check_segment(segment: &str, violations: &mut Vec<CommandViolation>) {
    let tokens = tokenize(segment);
    if tokens.is_empty() {
        return;
    }

    // Find the first non-env-assignment token (skip `FOO=bar` prefixes).
    let cmd_start = tokens
        .iter()
        .position(|t| !t.contains('=') || t.starts_with('='))
        .unwrap_or(0);

    if cmd_start >= tokens.len() {
        return;
    }

    let first = &tokens[cmd_start];

    // Sudo wraps another command — recurse into the inner command.
    if first == "sudo" || first == "doas" {
        let inner_tokens: Vec<&str> = tokens[cmd_start + 1..].iter().map(|s| s.as_str()).collect();
        if inner_tokens.is_empty() {
            violations.push(CommandViolation::UnknownTool);
            return;
        }
        let inner_cmd = inner_tokens.join(" ");
        check_segment(&inner_cmd, violations);
        return;
    }

    // Env command — skip to the actual command.
    if first == "env" {
        let remaining: Vec<&str> = tokens[cmd_start + 1..].iter().map(|s| s.as_str()).collect();
        if remaining.is_empty() {
            return;
        }
        let inner_cmd = remaining.join(" ");
        check_segment(&inner_cmd, violations);
        return;
    }

    // No-op / builtins
    if first == "true" || first == "false" || first == ":" || first == "noop" {
        return;
    }

    // Check if the command is in the allowlist.
    if !ALLOWED_COMMANDS.contains(&first.as_str()) {
        violations.push(CommandViolation::UnknownTool);
        return;
    }

    // Command-specific checks.
    match first.as_str() {
        "git" => check_git(&tokens[cmd_start..], violations),
        "curl" => check_curl(&tokens[cmd_start..], violations),
        "wget" => check_wget(&tokens[cmd_start..], violations),
        "cargo" => check_cargo(&tokens[cmd_start..], violations),
        "psql" | "mysql" | "sqlite3" => check_db_cli(&tokens[cmd_start..], violations),
        "pip" | "pip3" => check_pip(&tokens[cmd_start..], violations),
        "npm" | "pnpm" | "yarn" => check_npm(&tokens[cmd_start..], violations),
        "xargs" => check_xargs(&tokens[cmd_start..], violations),
        "sed" => check_sed(&tokens[cmd_start..], violations),
        "find" => check_find(&tokens[cmd_start..], violations),
        "tree" => {} // always read-only
        "redis-cli" => check_redis_cli(&tokens[cmd_start..], violations),
        _ => {
            // Allowed command with no special checks needed.
        }
    }
}

/// Validate `git` sub-commands.
fn check_git(tokens: &[String], violations: &mut Vec<CommandViolation>) {
    if tokens.len() < 2 {
        // bare `git` is harmless
        return;
    }

    let subcmd = &tokens[1];

    // Check explicitly denied sub-commands.
    if DENIED_GIT_SUBCOMMANDS.contains(&subcmd.as_str()) {
        violations.push(CommandViolation::VcsMutation);
        return;
    }

    // `git stash` sub-sub-commands
    if subcmd == "stash" {
        if tokens.len() >= 3 {
            let stash_action = &tokens[2];
            match stash_action.as_str() {
                "list" | "show" | "diff" => {} // read-only
                "push" | "pop" | "apply" | "drop" | "clear" | "branch" | "create" => {
                    violations.push(CommandViolation::VcsMutation);
                }
                _ => {
                    violations.push(CommandViolation::VcsMutation);
                }
            }
        }
        return;
    }

    // `git branch` — allow listing, reject -d/-D/delete
    if subcmd == "branch" {
        for tok in &tokens[2..] {
            if tok == "-d" || tok == "-D" || tok == "--delete" {
                violations.push(CommandViolation::VcsMutation);
                return;
            }
        }
        return;
    }

    // `git remote` — allow listing, reject add/remove/rename
    if subcmd == "remote" {
        if tokens.len() >= 3 {
            match tokens[2].as_str() {
                "add" | "remove" | "rm" | "rename" | "set-url" | "set-branches" => {
                    violations.push(CommandViolation::VcsMutation);
                }
                _ => {}
            }
        }
        return;
    }

    // `git config` — allow reading, reject writing
    if subcmd == "config" {
        // If no flags, it's listing. If --get/--list/--get-regexp, it's reading.
        // If --set/--unset/--replace-all/--add, it's writing.
        for tok in &tokens[2..] {
            match tok.as_str() {
                "--set" | "--unset" | "--replace-all" | "--add" | "--edit" | "--rename-section"
                | "--remove-section" => {
                    violations.push(CommandViolation::VcsMutation);
                    return;
                }
                _ => {}
            }
        }
        return;
    }

    // `git tag` — allow listing, reject -d/--delete/-a (creates annotated)
    if subcmd == "tag" {
        for tok in &tokens[2..] {
            if tok == "-d" || tok == "--delete" {
                violations.push(CommandViolation::VcsMutation);
                return;
            }
        }
        return;
    }

    // `git checkout` / `git switch` — reject (can modify working tree)
    if subcmd == "checkout" || subcmd == "switch" || subcmd == "restore" {
        violations.push(CommandViolation::VcsMutation);
        return;
    }

    // For other sub-commands, check if they're in the allowed list.
    if !ALLOWED_GIT_SUBCOMMANDS.contains(&subcmd.as_str()) {
        violations.push(CommandViolation::VcsMutation);
    }
}

/// Validate `curl` invocations — reject POST/PUT/PATCH/DELETE methods and data.
fn check_curl(tokens: &[String], violations: &mut Vec<CommandViolation>) {
    let mut i = 1;
    while i < tokens.len() {
        let tok = &tokens[i];
        match tok.as_str() {
            "-X" | "--request" => {
                if i + 1 < tokens.len() {
                    let method = tokens[i + 1].to_uppercase();
                    if method != "GET" && method != "HEAD" {
                        violations.push(CommandViolation::NetworkMutation);
                        return;
                    }
                    i += 2;
                    continue;
                }
            }
            "-d" | "--data" | "--data-raw" | "--data-binary" | "--data-urlencode" | "--form"
            | "-F" => {
                violations.push(CommandViolation::NetworkMutation);
                return;
            }
            "-T" | "--upload-file" => {
                violations.push(CommandViolation::NetworkMutation);
                return;
            }
            _ => {}
        }
        i += 1;
    }
}

/// Validate `wget` invocations — reject POST data.
fn check_wget(tokens: &[String], violations: &mut Vec<CommandViolation>) {
    for tok in &tokens[1..] {
        if tok.starts_with("--post-data") || tok.starts_with("--post-file") {
            violations.push(CommandViolation::NetworkMutation);
            return;
        }
        if tok == "--method=POST" || tok == "--method=PUT" || tok == "--method=PATCH" {
            violations.push(CommandViolation::NetworkMutation);
            return;
        }
    }
}

/// Validate `cargo` sub-commands.
fn check_cargo(tokens: &[String], violations: &mut Vec<CommandViolation>) {
    if tokens.len() < 2 {
        return;
    }

    let subcmd = &tokens[1];

    // Expand cargo aliases (e.g. `cargo c` = `cargo check`).
    let expanded = match subcmd.as_str() {
        "c" => "check",
        "b" => "build",
        "t" => "test",
        "r" => "run",
        other => other,
    };

    if DENIED_CARGO_SUBCOMMANDS.contains(&expanded) {
        violations.push(CommandViolation::PackageInstall);
        return;
    }

    // `cargo run` can execute arbitrary code
    if expanded == "run" {
        violations.push(CommandViolation::UnknownTool);
        return;
    }

    // For other sub-commands, check if they're in the allowed list.
    if !ALLOWED_CARGO_SUBCOMMANDS.contains(&expanded) && expanded != subcmd.as_str() {
        // Alias that doesn't expand to an allowed command
        violations.push(CommandViolation::PackageInstall);
    }
}

/// Validate database CLI clients — reject DML/DDL.
fn check_db_cli(tokens: &[String], violations: &mut Vec<CommandViolation>) {
    // Look for -c/--command flags with SQL content
    let mut i = 1;
    while i < tokens.len() {
        let tok = &tokens[i];
        if (tok == "-c" || tok == "--command") && i + 1 < tokens.len() {
            let sql = tokens[i + 1].to_lowercase();
            for keyword in SQL_MUTATION_KEYWORDS {
                if sql.starts_with(keyword) || sql.contains(&format!(" {keyword} ")) {
                    violations.push(CommandViolation::DatabaseMutation);
                    return;
                }
            }
            i += 2;
            continue;
        }
        // Inline SQL without -c (e.g. `sqlite3 db.db "INSERT..."`)
        if !tok.starts_with('-') {
            let lower = tok.to_lowercase();
            for keyword in SQL_MUTATION_KEYWORDS {
                if lower.starts_with(keyword) || lower.contains(&format!(" {keyword} ")) {
                    violations.push(CommandViolation::DatabaseMutation);
                    return;
                }
            }
        }
        i += 1;
    }
}

/// Validate `pip`/`pip3` — only allow read-only sub-commands.
fn check_pip(tokens: &[String], violations: &mut Vec<CommandViolation>) {
    if tokens.len() < 2 {
        return;
    }
    match tokens[1].as_str() {
        "install" | "uninstall" | "download" => {
            violations.push(CommandViolation::PackageInstall);
        }
        _ => {} // list, show, freeze, etc. are read-only
    }
}

/// Validate `npm`/`pnpm`/`yarn` — only allow read-only sub-commands.
fn check_npm(tokens: &[String], violations: &mut Vec<CommandViolation>) {
    if tokens.len() < 2 {
        return;
    }
    match tokens[1].as_str() {
        "install" | "add" | "remove" | "uninstall" | "update" | "link" | "unlink" | "publish"
        | "deploy" | "create" => {
            violations.push(CommandViolation::PackageInstall);
        }
        "run" => {
            // `npm run <script>` can execute arbitrary commands — reject.
            violations.push(CommandViolation::UnknownTool);
        }
        _ => {} // list, info, ls, outdated, etc. are read-only
    }
}

/// Validate `xargs` — ensure it doesn't chain to a mutation command.
fn check_xargs(tokens: &[String], violations: &mut Vec<CommandViolation>) {
    // xargs without a command defaults to `echo` (safe).
    // xargs <cmd> runs <cmd> for each input line — check <cmd>.
    if tokens.len() >= 2 {
        let first_arg = &tokens[1];
        if !first_arg.starts_with('-') {
            // The first non-flag arg is the command to run.
            // Build a synthetic command from remaining tokens and validate.
            let inner: Vec<&str> = tokens[1..].iter().map(|s| s.as_str()).collect();
            let inner_cmd = inner.join(" ");
            // Recursively validate the inner command.
            if let Err(inner_violations) = validate_read_only_command(&inner_cmd) {
                violations.extend(inner_violations);
            }
        }
    }
}

/// Validate `sed` — reject in-place editing (`-i`).
fn check_sed(tokens: &[String], violations: &mut Vec<CommandViolation>) {
    for tok in &tokens[1..] {
        if tok == "-i" || tok.starts_with("-i") {
            violations.push(CommandViolation::FileMutation);
            return;
        }
    }
}

/// Validate `find` — reject `-delete` and `-exec` with mutation commands.
fn check_find(tokens: &[String], violations: &mut Vec<CommandViolation>) {
    let mut i = 1;
    while i < tokens.len() {
        let tok = &tokens[i];
        if tok == "-delete" {
            violations.push(CommandViolation::FileMutation);
            return;
        }
        if tok == "-exec" || tok == "-execdir" {
            // Check the command after -exec
            if i + 1 < tokens.len() {
                let exec_cmd = &tokens[i + 1];
                if !ALLOWED_COMMANDS.contains(&exec_cmd.as_str()) {
                    violations.push(CommandViolation::UnknownTool);
                    return;
                }
            }
        }
        i += 1;
    }
}

/// Validate `redis-cli` — reject write commands.
fn check_redis_cli(tokens: &[String], violations: &mut Vec<CommandViolation>) {
    let mut i = 1;
    while i < tokens.len() {
        let tok = &tokens[i];
        if tok == "-c" && i + 1 < tokens.len() {
            let cmd = tokens[i + 1].to_lowercase();
            if is_redis_write_command(&cmd) {
                violations.push(CommandViolation::DatabaseMutation);
                return;
            }
            i += 2;
            continue;
        }
        // Direct command without -c
        if !tok.starts_with('-') {
            let lower = tok.to_lowercase();
            if is_redis_write_command(&lower) {
                violations.push(CommandViolation::DatabaseMutation);
                return;
            }
        }
        i += 1;
    }
}

fn is_redis_write_command(cmd: &str) -> bool {
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

/// Check for suspicious command chains.
fn check_chains(cmd: &str, violations: &mut Vec<CommandViolation>) {
    // Chains with `&&`, `||`, or `;` — split and validate each segment.
    // This is defense-in-depth: individual segments are already validated,
    // but chains can hide mutation in later segments if the split logic
    // misses something.
    //
    // We validate each chain segment independently.
    for separator in ["&&", "||", ";"] {
        if cmd.contains(separator) {
            let parts: Vec<&str> = cmd.split(separator).collect();
            for part in parts {
                let part = part.trim();
                if !part.is_empty()
                    && let Err(mut sub_violations) = validate_read_only_command(part)
                {
                    violations.append(&mut sub_violations);
                }
            }
        }
    }
}

// ── Production destructive-command classifier ────────────────────────────
//
// The classifier below is the **production** counterpart of the fail-closed
// `validate_read_only_command`.  Where the read-only validator is
// reject-unknown (anything not on an explicit allowlist is rejected), the
// production classifier is **allow-by-default**: ordinary build, read, and
// test commands pass through without restriction.  Only commands that match
// explicit destructive patterns produce a `HardDeny` or `SoftGate` outcome.

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
/// Unlike the fail-closed [`validate_read_only_command`] (which rejects
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
/// [`validate_read_only_command`], which is fail-closed (reject-unknown).
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
///   `.djinn/read-sources`, durable data files)
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
/// - `.djinn/read-sources/` (read-only sibling checkouts)
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
    // .djinn/read-sources (read-only sibling checkouts).
    if path.starts_with(".djinn/read-sources") || path.contains("/.djinn/read-sources") {
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

#[cfg(test)]
#[path = "command_validator_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "command_classifier_tests.rs"]
mod classifier_tests;
