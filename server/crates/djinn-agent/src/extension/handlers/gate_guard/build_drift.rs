//! Build-drift soft gate (ri23 Part 1).
//!
//! This is an **advisory** extension to the worker shell soft-gate. It never
//! intercepts, rewrites, or blocks a build outright; its only power is to deny
//! the *first* distinct ad-hoc compile-producing shell command per
//! `(session, project)` once, with a message steering the worker toward the
//! `run_verification` tool. Cache integrity is already structural — only
//! `run_verification` records a canonical pass — so this gate exists purely to
//! reduce wasted ad-hoc compiles that diverge from the resolved canonical
//! verification plan.
//!
//! # Decision model
//!
//! For a shell command that the destructive classifier already deemed `Allow`,
//! and only when a canonical plan is configured (non-empty `command_groups`),
//! the gate:
//!
//! 1. Parses the command. Anything that is not a **simple** command — env
//!    assignment prefix, redirection, pipeline, chain/compound, subshell,
//!    substitution, background op, parse error, absolute-path executable, or a
//!    nested interpreter (`bash -lc …`) — fails **open** (`Ineligible`) with an
//!    enumerated reason. No behavior change for those shapes.
//! 2. Compares the observed executable **basename** + argv against the
//!    flattened canonical commands:
//!    - exact argv-array equality with a same-basename canonical command →
//!      [`BuildDriftClassification::ArgvEqual`] (pass);
//!    - basename matches a canonical command but argv differs →
//!      [`BuildDriftClassification::Drift`] (steer once);
//!    - basename unrelated to every canonical command →
//!      [`BuildDriftClassification::Unrelated`] (pass).
//!
//! The `run_verification` tool is a distinct MCP tool call and is never routed
//! through the shell handler, so the gate structurally never matches it. The
//! closest shell-observable analog — a worker manually running the *exact*
//! canonical command — is an `ArgvEqual` pass, never a deny.
//!
//! # Drift key
//!
//! The per-`(session, project)` deny-once bookkeeping reuses the existing
//! [`crate::file_time::FileTime`] `bash_soft_forced` set. Each drift key is a
//! [`DestructiveClass`] whose label is a domain-separated SHA-256 hash of the
//! project id, the executable basename, and the length-prefixed argv tokens.
//! Hashing (rather than the raw command) bounds the size of the leaked
//! `'static` label interned by [`DestructiveClass::from_owned`]. The session
//! dimension is supplied by the map key (the live session id); the project
//! dimension is folded into the hash.

use crate::file_time::DestructiveClass;

/// Domain-separation prefix for the drift-key hash. Bumping the version
/// invalidates all previously computed keys.
const DRIFT_KEY_DOMAIN: &[u8] = b"djinn.gate_guard.build_drift.v1";

/// Namespace prefix on the interned [`DestructiveClass`] label so drift keys
/// never collide with the `destructive.*` soft-gate classes that share the
/// `bash_soft_forced` set.
const DRIFT_KEY_LABEL_PREFIX: &str = "build_drift.";

/// Advisory message shown on the first drift deny per `(session, project)`.
///
/// It is deliberately non-blocking in spirit: a retry of the *same* command
/// passes (the worker keeps agency), but the message points at the canonical
/// path first.
pub const BUILD_DRIFT_STEER_MESSAGE: &str = "\
GateGuard (advisory): this looks like an ad-hoc compile-producing build that \
diverges from the project's canonical verification plan. Prefer the \
`run_verification` tool, which runs the resolved canonical command groups and \
records a canonical pass (ad-hoc builds do not). If you specifically need this \
exact command, re-run it and it will proceed.";

/// One canonical command drawn from the resolved
/// `lifecycle.final_verification.command_groups`, reduced to the two fields the
/// gate compares against: the executable **basename** and its argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalCommand {
    /// Basename of the canonical `executable` (path components stripped).
    pub executable_basename: String,
    /// Canonical argv (arguments after the executable).
    pub argv: Vec<String>,
}

impl CanonicalCommand {
    /// Build a [`CanonicalCommand`] from a raw executable string (which may be
    /// a bare name or a path) and its argv.
    pub fn new(executable: &str, argv: Vec<String>) -> Self {
        Self {
            executable_basename: basename(executable).to_string(),
            argv,
        }
    }
}

