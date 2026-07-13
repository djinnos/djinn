#![allow(dead_code)]
use super::super::fuzzy::{MatchMetadata, MatchOutcome, UnicodeSpliceStatus};
use ::djinn_telemetry::cargo_invocation::{
    KIND_BUILD, KIND_CHECK, KIND_CLIPPY, KIND_OTHER, KIND_TEST,
};
use std::borrow::Cow;

/// Denial message for `cargo check`/`cargo build` by worker/reviewer roles.
/// These cold-build the whole workspace (~12min); clippy reuses the warm cache.
pub(crate) const CARGO_CHECK_DENIED_MSG: &str = "cargo check / cargo build (for type-checking) is disabled for the worker and reviewer roles: it produces different artifacts than the warm cargo cache and cold-builds the workspace. Use `cargo clippy -p <crate>` instead (it reuses the warm cache and also lints). `cargo test`/`cargo nextest`, `cargo tree`, `cargo metadata`, and `cargo fmt` are allowed.";

/// Does this shell command, run by a code-executing role (worker or reviewer),
/// invoke a `cargo check` or `cargo build` whose only purpose is type-checking?
/// Returns the denial message when so. We allow clippy/test/nextest/tree/metadata/fmt
/// and all non-cargo commands.
///
/// Robust to: `bash -lc "cargo check"`, leading paths
/// (`/usr/local/bin/cargo build`), `cargo +nightly check`, and `&&`/`;`/`|`
/// command chains. Conservative: only the exact denied subcommands trip it, so a
/// `cargo clippy` containing the word "check" in a path is unaffected.
pub(crate) fn cargo_check_denied(command: &str) -> Option<&'static str> {
    for segment in command.split(['\n', ';', '&', '|']) {
        if segment_is_denied_cargo(segment) {
            return Some(CARGO_CHECK_DENIED_MSG);
        }
    }
    None
}

/// True when a single shell segment is a `cargo check`/`cargo build` invocation.
fn segment_is_denied_cargo(segment: &str) -> bool {
    let mut tokens = segment.split_whitespace().peekable();

    while let Some(&tok) = tokens.peek() {
        let bare = tok.trim_matches(['"', '\'']);
        let base = bare.rsplit('/').next().unwrap_or(bare);
        if base == "cargo" {
            break;
        }
        if bare.contains('=') && !bare.starts_with('-') {
            tokens.next();
            continue;
        }
        tokens.next();
    }

    match tokens.next() {
        Some(tok) => {
            let bare = tok.trim_matches(['"', '\'']);
            let base = bare.rsplit('/').next().unwrap_or(bare);
            if base != "cargo" {
                return false;
            }
        }
        None => return false,
    }

    if tokens.peek().is_some_and(|next| next.starts_with('+')) {
        tokens.next();
    }

    match tokens.next() {
        Some(sub) => {
            let sub = sub.trim_matches(['"', '\'']);
            sub == "check" || sub == "build"
        }
        None => false,
    }
}

