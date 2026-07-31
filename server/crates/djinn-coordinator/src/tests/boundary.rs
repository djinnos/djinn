// ─── Boundary checks: orchestration crate dependency invariants ──────────────
//
// These tests verify the architectural boundaries established by the Phase 5
// orchestration split.  The manifest guards read `Cargo.toml` at compile time
// via `include_str!` and assert that forbidden dependency edges are absent.
// The source guards walk a crate's `src/` tree and assert that no PRODUCTION
// file declares a `use` of a forbidden crate — the case a manifest guard
// cannot see, because a dev-dependency is legitimately importable from
// `#[cfg(test)]` code in the very same file.
//
// The source-scanning primitives live in the "Shared source-guard primitives"
// section at the bottom of this file and are reused by
// `raw_signal_bypass_guard.rs`.

use std::path::{Path, PathBuf};

/// `djinn-slot` must NOT depend on `djinn-coordinator` or `djinn-agent`.
///
/// The slot crate owns pool lifecycle, reply loop, session extraction, and
/// related helpers.  It must be usable by both the coordinator and the agent
/// facade without pulling in either's internals.
#[test]
fn boundary_djinn_slot_has_no_coordinator_or_agent_dependency() {
    let cargo_toml = include_str!("../../../djinn-slot/Cargo.toml");
    for forbidden in &["djinn-coordinator", "djinn-agent"] {
        let dep_pattern = format!("{forbidden} =");
        let has_dep = cargo_toml.lines().any(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with('#') && trimmed.starts_with(&dep_pattern)
        });
        assert!(
            !has_dep,
            "djinn-slot Cargo.toml must not contain a dependency on {forbidden}; \
             found a dependency declaration line in: djinn-slot/Cargo.toml"
        );
    }
}

/// `djinn-coordinator` production code must NOT depend on `djinn-agent`.
///
/// The coordinator crate owns dispatch, doctor, PR polling, and supervisor
/// disposition logic.  It depends on `djinn-slot` and `djinn-orchestration-types`
/// but must never pull the agent facade into its production dependency graph.
/// A test-only edge is permitted for cross-crate integration regressions.
#[test]
fn boundary_djinn_coordinator_has_no_agent_dependency() {
    let cargo_toml = include_str!("../../../djinn-coordinator/Cargo.toml");
    let dep_pattern = "djinn-agent =";
    let has_dep = cargo_toml
        .lines()
        .skip_while(|line| line.trim() != "[dependencies]")
        .skip(1)
        .take_while(|line| !line.trim_start().starts_with('['))
        .any(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with('#') && trimmed.starts_with(dep_pattern)
        });
    assert!(
        !has_dep,
        "djinn-coordinator production dependencies must not include djinn-agent"
    );
}

/// `djinn-slot` production code must NOT use `djinn-coordinator` types.
#[test]
fn boundary_djinn_slot_source_has_no_coordinator_import() {
    assert_no_production_import("../djinn-slot/src", "djinn_coordinator");
}

/// `djinn-coordinator` production code must NOT use `djinn_agent` types.
///
/// `djinn-agent` is a *dev*-dependency of `djinn-coordinator`, so a
/// `#[cfg(test)]` import compiles and a production one does not.  That makes
/// this guard's job the narrow one it looks like: catch the moment the edge is
/// promoted to `[dependencies]` and used, with a message that names the file.
#[test]
fn boundary_djinn_coordinator_source_has_no_agent_import() {
    assert_no_production_import("../djinn-coordinator/src", "djinn_agent");
}

/// `djinn-coordinator` must NOT have a direct `sqlx` dependency.
///
/// All coordinator SQL is routed through `djinn-db` repository and
/// test-support helpers; the crate's `Cargo.toml` must not list `sqlx`
/// as a dependency.
#[test]
fn boundary_djinn_coordinator_has_no_sqlx_dependency() {
    let cargo_toml = include_str!("../../../djinn-coordinator/Cargo.toml");
    let has_sqlx = cargo_toml.lines().any(|line| {
        let trimmed = line.trim_start();
        !trimmed.starts_with('#') && trimmed.starts_with("sqlx =")
    });
    assert!(
        !has_sqlx,
        "djinn-coordinator Cargo.toml must not contain a direct sqlx dependency; \
         all SQL should go through djinn-db helpers"
    );
}