/// Enumerated, bounded reasons a command is ineligible for build-drift
/// comparison. Each maps to a fixed telemetry label; there are no dynamic
/// values, keeping metric cardinality bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildDriftIneligibleReason {
    /// Unbalanced quotes, empty, or otherwise unparseable.
    ParseError,
    /// Leading `NAME=value` environment-assignment prefix.
    EnvAssignment,
    /// Input/output redirection (`>`, `>>`, `<`, `2>`, …).
    Redirection,
    /// Pipeline (`|`).
    Pipeline,
    /// Command chain / compound (`&&`, `||`, `;`, newline).
    Chain,
    /// Subshell / grouping (`(` … `)`).
    Subshell,
    /// Command or variable substitution (`` ` ``, `$(`, `${`, `$VAR`).
    Substitution,
    /// Background operator (trailing `&`).
    Background,
    /// Nested interpreter / wrapper (`bash`, `sh`, `env`, `sudo`, `xargs`, …).
    NestedInterpreter,
    /// Executable given as an absolute path.
    AbsolutePath,
}

impl BuildDriftIneligibleReason {
    /// Stable, bounded telemetry label for this reason.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ParseError => "parse_error",
            Self::EnvAssignment => "env_assignment",
            Self::Redirection => "redirection",
            Self::Pipeline => "pipeline",
            Self::Chain => "chain",
            Self::Subshell => "subshell",
            Self::Substitution => "substitution",
            Self::Background => "background",
            Self::NestedInterpreter => "nested_interpreter",
            Self::AbsolutePath => "absolute_path",
        }
    }
}

/// Pure classification of one shell command against a canonical plan.
///
/// Independent of session state and telemetry; the async gate wrapper layers
/// deny-once bookkeeping and metric emission on top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildDriftClassification {
    /// No canonical plan configured — the gate is inert (no telemetry).
    Inert,
    /// Command was not a simple command — fail open with an enumerated reason.
    Ineligible(BuildDriftIneligibleReason),
    /// Eligible, exact argv match with a same-basename canonical command.
    ArgvEqual,
    /// Eligible, basename unrelated to every canonical command.
    Unrelated,
    /// Eligible, basename matches a canonical command but argv differs.
    Drift {
        /// Deny-once key for the `bash_soft_forced` set.
        key: DestructiveClass,
    },
}

impl BuildDriftClassification {
    /// Stable outcome label for eligible/ineligible classifications that map to
    /// a pass. `Drift` is intentionally excluded — its terminal outcome
    /// (`drift_deny` vs `repeat_pass`) is decided by session state, not the
    /// pure classification.
    #[cfg(any(test, feature = "test-support"))]
    pub fn describe(&self) -> String {
        match self {
            Self::Inert => "inert".to_string(),
            Self::Ineligible(reason) => format!("ineligible:{}", reason.as_str()),
            Self::ArgvEqual => "argv_equal".to_string(),
            Self::Unrelated => "unrelated".to_string(),
            Self::Drift { .. } => "drift".to_string(),
        }
    }
}

/// Classify one shell command against the resolved canonical plan.
///
/// `project_id` is folded into the drift-key hash so the deny-once scope is
/// per-`(session, project)` (the session dimension is the caller's map key).
pub fn classify_build_drift(
    command: &str,
    project_id: Option<&str>,
    canonical: &[CanonicalCommand],
) -> BuildDriftClassification {
    // Inert when there is no canonical plan to compare against.
    if canonical.is_empty() {
        return BuildDriftClassification::Inert;
    }

    let simple = match parse_simple_command(command) {
        Ok(simple) => simple,
        Err(reason) => return BuildDriftClassification::Ineligible(reason),
    };

    let observed_basename = simple.executable_basename.as_str();

    // Does any canonical command share this basename?
    let mut basename_matched = false;
    for canon in canonical {
        if canon.executable_basename == observed_basename {
            basename_matched = true;
            // Exact argv equality with a same-basename canonical command is a
            // pass — the worker ran the real thing.
            if canon.argv == simple.argv {
                return BuildDriftClassification::ArgvEqual;
            }
        }
    }

    if !basename_matched {
        return BuildDriftClassification::Unrelated;
    }

    // Same-basename canonical command exists but no argv matched → drift.
    let key = drift_key(project_id, observed_basename, &simple.argv);
    BuildDriftClassification::Drift { key }
}

/// A successfully parsed simple command.
struct SimpleCommand {
    executable_basename: String,
    argv: Vec<String>,
}