/// Emit bounded-cardinality telemetry for an edit match outcome.
///
/// Emits `edit_match_outcome` for all outcomes, and additionally
/// `edit_match_strategy` for successful matches. Uses structured `tracing`
/// fields; never logs full file paths or file content.
///
/// Telemetry failures are swallowed — this must never make an edit call fail.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_edit_match_telemetry(
    metadata: &MatchMetadata,
    task_id: Option<&str>,
    session_id: Option<&str>,
    agent_role: Option<&str>,
    path_ext: &str,
    old_bytes: usize,
    new_bytes: usize,
    matched_bytes: Option<usize>,
) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let strategy = metadata.strategy.as_str();
        let outcome = match metadata.outcome {
            MatchOutcome::Success => "success",
            MatchOutcome::Ambiguous => "ambiguous",
            MatchOutcome::NoMatch => "no_match",
            MatchOutcome::GuardRejected => "guard_rejected",
        };
        let guard = metadata.guard_rejected_reason.unwrap_or("");
        let score = metadata.nearest_miss;
        let unicode_splice = metadata.unicode_splice.map(|s| match s {
            UnicodeSpliceStatus::Clean => "clean",
            UnicodeSpliceStatus::Adjusted => "adjusted",
        });

        tracing::info!(
            event_name = "edit_match_outcome",
            task_id = task_id.unwrap_or(""),
            session_id = session_id.unwrap_or(""),
            agent_role = agent_role.unwrap_or(""),
            tool_name = "edit",
            path_ext,
            strategy,
            outcome,
            guard,
            candidate_count = metadata.candidate_count,
            score,
            old_bytes,
            new_bytes,
            matched_bytes,
            reindented = metadata.reindented,
            unicode_splice,
            "edit match outcome telemetry"
        );

        if metadata.outcome == MatchOutcome::Success {
            tracing::info!(
                event_name = "edit_match_strategy",
                task_id = task_id.unwrap_or(""),
                session_id = session_id.unwrap_or(""),
                agent_role = agent_role.unwrap_or(""),
                tool_name = "edit",
                path_ext,
                strategy,
                outcome,
                guard,
                candidate_count = metadata.candidate_count,
                score,
                old_bytes,
                new_bytes,
                matched_bytes,
                reindented = metadata.reindented,
                unicode_splice,
                "edit match strategy success telemetry"
            );
        }
    }));
}

// ---------------------------------------------------------------------------
// Bounded cargo command classifier for shell telemetry.
// ---------------------------------------------------------------------------

const MAX_SEGMENTS: usize = 64;
const MAX_TOKENS: usize = 256;
const MAX_WRAPPER_DEPTH: u8 = 2;

/// Internal representation of a cargo telemetry kind, ordered by the required
/// aggregation precedence: check > clippy > test > build > other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Other,
    Build,
    Test,
    Clippy,
    Check,
}