/// `djinn-slot` must NOT have a direct `sqlx` dependency.
///
/// All slot SQL is routed through `djinn-db` helpers; the crate's
/// `Cargo.toml` must not list `sqlx` as a dependency.
#[test]
fn boundary_djinn_slot_has_no_sqlx_dependency() {
    let cargo_toml = include_str!("../../../djinn-slot/Cargo.toml");
    let has_sqlx = cargo_toml.lines().any(|line| {
        let trimmed = line.trim_start();
        !trimmed.starts_with('#') && trimmed.starts_with("sqlx =")
    });
    assert!(
        !has_sqlx,
        "djinn-slot Cargo.toml must not contain a direct sqlx dependency; \
         all SQL should go through djinn-db helpers"
    );
}

// ── The source guard ─────────────────────────────────────────────────────────

/// Fail if any production `.rs` file under `relative_src_dir` declares a `use`
/// of `crate_ident`.
///
/// The scanner's own failures are asserted BEFORE the offender list: a file the
/// scanner cannot resolve is not evidence of a clean tree, and reporting it as
/// one is how a guard goes quietly dead.
fn assert_no_production_import(relative_src_dir: &str, crate_ident: &str) {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative_src_dir);
    // Previously this whole scan sat behind `if src_dir.exists()`, so a moved
    // or renamed crate would have silently emptied the guard.
    assert!(
        src_dir.exists(),
        "{} must exist; a missing tree makes this guard vacuous rather than green",
        src_dir.display()
    );

    let mut offenders = Vec::new();
    let mut problems = Vec::new();
    for path in rust_files(&src_dir) {
        let relative = path.strip_prefix(&src_dir).unwrap_or(&path).to_path_buf();
        if is_test_only_path(&relative) {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            problems.push(format!("{}: unreadable", relative.display()));
            continue;
        };
        match production_code(&contents) {
            Ok(code) => {
                if declares_use_of_crate(&code, crate_ident) {
                    offenders.push(relative.display().to_string());
                }
            }
            Err(error) => problems.push(format!("{}: {error}", relative.display())),
        }
    }
    offenders.sort();

    assert!(
        problems.is_empty(),
        "the {crate_ident} boundary scanner could not resolve some sources; fix the \
         scanner rather than the guard: {problems:?}"
    );
    assert!(
        offenders.is_empty(),
        "{} production source must not import {crate_ident}; offenders: {offenders:?}",
        relative_src_dir
            .trim_start_matches("../")
            .trim_end_matches("/src")
    );
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match std::fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
                result.push(path);
            }
        }
    }
    result
}

/// Whether `relative` (a path relative to a crate's `src/`) names test-only
/// source.
///
/// `Path::starts_with` — the form this replaced — compares whole COMPONENTS
/// anchored at the root, so it excluded `src/tests/*` and nothing else.
/// `src/cargo_warm_base_gc/tests/pressure_execution.rs` is test code that it
/// missed, and that file imports `djinn_agent_worker`, a dev-dependency.
fn is_test_only_path(relative: &Path) -> bool {
    relative
        .components()
        .any(|component| component.as_os_str() == "tests")
        || relative.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name == "tests.rs"
                || name.ends_with("_tests.rs")
                || name.ends_with("_test.rs")
                || name.contains("test_helper")
        })
}

/// Whether `code` carries a `use` declaration naming exactly `crate_ident`.
///
/// `contains("use djinn_agent")` — the form this replaced — also matches
/// `use djinn_agent_worker::…`, a DIFFERENT crate that djinn-coordinator
/// legitimately dev-depends on.  The crate name must end on a token boundary
/// and be introduced by `use`, which covers `use x::…`, `use x::{…}`,
/// `use x;`, `use x as y;`, `pub use x::…`, and `use ::x::…`.
///
/// Known gap: a fully qualified `djinn_agent::Foo` with no `use` declaration is
/// not matched.  That form cannot compile today (the edge is a dev-dependency,
/// and `boundary_djinn_coordinator_has_no_agent_dependency` guards the
/// promotion), so the `use` forms are where this guard adds signal.
fn declares_use_of_crate(code: &str, crate_ident: &str) -> bool {
    code.match_indices(crate_ident).any(|(start, _)| {
        let before = code[..start].chars().next_back();
        let after = code[start + crate_ident.len()..].chars().next();
        if before.is_some_and(is_ident_char) || after.is_some_and(is_ident_char) {
            return false;
        }
        // `use ::krate` and `use krate` both reduce to a prefix ending in `use`.
        let prefix = code[..start].trim_end();
        let prefix = prefix.strip_suffix("::").unwrap_or(prefix).trim_end();
        match prefix.strip_suffix("use") {
            Some(head) => !head.ends_with(is_ident_char),
            None => false,
        }
    })
}