/// Parse `command` into a [`SimpleCommand`], or return the enumerated reason it
/// is ineligible. This is intentionally conservative: any shape that could
/// change the effective executable/argv at runtime fails open.
fn parse_simple_command(command: &str) -> Result<SimpleCommand, BuildDriftIneligibleReason> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err(BuildDriftIneligibleReason::ParseError);
    }

    // First pass: scan for control operators / substitution / redirection,
    // quote-aware, returning the first offending shape encountered.
    scan_for_control_operators(trimmed)?;

    // Second pass: tokenize (quote-aware). Malformed quoting → ParseError.
    let tokens = tokenize(trimmed).ok_or(BuildDriftIneligibleReason::ParseError)?;
    if tokens.is_empty() {
        return Err(BuildDriftIneligibleReason::ParseError);
    }

    // Leading env assignment (`NAME=value`) prefix.
    if is_env_assignment(&tokens[0]) {
        return Err(BuildDriftIneligibleReason::EnvAssignment);
    }

    let executable = tokens[0].as_str();

    // Absolute-path executable — cannot reliably compare; fail open.
    if executable.starts_with('/') {
        return Err(BuildDriftIneligibleReason::AbsolutePath);
    }

    let exe_basename = basename(executable);

    // Nested interpreter / wrapper — the effective command is nested; fail open.
    if is_interpreter_or_wrapper(exe_basename) {
        return Err(BuildDriftIneligibleReason::NestedInterpreter);
    }

    Ok(SimpleCommand {
        executable_basename: exe_basename.to_string(),
        argv: tokens[1..].to_vec(),
    })
}

/// Quote-aware scan for the first control operator / substitution /
/// redirection. Returns the matching ineligible reason, or `Ok(())` when the
/// command contains none of them.
fn scan_for_control_operators(cmd: &str) -> Result<(), BuildDriftIneligibleReason> {
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            _ if in_single || in_double => {}
            '|' => {
                // `||` is a chain; single `|` is a pipeline.
                if i + 1 < chars.len() && chars[i + 1] == '|' {
                    return Err(BuildDriftIneligibleReason::Chain);
                }
                return Err(BuildDriftIneligibleReason::Pipeline);
            }
            '&' => {
                // `&&` is a chain; single `&` is a background op.
                if i + 1 < chars.len() && chars[i + 1] == '&' {
                    return Err(BuildDriftIneligibleReason::Chain);
                }
                return Err(BuildDriftIneligibleReason::Background);
            }
            ';' | '\n' => return Err(BuildDriftIneligibleReason::Chain),
            '(' | ')' => return Err(BuildDriftIneligibleReason::Subshell),
            '`' => return Err(BuildDriftIneligibleReason::Substitution),
            '$' => return Err(BuildDriftIneligibleReason::Substitution),
            '<' | '>' => return Err(BuildDriftIneligibleReason::Redirection),
            _ => {}
        }
        i += 1;
    }

    if in_single || in_double {
        return Err(BuildDriftIneligibleReason::ParseError);
    }
    Ok(())
}

/// Minimal quote-aware tokenizer. Returns `None` on unbalanced quotes.
///
/// This handles single and double quotes and simple backslash escaping; the
/// control-operator scan has already rejected anything with shell
/// metacharacters, so the token stream here is a plain executable + argv.
fn tokenize(cmd: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = cmd.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                has_token = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                has_token = true;
            }
            '\\' if !in_single => {
                // Trailing backslash (no following char) is malformed → None.
                let next = chars.next()?;
                current.push(next);
                has_token = true;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            c => {
                current.push(c);
                has_token = true;
            }
        }
    }

    if in_single || in_double {
        return None;
    }
    if has_token {
        tokens.push(current);
    }
    Some(tokens)
}