impl Kind {
    const fn telemetry(self) -> &'static str {
        match self {
            Kind::Other => KIND_OTHER,
            Kind::Build => KIND_BUILD,
            Kind::Test => KIND_TEST,
            Kind::Clippy => KIND_CLIPPY,
            Kind::Check => KIND_CHECK,
        }
    }

    const fn rank(self) -> u8 {
        match self {
            Kind::Other => 0,
            Kind::Build => 1,
            Kind::Test => 2,
            Kind::Clippy => 3,
            Kind::Check => 4,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Malformed;

#[derive(Default)]
struct Budget {
    tokens: usize,
    segments: usize,
}

impl Budget {
    fn consume_tokens(&mut self, n: usize) -> Result<(), Malformed> {
        self.tokens += n;
        if self.tokens > MAX_TOKENS {
            return Err(Malformed);
        }
        Ok(())
    }

    fn consume_segments(&mut self, n: usize) -> Result<(), Malformed> {
        self.segments += n;
        if self.segments > MAX_SEGMENTS {
            return Err(Malformed);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Token<'a> {
    Word(Cow<'a, str>),
    Op,
}

/// Conservative, quote-aware classifier for cargo invocations in a single shell
/// command string. Returns the highest-precedence kind across all recognized
/// cargo invocations, or `None` when no cargo is found in executable position or
/// when the input exceeds the scanner's bounds or contains malformed quoting.
///
/// The returned string is one of the `djinn_telemetry::cargo_invocation`
/// constants (`KIND_CHECK`, `KIND_CLIPPY`, `KIND_TEST`, `KIND_BUILD`,
/// `KIND_OTHER`).
pub(crate) fn classify_cargo_command(command: &str) -> Option<&'static str> {
    let mut budget = Budget::default();
    classify_cargo_command_inner(command, 0, &mut budget)
        .ok()
        .flatten()
        .map(Kind::telemetry)
}

fn classify_cargo_command_inner(
    input: &str,
    depth: u8,
    budget: &mut Budget,
) -> Result<Option<Kind>, Malformed> {
    if depth > MAX_WRAPPER_DEPTH {
        return Err(Malformed);
    }

    let tokens = tokenize(input, budget)?;
    let mut best: Option<Kind> = None;
    let mut seg_start = 0;

    for (i, tok) in tokens.iter().enumerate() {
        if matches!(tok, Token::Op) {
            if i > seg_start {
                budget.consume_segments(1)?;
                let words: Vec<&str> = tokens[seg_start..i]
                    .iter()
                    .map(|t| match t {
                        Token::Word(w) => w.as_ref(),
                        Token::Op => unreachable!(),
                    })
                    .collect();
                if let Some(k) = classify_segment(&words, depth, budget)? {
                    best = Some(higher(best, k));
                }
            }
            seg_start = i + 1;
        }
    }

    if seg_start < tokens.len() {
        budget.consume_segments(1)?;
        let words: Vec<&str> = tokens[seg_start..]
            .iter()
            .map(|t| match t {
                Token::Word(w) => w.as_ref(),
                Token::Op => unreachable!(),
            })
            .collect();
        if let Some(k) = classify_segment(&words, depth, budget)? {
            best = Some(higher(best, k));
        }
    }

    Ok(best)
}

fn higher(a: Option<Kind>, b: Kind) -> Kind {
    match a {
        Some(a) if a.rank() >= b.rank() => a,
        _ => b,
    }
}

fn classify_segment(
    words: &[&str],
    depth: u8,
    budget: &mut Budget,
) -> Result<Option<Kind>, Malformed> {
    let mut idx = 0;

    // Skip leading shell assignments in command position.
    while idx < words.len() && is_assignment(words[idx]) {
        idx += 1;
    }

    // Permit approved `env`/`command` prefixes, skipping their options and
    // assignment arguments.
    while idx < words.len() {
        let base = basename(words[idx]);
        if base == "env" || base == "command" {
            idx += 1;
            while idx < words.len() && (words[idx].starts_with('-') || is_assignment(words[idx])) {
                idx += 1;
            }
            continue;
        }
        break;
    }

    if idx >= words.len() {
        return Ok(None);
    }

    let exec = words[idx];
    let base = basename(exec);

    if matches!(base, "sh" | "bash" | "zsh") {
        let mut j = idx + 1;
        while j < words.len() {
            if words[j] == "-c" || words[j] == "-lc" {
                if j + 1 < words.len() {
                    return classify_cargo_command_inner(words[j + 1], depth + 1, budget);
                }
                return Ok(None);
            }
            if words[j].starts_with('-') {
                j += 1;
                continue;
            }
            break;
        }
        return Ok(None);
    }

    if base == "cargo" {
        return Ok(Some(cargo_kind(&words[idx + 1..])));
    }

    Ok(None)
}

fn cargo_kind(words: &[&str]) -> Kind {
    if words.is_empty() {
        return Kind::Other;
    }
    let mut idx = 0;
    if words[idx].starts_with('+') {
        idx += 1;
    }
    if idx >= words.len() {
        return Kind::Other;
    }
    match words[idx] {
        "check" => Kind::Check,
        "clippy" => Kind::Clippy,
        "test" => Kind::Test,
        "nextest" => Kind::Test,
        "build" => Kind::Build,
        _ => Kind::Other,
    }
}

fn basename(path: &str) -> &str {
    path.rsplit_once('/').map(|(_, b)| b).unwrap_or(path)
}

fn is_assignment(word: &str) -> bool {
    if word.starts_with('-') {
        return false;
    }
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    is_valid_shell_identifier(name)
}

fn is_valid_shell_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn tokenize<'a>(input: &'a str, budget: &mut Budget) -> Result<Vec<Token<'a>>, Malformed> {
    let mut tokens = Vec::new();
    let mut pos = 0;

    while pos < input.len() {
        let c = input[pos..].chars().next().unwrap();
        let len = c.len_utf8();

        if c == '\n' {
            tokens.push(Token::Op);
            pos += len;
        } else if c.is_ascii_whitespace() {
            pos += len;
        } else if c == '#' {
            // Comment to end of line; leave the newline to be tokenized as a
            // segment separator.
            if let Some(nl) = input[pos..].find('\n') {
                pos += nl;
            } else {
                pos = input.len();
            }
        } else if c == ';' || c == '&' || c == '|' {
            if (c == '&' || c == '|')
                && pos + 1 < input.len()
                && input.as_bytes()[pos + 1] == c as u8
            {
                pos += 2 * len;
            } else {
                pos += len;
            }
            tokens.push(Token::Op);
        } else {
            let word = parse_word(input, &mut pos)?;
            tokens.push(Token::Word(word));
        }

        if tokens.len() > MAX_TOKENS {
            return Err(Malformed);
        }
    }

    budget.consume_tokens(tokens.len())?;
    Ok(tokens)
}

fn parse_word<'a>(s: &'a str, pos: &mut usize) -> Result<Cow<'a, str>, Malformed> {
    let start = *pos;
    let mut owned: Option<String> = None;

    while let Some(c) = s[*pos..].chars().next() {
        if c.is_ascii_whitespace() || c == ';' || c == '&' || c == '|' {
            break;
        }
        let len = c.len_utf8();

        if c == '\\' {
            if owned.is_none() {
                owned = Some(s[start..*pos].to_string());
            }
            *pos += len;
            if let Some(c2) = s[*pos..].chars().next() {
                owned.as_mut().unwrap().push(c2);
                *pos += c2.len_utf8();
            } else {
                return Err(Malformed);
            }
        } else if c == '\'' || c == '"' {
            if owned.is_none() {
                owned = Some(s[start..*pos].to_string());
            }
            *pos += len;
            let content = parse_quoted(s, pos, c)?;
            owned.as_mut().unwrap().push_str(&content);
        } else {
            if let Some(ref mut o) = owned {
                o.push(c);
            }
            *pos += len;
        }
    }

    if let Some(o) = owned {
        Ok(Cow::Owned(o))
    } else {
        Ok(Cow::Borrowed(&s[start..*pos]))
    }
}