// ── Shared source-guard primitives ───────────────────────────────────────────
//
// A text guard matches against SOURCE, and a comment is not source: it cannot
// import a crate or call a function.  So a comment must neither trip a ban nor
// satisfy a presence assertion, and both directions have bitten this repo (see
// the `code_lines` commentary in `server/tests/task_run_resize_kind.rs`).  A
// string literal is not source either: `"classify_task_liveness: …"` in a log
// message is not a call to it.
//
// `#[cfg(test)]` code is likewise not production source, and tracking it MUST
// be structural.  `scripts/check-resize-reachability.sh` truncated its scan at
// the FIRST `#[cfg(test)]` marker; in `server/src/server/state/mod.rs` that
// marker is a struct FIELD attribute on line 342 of 4147, so the scan saw 8% of
// the file and passed.  An attribute on a field (`…,`) or on a declaration
// (`mod x;`) opens no block at all, and arming a "skip to the next closing
// brace" on either swallows every line of production code that follows.

const CFG_TEST: &str = "#[cfg(test)]";

/// Keywords introducing an item or statement whose body a `#[cfg(test)]`
/// attribute gates, once any visibility is stripped.  Anything else an
/// attribute can precede — a struct field, an enum variant, a match arm — owns
/// no scope to exclude.
const ITEM_KEYWORDS: &[&str] = &[
    "async",
    "const",
    "default",
    "enum",
    "extern",
    "fn",
    "impl",
    "let",
    "macro",
    "macro_rules",
    "mod",
    "static",
    "struct",
    "trait",
    "type",
    "union",
    "unsafe",
    "use",
];