/// Return `true` if `token` is a leading `NAME=value` environment assignment.
fn is_env_assignment(token: &str) -> bool {
    let Some(eq) = token.find('=') else {
        return false;
    };
    if eq == 0 {
        return false;
    }
    let name = &token[..eq];
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Strip path components, returning the basename of `token`.
fn basename(token: &str) -> &str {
    token.rsplit(['/', '\\']).next().unwrap_or(token)
}

/// Return `true` if `basename` names a shell interpreter or command wrapper
/// whose real command is nested inside its arguments.
fn is_interpreter_or_wrapper(basename: &str) -> bool {
    matches!(
        basename,
        "sh" | "bash"
            | "zsh"
            | "dash"
            | "ksh"
            | "fish"
            | "csh"
            | "tcsh"
            | "env"
            | "sudo"
            | "doas"
            | "xargs"
            | "timeout"
            | "nohup"
            | "nice"
            | "ionice"
            | "stdbuf"
            | "time"
            | "watch"
            | "eval"
            | "exec"
            | "command"
    )
}

/// Compute the domain-separated drift key for a `(project, basename, argv)`
/// triple. Argv tokens are length-prefixed so distinct argv vectors cannot
/// collide via concatenation.
fn drift_key(project_id: Option<&str>, basename: &str, argv: &[String]) -> DestructiveClass {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(DRIFT_KEY_DOMAIN);
    hasher.update([0u8]);
    hasher.update(project_id.unwrap_or("").as_bytes());
    hasher.update([0u8]);
    hasher.update(basename.as_bytes());
    hasher.update([0u8]);
    for arg in argv {
        hasher.update((arg.len() as u64).to_le_bytes());
        hasher.update(arg.as_bytes());
    }
    let digest = hex::encode(hasher.finalize());
    DestructiveClass::from_owned(format!("{DRIFT_KEY_LABEL_PREFIX}{digest}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(exe: &str, argv: &[&str]) -> CanonicalCommand {
        CanonicalCommand::new(exe, argv.iter().map(|s| s.to_string()).collect())
    }

    fn plan() -> Vec<CanonicalCommand> {
        vec![
            canon(
                "cargo",
                &["clippy", "--all-targets", "--", "-D", "warnings"],
            ),
            canon("cargo", &["nextest", "run", "--workspace"]),
            canon("pnpm", &["-C", "web", "test"]),
        ]
    }

    #[test]
    fn inert_when_no_plan() {
        assert_eq!(
            classify_build_drift("cargo build -p foo", Some("p1"), &[]),
            BuildDriftClassification::Inert
        );
    }

    #[test]
    fn argv_equal_is_pass() {
        let c = classify_build_drift(
            "cargo clippy --all-targets -- -D warnings",
            Some("p1"),
            &plan(),
        );
        assert_eq!(c, BuildDriftClassification::ArgvEqual);
    }

    #[test]
    fn same_basename_diverging_argv_is_drift() {
        let c = classify_build_drift("cargo build -p foo", Some("p1"), &plan());
        assert!(matches!(c, BuildDriftClassification::Drift { .. }));
    }

    #[test]
    fn unrelated_basename_is_pass() {
        assert_eq!(
            classify_build_drift("go build ./...", Some("p1"), &plan()),
            BuildDriftClassification::Unrelated
        );
    }

    #[test]
    fn ineligible_shapes_fail_open() {
        use BuildDriftIneligibleReason::*;
        let cases: &[(&str, BuildDriftIneligibleReason)] = &[
            ("FOO=bar cargo build", EnvAssignment),
            ("cargo build | tee log", Pipeline),
            ("cargo build && cargo test", Chain),
            ("cargo build || true", Chain),
            ("cargo build; echo done", Chain),
            ("cargo build > out.txt", Redirection),
            ("cargo build 2> err.txt", Redirection),
            ("cargo build <in.txt", Redirection),
            ("(cargo build)", Subshell),
            ("echo $(cargo build)", Substitution),
            ("cargo build $FLAGS", Substitution),
            ("echo `cargo build`", Substitution),
            ("cargo build &", Background),
            ("bash -lc \"cargo build\"", NestedInterpreter),
            ("sh -c 'cargo build'", NestedInterpreter),
            ("env CARGO=x cargo build", NestedInterpreter),
            ("sudo cargo build", NestedInterpreter),
            ("xargs cargo build", NestedInterpreter),
            ("timeout 60 cargo build", NestedInterpreter),
            ("/usr/local/bin/cargo build", AbsolutePath),
            ("cargo \"build", ParseError),
            ("", ParseError),
            ("   ", ParseError),
        ];
        for (cmd, want) in cases {
            assert_eq!(
                classify_build_drift(cmd, Some("p1"), &plan()),
                BuildDriftClassification::Ineligible(*want),
                "command: {cmd:?}"
            );
        }
    }

    #[test]
    fn drift_key_is_stable_and_scoped() {
        let a = drift_key(
            Some("p1"),
            "cargo",
            &["build".into(), "-p".into(), "foo".into()],
        );
        let b = drift_key(
            Some("p1"),
            "cargo",
            &["build".into(), "-p".into(), "foo".into()],
        );
        assert_eq!(a, b, "identical inputs → identical key");

        let diff_project = drift_key(
            Some("p2"),
            "cargo",
            &["build".into(), "-p".into(), "foo".into()],
        );
        assert_ne!(a, diff_project, "project is part of the key");

        // Length-prefixing prevents concatenation collisions.
        let ab = drift_key(Some("p1"), "cargo", &["ab".into(), "c".into()]);
        let a_bc = drift_key(Some("p1"), "cargo", &["a".into(), "bc".into()]);
        assert_ne!(ab, a_bc);

        // Labels are namespaced.
        assert!(a.as_str().starts_with(DRIFT_KEY_LABEL_PREFIX));
    }

    #[test]
    fn absolute_path_matching_basename_still_fails_open() {
        // Even though the basename would match a canonical command, an
        // absolute-path executable is ineligible.
        assert_eq!(
            classify_build_drift("/usr/bin/cargo clippy", Some("p1"), &plan()),
            BuildDriftClassification::Ineligible(BuildDriftIneligibleReason::AbsolutePath)
        );
    }

    #[test]
    fn relative_path_executable_compares_by_basename() {
        // A relative path (not absolute) is eligible and compared by basename.
        let c = classify_build_drift("./cargo build -p foo", Some("p1"), &plan());
        assert!(matches!(c, BuildDriftClassification::Drift { .. }));
    }
}