fn parse_quoted(s: &str, pos: &mut usize, quote: char) -> Result<String, Malformed> {
    let mut out = String::new();
    while let Some(c) = s[*pos..].chars().next() {
        let len = c.len_utf8();
        if c == quote {
            *pos += len;
            return Ok(out);
        }
        if quote == '"' && c == '\\' {
            *pos += len;
            if let Some(c2) = s[*pos..].chars().next() {
                out.push(c2);
                *pos += c2.len_utf8();
            } else {
                return Err(Malformed);
            }
        } else {
            out.push(c);
            *pos += len;
        }
    }
    Err(Malformed)
}

#[cfg(test)]
mod cargo_guard_tests {
    use super::cargo_check_denied;

    #[test]
    fn rejects_cargo_check_and_build() {
        assert!(cargo_check_denied("cargo check -p djinn-db").is_some());
        assert!(cargo_check_denied("cargo build -p x").is_some());
        // bash -lc wrapper.
        assert!(cargo_check_denied(r#"bash -lc "cargo check""#).is_some());
        assert!(cargo_check_denied(r#"bash -lc 'cargo check'"#).is_some());
        // leading path + toolchain selector.
        assert!(cargo_check_denied("/usr/local/bin/cargo check").is_some());
        assert!(cargo_check_denied("cargo +nightly check -p x").is_some());
        // env-assignment prefix.
        assert!(cargo_check_denied("FOO=bar cargo check").is_some());
        // chain — the denied segment trips it even after an allowed one.
        assert!(cargo_check_denied("cargo clippy -p x && cargo check -p x").is_some());
        assert!(cargo_check_denied("cd server; cargo build").is_some());
    }

    #[test]
    fn allows_clippy_test_and_inspection() {
        assert!(cargo_check_denied("cargo clippy -p x").is_none());
        assert!(cargo_check_denied("cargo clippy --all-targets -- -D warnings").is_none());
        assert!(cargo_check_denied("cargo nextest run -p x").is_none());
        assert!(cargo_check_denied("cargo test -p x").is_none());
        assert!(cargo_check_denied("cargo tree -p x").is_none());
        assert!(cargo_check_denied("cargo metadata --format-version 1").is_none());
        assert!(cargo_check_denied("cargo fmt").is_none());
        assert!(cargo_check_denied("git diff").is_none());
        assert!(cargo_check_denied("ls -la").is_none());
        // A non-cargo binary that happens to be named with "check" is fine.
        assert!(cargo_check_denied("./scripts/check-file-size.sh").is_none());
        // clippy chained with an allowed cargo command stays allowed.
        assert!(cargo_check_denied("cargo clippy -p x && cargo test -p x").is_none());
    }
}

#[cfg(test)]
mod cargo_classify_tests {
    use super::classify_cargo_command;
    use ::djinn_telemetry::cargo_invocation::{
        KIND_BUILD, KIND_CHECK, KIND_CLIPPY, KIND_OTHER, KIND_TEST,
    };

    fn assert_kind(command: &str, expected: Option<&'static str>) {
        assert_eq!(
            classify_cargo_command(command),
            expected,
            "command: {command:?}"
        );
    }

    #[test]
    fn maps_each_cargo_subcommand() {
        assert_kind("cargo check", Some(KIND_CHECK));
        assert_kind("cargo clippy", Some(KIND_CLIPPY));
        assert_kind("cargo test", Some(KIND_TEST));
        assert_kind("cargo nextest", Some(KIND_TEST));
        assert_kind("cargo nextest run", Some(KIND_TEST));
        assert_kind("cargo build", Some(KIND_BUILD));
        assert_kind("cargo metadata", Some(KIND_OTHER));
        assert_kind("cargo fmt", Some(KIND_OTHER));
        assert_kind("cargo", Some(KIND_OTHER));
        assert_kind("cargo --verbose check", Some(KIND_OTHER));
    }

    #[test]
    fn recognizes_cargo_prefixes() {
        assert_kind("/usr/local/bin/cargo check", Some(KIND_CHECK));
        assert_kind("./cargo build", Some(KIND_BUILD));
        assert_kind("cargo +nightly check", Some(KIND_CHECK));
        assert_kind("cargo +nightly clippy", Some(KIND_CLIPPY));
        assert_kind("FOO=bar cargo check", Some(KIND_CHECK));
        assert_kind("RUSTFLAGS='-D warnings' cargo build", Some(KIND_BUILD));
        assert_kind("env cargo check", Some(KIND_CHECK));
        assert_kind("env RUSTFLAGS='-D warnings' cargo check", Some(KIND_CHECK));
        assert_kind("command cargo check", Some(KIND_CHECK));
    }

    #[test]
    fn recognizes_wrapper_payloads() {
        assert_kind(r#"bash -lc "cargo check""#, Some(KIND_CHECK));
        assert_kind(r#"bash -lc 'cargo test'"#, Some(KIND_TEST));
        assert_kind(r#"sh -c "cargo build""#, Some(KIND_BUILD));
        assert_kind(r#"zsh -c "cargo clippy""#, Some(KIND_CLIPPY));
        assert_kind(r#"/bin/bash -lc "cargo nextest run""#, Some(KIND_TEST));
        assert_kind(r#"bash -e -lc "cargo check""#, Some(KIND_CHECK));
    }

    #[test]
    fn aggregates_with_required_precedence() {
        // check beats everything, even when it appears later.
        assert_kind("cargo clippy && cargo check", Some(KIND_CHECK));
        assert_kind("cargo test && cargo check", Some(KIND_CHECK));
        assert_kind("cargo build && cargo check", Some(KIND_CHECK));
        assert_kind("cargo metadata && cargo check", Some(KIND_CHECK));

        // clippy beats test/build/other.
        assert_kind("cargo test && cargo clippy", Some(KIND_CLIPPY));
        assert_kind("cargo build && cargo clippy", Some(KIND_CLIPPY));
        assert_kind("cargo fmt && cargo clippy", Some(KIND_CLIPPY));

        // test beats build/other.
        assert_kind("cargo build && cargo test", Some(KIND_TEST));
        assert_kind("cargo metadata && cargo test", Some(KIND_TEST));

        // build beats other.
        assert_kind("cargo fmt && cargo build", Some(KIND_BUILD));
    }

    #[test]
    fn recognizes_chains_and_newlines() {
        assert_kind("cd server && cargo clippy -p x", Some(KIND_CLIPPY));
        assert_kind("cargo fmt && cargo test", Some(KIND_TEST));
        assert_kind("cargo build && cargo clippy", Some(KIND_CLIPPY));
        assert_kind("cd server; cargo check", Some(KIND_CHECK));
        assert_kind("cargo build\ncargo test\ncargo check", Some(KIND_CHECK));
    }

    #[test]
    fn rejects_argument_position_and_quoted_prose() {
        assert_kind("echo cargo test", None);
        assert_kind("printf 'cargo test'", None);
        assert_kind(r#""cargo check""#, None);
        assert_kind(r#""cargo" check""#, None); // quoted executable, not a bare cargo invocation
        assert_kind(r#"bash -lc "echo cargo check""#, None);
    }

    #[test]
    fn rejects_comments_and_non_cargo_tools() {
        assert_kind("# cargo check", None);
        assert_kind("echo cargo check # cargo check", None);
        assert_kind("nextest run", None);
        assert_kind("go test ./...", None);
        assert_kind("pnpm test", None);
        assert_kind("npm run build", None);
        assert_kind("make build", None);
        assert_kind("bazel build", None);
        assert_kind("git diff", None);
        assert_kind("ls -la", None);
        assert_kind("./scripts/cargo-test.sh", None);
    }

    #[test]
    fn rejects_malformed_quotes_and_escapes() {
        assert_kind(r#"cargo check ""#, None);
        assert_kind("cargo check '", None);
        assert_kind(r#"bash -lc "cargo check"#, None); // unclosed wrapper payload quote
        assert_kind("cargo check \\", None);
    }

    #[test]
    fn rejects_segment_overflow() {
        let ok: String = std::iter::repeat("cargo check")
            .take(64)
            .collect::<Vec<_>>()
            .join(" && ");
        assert_eq!(classify_cargo_command(&ok), Some(KIND_CHECK));

        let over: String = std::iter::repeat("cargo check")
            .take(65)
            .collect::<Vec<_>>()
            .join(" && ");
        assert_eq!(classify_cargo_command(&over), None);
    }

    #[test]
    fn rejects_token_overflow() {
        let mut cmd = "cargo check".to_string();
        for i in 0..255 {
            cmd.push(' ');
            cmd.push_str(&format!("a{i}"));
        }
        // 2 + 255 = 257 tokens.
        assert_eq!(classify_cargo_command(&cmd), None);
    }

    #[test]
    fn rejects_wrapper_recursion_overflow() {
        // Depth 2 is allowed.
        assert_kind(r#"bash -c "bash -c 'cargo check'""#, Some(KIND_CHECK));
        // Depth 3 is not.
        assert_kind(r#"bash -c "bash -c \"bash -c 'cargo check'\"""#, None);
    }

    #[test]
    fn does_not_use_substring_fallback() {
        // The `cargo check` is recognized, but the malformed quote fails closed
        // for the whole command rather than returning KIND_CHECK.
        assert_kind(r#"cargo check && cargo clippy ""#, None);
    }
}