/// `source` reduced to production Rust: comment bodies and literal bodies
/// blanked, `#[cfg(test)]`-gated items removed.
///
/// Removed text is replaced by spaces rather than deleted, so line structure
/// and offsets survive: a needle can never be manufactured by splicing two
/// distant lines together, and a trailing comment cannot launder the code in
/// front of it — `foo(); // banned` still contains a real call to `foo()`.
///
/// Returns `Err` when the source cannot be resolved (an unterminated literal, an
/// unbalanced delimiter, or a `#[cfg(test)]` item with neither a body nor a
/// terminator).  Callers MUST fail on `Err`: an unscannable file is not a clean
/// one.
///
/// Only the canonical `#[cfg(test)]` spelling is recognized.  `#[cfg(all(test,
/// …))]` and friends do not appear in the guarded trees; if one is introduced,
/// its body is scanned as production code, which is the safe direction for a ban
/// and a visible one for a presence assertion.
pub(super) fn production_code(source: &str) -> Result<String, String> {
    strip_cfg_test(&mask_non_code(source)?)
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Overwrite `range` with spaces, preserving newlines.
fn blank(text: &mut [char], range: std::ops::Range<usize>) {
    for slot in &mut text[range] {
        if *slot != '\n' {
            *slot = ' ';
        }
    }
}

/// Blank every comment body and every literal body in `source`.
///
/// After this, the remaining `{`, `}`, `#` and `;` characters are all real
/// tokens, which is what makes the structural `#[cfg(test)]` walk below safe.
/// A hand-rolled brace counter without this step is exactly what the
/// `StreamEvent` audit had to replace: one `{` inside a string literal left the
/// scan unbalanced and an entire test module was audited as production code.
fn mask_non_code(source: &str) -> Result<String, String> {
    let chars: Vec<char> = source.chars().collect();
    let mut out = chars.clone();
    let mut index = 0;
    while index < chars.len() {
        let current = chars[index];
        let after_ident = index > 0 && is_ident_char(chars[index - 1]);

        if current == '/' && chars.get(index + 1) == Some(&'/') {
            let start = index;
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
            blank(&mut out, start..index);
            continue;
        }
        if current == '/' && chars.get(index + 1) == Some(&'*') {
            let start = index;
            let mut depth = 0usize;
            while index + 1 < chars.len() {
                if chars[index] == '/' && chars[index + 1] == '*' {
                    depth += 1;
                    index += 2;
                } else if chars[index] == '*' && chars[index + 1] == '/' {
                    depth -= 1;
                    index += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    index += 1;
                }
            }
            if depth != 0 {
                return Err("unterminated block comment".to_owned());
            }
            blank(&mut out, start..index);
            continue;
        }
        if !after_ident && let Some((quote, hashes)) = raw_literal_open(&chars, index) {
            let (close, end) = raw_literal_close(&chars, quote, hashes)?;
            blank(&mut out, quote + 1..close);
            index = end;
            continue;
        }
        // `"…"`, `b"…"`, `c"…"`, `b'…'`.
        let quoted = if current == '"' {
            Some(index)
        } else if !after_ident
            && matches!(
                (current, chars.get(index + 1).copied()),
                ('b' | 'c', Some('"')) | ('b', Some('\''))
            )
        {
            Some(index + 1)
        } else {
            None
        };
        if let Some(open) = quoted {
            let close = quoted_literal_close(&chars, open)?;
            blank(&mut out, open + 1..close);
            index = close + 1;
            continue;
        }
        if current == '\'' {
            // `'a'` and `'\n'` are literals; `'static` and `'outer:` are not.
            if chars.get(index + 1) == Some(&'\\') || chars.get(index + 2) == Some(&'\'') {
                let close = quoted_literal_close(&chars, index)?;
                blank(&mut out, index + 1..close);
                index = close + 1;
            } else {
                index += 1;
            }
            continue;
        }
        index += 1;
    }
    Ok(out.into_iter().collect())
}

/// If a raw literal starts at `start`, its opening quote index and hash count.
fn raw_literal_open(chars: &[char], start: usize) -> Option<(usize, usize)> {
    let mut index = start;
    if matches!(chars.get(index), Some('b' | 'c')) {
        index += 1;
    }
    if chars.get(index) != Some(&'r') {
        return None;
    }
    index += 1;
    let hashes_start = index;
    while chars.get(index) == Some(&'#') {
        index += 1;
    }
    // `r#type` is a raw identifier, not a raw string.
    (chars.get(index) == Some(&'"')).then(|| (index, index - hashes_start))
}

/// The closing quote index and the index just past the closing hashes.
fn raw_literal_close(
    chars: &[char],
    quote: usize,
    hashes: usize,
) -> Result<(usize, usize), String> {
    let mut index = quote + 1;
    while index < chars.len() {
        if chars[index] == '"'
            && chars[index + 1..]
                .iter()
                .take(hashes)
                .filter(|c| **c == '#')
                .count()
                == hashes
        {
            return Ok((index, index + 1 + hashes));
        }
        index += 1;
    }
    Err("unterminated raw literal".to_owned())
}

/// The closing delimiter index of the escaped literal opened at `open`.
fn quoted_literal_close(chars: &[char], open: usize) -> Result<usize, String> {
    let delimiter = chars[open];
    let mut index = open + 1;
    while index < chars.len() {
        match chars[index] {
            '\\' => index += 2,
            c if c == delimiter => return Ok(index),
            _ => index += 1,
        }
    }
    Err(format!("unterminated {delimiter} literal"))
}

/// Blank every `#[cfg(test)]`-gated item in already-masked source.
fn strip_cfg_test(masked: &str) -> Result<String, String> {
    let chars: Vec<char> = masked.chars().collect();
    let mut out = chars.clone();
    let marker: Vec<char> = CFG_TEST.chars().collect();
    let mut index = 0;
    while index + marker.len() <= chars.len() {
        if chars[index..index + marker.len()] != marker[..] {
            index += 1;
            continue;
        }
        let attribute_end = index + marker.len();
        match gated_region_end(&chars, attribute_end)? {
            // A gated item: drop the attribute and everything it gates.
            Some(end) => {
                blank(&mut out, index..end);
                index = end;
            }
            // A gated field, variant, arm, or statement: there is no scope to
            // exclude, and everything after it is still production code.
            None => {
                blank(&mut out, index..attribute_end);
                index = attribute_end;
            }
        }
    }
    Ok(out.into_iter().collect())
}

/// Where the item gated by a `#[cfg(test)]` ending at `from` stops, or `None`
/// when the attribute gates something that owns no scope.
fn gated_region_end(chars: &[char], from: usize) -> Result<Option<usize>, String> {
    let mut index = skip_whitespace(chars, from);
    // Attributes stacked on the same item, e.g. `#[cfg(test)] #[derive(Debug)]`.
    while chars.get(index) == Some(&'#') {
        let open = skip_whitespace(chars, index + 1);
        if chars.get(open) != Some(&'[') {
            break;
        }
        index = skip_whitespace(chars, matching_delimiter(chars, open)? + 1);
    }
    // A bare gated block: `#[cfg(test)] { true }`.
    if chars.get(index) == Some(&'{') {
        return Ok(Some(matching_delimiter(chars, index)? + 1));
    }
    let mut keyword = read_ident(chars, index);
    if keyword == "pub" {
        index = skip_whitespace(chars, index + keyword.len());
        if chars.get(index) == Some(&'(') {
            index = skip_whitespace(chars, matching_delimiter(chars, index)? + 1);
        }
        keyword = read_ident(chars, index);
    }
    if !ITEM_KEYWORDS.contains(&keyword.as_str()) {
        return Ok(None);
    }
    while index < chars.len() {
        match chars[index] {
            // Generic bounds, argument lists, and array types can hold both `;`
            // and `{`; skipping them balanced keeps the search at item level.
            '(' | '[' => index = matching_delimiter(chars, index)? + 1,
            '{' => return Ok(Some(matching_delimiter(chars, index)? + 1)),
            ';' => return Ok(Some(index + 1)),
            _ => index += 1,
        }
    }
    Err(format!(
        "the #[cfg(test)] item at char {from} has neither a body nor a terminator"
    ))
}

fn matching_delimiter(chars: &[char], open: usize) -> Result<usize, String> {
    let mut stack: Vec<char> = Vec::new();
    for (offset, current) in chars.iter().enumerate().skip(open) {
        match current {
            '(' => stack.push(')'),
            '[' => stack.push(']'),
            '{' => stack.push('}'),
            ')' | ']' | '}' => match stack.pop() {
                Some(expected) if expected == *current => {
                    if stack.is_empty() {
                        return Ok(offset);
                    }
                }
                _ => return Err(format!("unbalanced delimiter at char {offset}")),
            },
            _ => {}
        }
    }
    Err(format!("unclosed delimiter at char {open}"))
}

fn skip_whitespace(chars: &[char], from: usize) -> usize {
    let mut index = from;
    while chars.get(index).is_some_and(|c| c.is_whitespace()) {
        index += 1;
    }
    index
}

fn read_ident(chars: &[char], from: usize) -> String {
    chars
        .get(from..)
        .unwrap_or_default()
        .iter()
        .take_while(|c| is_ident_char(**c))
        .collect()
}

// ── Self-tests for the primitives above ──────────────────────────────────────
//
// Without these, "ignores comments" and "ignores everything" are
// indistinguishable from a green run.

/// The predicate the two source guards are built from.
fn production_import(source: &str, crate_ident: &str) -> bool {
    let code = production_code(source).expect("the fixture is scannable");
    declares_use_of_crate(&code, crate_ident)
}

#[test]
fn a_real_production_import_is_an_offence() {
    assert!(production_import(
        "use djinn_agent::context::AgentContext;\nfn main() {}\n",
        "djinn_agent"
    ));
    assert!(production_import(
        "use djinn_agent::{context, roles};\n",
        "djinn_agent"
    ));
    assert!(production_import("use djinn_agent;\n", "djinn_agent"));
    assert!(production_import(
        "use djinn_agent as agent;\n",
        "djinn_agent"
    ));
    assert!(production_import(
        "pub use ::djinn_agent::context;\n",
        "djinn_agent"
    ));
}

#[test]
fn a_comment_or_a_string_naming_the_import_is_not_an_offence() {
    assert!(!production_import(
        "//! Mirrors `use djinn_agent::context;` without the dependency.\n",
        "djinn_agent"
    ));
    assert!(!production_import(
        "// use djinn_agent::context::AgentContext;\nfn main() {}\n",
        "djinn_agent"
    ));
    assert!(!production_import(
        "/* use djinn_agent::context; */\nfn main() {}\n",
        "djinn_agent"
    ));
    assert!(!production_import(
        "const BANNED: &str = \"use djinn_agent::context;\";\n",
        "djinn_agent"
    ));
    assert!(!production_import(
        "const BANNED: &str = r#\"use djinn_agent::context;\"#;\n",
        "djinn_agent"
    ));
}

#[test]
fn a_trailing_comment_does_not_launder_the_import_in_front_of_it() {
    assert!(production_import(
        "use djinn_agent::context; // test-only, honest\n",
        "djinn_agent"
    ));
}

#[test]
fn a_sibling_crate_with_a_shared_prefix_is_not_an_offence() {
    let source = "use djinn_agent_worker::cargo_incremental_prune::WarmWorkPhase;\n";
    assert!(!production_import(source, "djinn_agent"));
    assert!(production_import(source, "djinn_agent_worker"));
}

#[test]
fn a_cfg_test_import_is_excluded_but_production_code_after_it_is_not() {
    // The `check-resize-reachability.sh` blind spot: a scan that stops at the
    // first `#[cfg(test)]` marker never sees anything below it.
    let gated_only = "\
#[cfg(test)]
mod tests {
    use djinn_agent::context::AgentContext;
    fn helper() {}
}
fn production() {}
";
    assert!(!production_import(gated_only, "djinn_agent"));

    let gated_then_production = "\
#[cfg(test)]
mod tests {
    use djinn_slot::SlotHandle;
}
use djinn_agent::context::AgentContext;
fn production() {}
";
    assert!(production_import(gated_then_production, "djinn_agent"));
}

#[test]
fn a_cfg_test_field_or_declaration_does_not_swallow_the_code_after_it() {
    let field = "\
struct Config {
    #[cfg(test)]
    test_use_live_credential_resolution: bool,
    real: bool,
}
use djinn_agent::context::AgentContext;
";
    assert!(production_import(field, "djinn_agent"));

    let declaration = "\
#[cfg(test)]
mod slow_tests;
use djinn_agent::context::AgentContext;
";
    assert!(production_import(declaration, "djinn_agent"));

    let statement = "\
fn dispatch() {
    #[cfg(test)]
    observe_dispatch_cap_count(stage, count);
    commit();
}
use djinn_agent::context::AgentContext;
";
    assert!(production_import(statement, "djinn_agent"));
}

#[test]
fn braces_inside_literals_do_not_perturb_cfg_test_ranges() {
    let source = "\
#[cfg(test)]
mod tests {
    const METRIC: &str = \"djinn_taskrun_jobs_started_total{\";
    const RAW: &str = r#\"#[cfg(test)] mod nested {\"#;
    const CH: char = '}';
}
use djinn_agent::context::AgentContext;
";
    assert!(production_import(source, "djinn_agent"));
}

#[test]
fn lifetimes_are_not_mistaken_for_character_literals() {
    let source = "\
struct Borrowed<'a> { name: &'a str }
impl<'a> Borrowed<'a> {
    fn label(&self) -> &'static str { \"x\" }
}
use djinn_agent::context::AgentContext;
";
    assert!(production_import(source, "djinn_agent"));
}

#[test]
fn the_test_only_path_filter_covers_nested_test_trees() {
    for excluded in [
        "tests/boundary.rs",
        "cargo_warm_base_gc/tests/pressure_execution.rs",
        "dispatch/tests.rs",
        "resize_lift_tests.rs",
        "dispatch/admission_test.rs",
        "test_helpers.rs",
    ] {
        assert!(
            is_test_only_path(Path::new(excluded)),
            "{excluded} must be treated as test-only"
        );
    }
    for included in [
        "lib.rs",
        "dispatch/session_recovery.rs",
        "cargo_warm_base_gc/pressure.rs",
        "latest_attempt.rs",
    ] {
        assert!(
            !is_test_only_path(Path::new(included)),
            "{included} must be treated as production"
        );
    }
}

#[test]
fn an_unresolvable_source_is_reported_rather_than_reported_clean() {
    assert!(production_code("const OPEN: &str = \"unterminated;\n").is_err());
    assert!(production_code("#[cfg(test)]\nmod tests {\n").is_err());
}
