//! Static inventory only: this test makes no behavioral claim.
//!
//! It catches new production `match` sites with `StreamEvent` arms that need an
//! explicit owner-and-arm classification. Behavioral assertions remain in each
//! owner crate.
//!
//! Scope detection is tokenizer-driven. `proc-macro2` lexes each file and
//! balances its delimiters, so brace-like content in string, raw-string,
//! byte-string, and character literals, or in line and block comments, cannot
//! perturb `#[cfg(test)]` ranges. The hand-rolled brace counter this replaced
//! could not: a test asserting on `"djinn_taskrun_jobs_started_total{"` left an
//! unbalanced `{` inside a string literal, the scan bailed out with no test
//! scopes at all, an entire test module was audited as production code, and the
//! failure named an unrelated file and symbol. Scanner failures now name the
//! file that caused them instead of silently reclassifying test code.

use proc_macro2::{Delimiter, Group, TokenStream, TokenTree};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::Range;
use std::path::Path;

const FIXTURE: &str = include_str!("fixtures/stream_event_consumer_audit.tsv");
const CFG_TEST: &str = "#[cfg(test)]";

/// Keywords introducing an item or statement whose body a `#[cfg(test)]`
/// attribute gates, once any visibility is stripped. Anything else an attribute
/// can precede -- a struct field, an enum variant, a match arm -- owns no scope
/// to exclude, whether or not it carries a `pub(..)` visibility.
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

fn is_test_only(path: &Path) -> bool {
    path.components().any(|part| part.as_os_str() == "tests")
        || path.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name == "tests.rs" || name == "test_helpers.rs" || name.ends_with("_tests.rs")
        })
}

/// A `match` keyword offset with the byte range of its arm body, braces included.
#[derive(Debug)]
struct MatchSite {
    keyword: usize,
    body: Range<usize>,
}

/// What a recognized `#[cfg(test)]` attribute turned out to gate.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Attribute {
    /// A brace-bodied item: its body range is a test scope.
    Body(Range<usize>),
    /// `#[cfg(test)] mod slow_tests;` -- the gated code lives in another file.
    Declaration,
    /// A gated field, variant, or arm: there is no body to exclude.
    NotAnItem,
    /// Neither a body nor a terminator was found: the scanner is broken or the
    /// source is truncated. Never silently treated as "no test scope".
    Unresolved,
}

#[derive(Debug, Default)]
struct SourceIndex {
    /// `match` sites in source order.
    matches: Vec<MatchSite>,
    /// Recognized `#[cfg(test)]` attributes, keyed by the `#` byte offset.
    attributes: BTreeMap<usize, Attribute>,
    /// Byte offsets of every `#` punctuation token the lexer produced, so text
    /// that merely looks like an attribute -- inside a literal or a comment --
    /// is never mistaken for one.
    hash_offsets: BTreeSet<usize>,
}

impl SourceIndex {
    /// Unit-test scopes are fixtures/assertions rather than production match
    /// sites. A scope spans the attribute through the closing brace of the item
    /// it gates.
    fn test_scopes(&self) -> Vec<Range<usize>> {
        self.attributes
            .iter()
            .filter_map(|(attribute_offset, attribute)| match attribute {
                Attribute::Body(body) => Some(*attribute_offset..body.end),
                _ => None,
            })
            .collect()
    }
}

fn index_source(source: &str) -> Result<SourceIndex, String> {
    let stream = source
        .parse::<TokenStream>()
        .map_err(|error| format!("cannot lex this file as Rust: {error}"))?;
    let mut index = SourceIndex::default();
    index_level(&stream.into_iter().collect::<Vec<_>>(), &mut index);
    index.matches.sort_by_key(|site| site.keyword);
    Ok(index)
}

/// Index one token-stream level, then descend into its groups. Delimiters are
/// already balanced by the lexer, so no brace counting happens anywhere here.
fn index_level(trees: &[TokenTree], index: &mut SourceIndex) {
    for (position, tree) in trees.iter().enumerate() {
        match tree {
            TokenTree::Ident(ident) if ident == "match" => {
                if let Some(body) = arm_body(&trees[position + 1..]) {
                    index.matches.push(MatchSite {
                        keyword: ident.span().byte_range().start,
                        body,
                    });
                }
            }
            TokenTree::Punct(punct) if punct.as_char() == '#' => {
                let attribute_offset = punct.span().byte_range().start;
                index.hash_offsets.insert(attribute_offset);
                if is_cfg_test(trees.get(position + 1)) {
                    index
                        .attributes
                        .insert(attribute_offset, gated_item(&trees[position + 2..]));
                }
            }
            _ => {}
        }
    }
    for tree in trees {
        if let TokenTree::Group(group) = tree {
            index_level(&group.stream().into_iter().collect::<Vec<_>>(), index);
        }
    }
}

/// A match body is the first sibling brace group after the keyword: braces in
/// the scrutinee are always nested inside its own parentheses or brackets, and
/// `{name}` in a format string is not a brace token at all.
fn arm_body(rest: &[TokenTree]) -> Option<Range<usize>> {
    for tree in rest {
        match tree {
            TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => {
                return Some(brace_range(group));
            }
            TokenTree::Punct(punct) if punct.as_char() == ';' => return None,
            _ => {}
        }
    }
    None
}

fn brace_range(group: &Group) -> Range<usize> {
    group.span_open().byte_range().start..group.span_close().byte_range().end
}

/// Exactly `#[cfg(test)]`, matched on tokens rather than text.
fn is_cfg_test(tree: Option<&TokenTree>) -> bool {
    let Some(TokenTree::Group(bracket)) = tree else {
        return false;
    };
    if bracket.delimiter() != Delimiter::Bracket {
        return false;
    }
    let trees = bracket.stream().into_iter().collect::<Vec<_>>();
    let [TokenTree::Ident(name), TokenTree::Group(predicate)] = trees.as_slice() else {
        return false;
    };
    if name != "cfg" || predicate.delimiter() != Delimiter::Parenthesis {
        return false;
    }
    matches!(
        predicate.stream().into_iter().collect::<Vec<_>>().as_slice(),
        [TokenTree::Ident(flag)] if flag == "test"
    )
}

/// Classify what follows a `#[cfg(test)]` attribute at the same token level.
fn gated_item(rest: &[TokenTree]) -> Attribute {
    let mut rest = rest;
    while let [TokenTree::Punct(punct), TokenTree::Group(group), tail @ ..] = rest {
        if punct.as_char() != '#' || group.delimiter() != Delimiter::Bracket {
            break;
        }
        rest = tail;
    }
    // `#[cfg(test)] pub(crate) probe: Option<..>` is a field, not an item, so
    // visibility is stripped before the item keyword is checked.
    if let [TokenTree::Ident(visibility), tail @ ..] = rest
        && visibility == "pub"
    {
        rest = match tail {
            [TokenTree::Group(group), scoped @ ..]
                if group.delimiter() == Delimiter::Parenthesis =>
            {
                scoped
            }
            _ => tail,
        };
    }
    let Some(TokenTree::Ident(keyword)) = rest.first() else {
        return Attribute::NotAnItem;
    };
    let keyword = keyword.to_string();
    if !ITEM_KEYWORDS.contains(&keyword.as_str()) {
        return Attribute::NotAnItem;
    }
    // A `use` item's braces are an import list, not a body.
    if keyword == "use" {
        return Attribute::Declaration;
    }
    for tree in rest {
        match tree {
            TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => {
                return Attribute::Body(brace_range(group));
            }
            TokenTree::Punct(punct) if punct.as_char() == ';' => return Attribute::Declaration,
            _ => {}
        }
    }
    Attribute::Unresolved
}

/// Match patterns begin an arm line with `StreamEvent` (or its `Result` /
/// `Option` wrapper). This excludes producers that construct a `StreamEvent`
/// in a different wire-event match body.
fn has_stream_event_arm(match_body: &str) -> bool {
    match_body.lines().any(|line| {
        let pattern = line.trim_start().trim_start_matches('|').trim_start();
        pattern.starts_with("StreamEvent::")
            || pattern.starts_with("Ok(StreamEvent::")
            || pattern.starts_with("Some(StreamEvent::")
    })
}

fn enclosing_function(source: &str, offset: usize) -> &str {
    source[..offset]
        .rmatch_indices("fn ")
        .find_map(|(function_offset, _)| {
            source[function_offset + 3..]
                .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
                .next()
                .filter(|name| !name.is_empty())
        })
        .unwrap_or("module")
}

fn line_of(source: &str, offset: usize) -> usize {
    source[..offset].lines().count().max(1)
}

#[derive(Debug, Default)]
struct Scan {
    /// `function#occurrence` for every production `StreamEvent` match site.
    sites: Vec<String>,
    test_scopes: Vec<Range<usize>>,
    /// Scanner defects, each naming the file (and line) that exposed them.
    problems: Vec<String>,
}

/// Report every way scope detection could have gone wrong for this file, so a
/// scanner defect is never mistaken for an unclassified production match site.
fn scanner_problems(
    label: &str,
    source: &str,
    index: &SourceIndex,
    test_scopes: &[Range<usize>],
) -> Vec<String> {
    let mut problems = Vec::new();
    for (attribute_offset, attribute) in &index.attributes {
        if *attribute == Attribute::Unresolved {
            problems.push(format!(
                "{label}:{}: StreamEvent audit scanner cannot resolve the scope gated by this `{CFG_TEST}`; its test code would be audited as production code",
                line_of(source, *attribute_offset)
            ));
        }
    }
    for (offset, _) in source.match_indices(CFG_TEST) {
        if index.hash_offsets.contains(&offset) && !index.attributes.contains_key(&offset) {
            problems.push(format!(
                "{label}:{}: StreamEvent audit scanner did not recognize this `{CFG_TEST}` attribute",
                line_of(source, offset)
            ));
        }
    }
    if test_scopes.is_empty()
        && index
            .attributes
            .values()
            .any(|attribute| !matches!(attribute, Attribute::NotAnItem | Attribute::Declaration))
    {
        problems.push(format!(
            "{label}: StreamEvent audit scanner resolved no test scopes though this file gates a brace-bodied `{CFG_TEST}` item; its whole test module would be audited as production code"
        ));
    }
    problems
}

fn scan_source(label: &str, source: &str) -> Scan {
    let index = match index_source(source) {
        Ok(index) => index,
        Err(error) => {
            return Scan {
                problems: vec![format!("{label}: StreamEvent audit scanner {error}")],
                ..Scan::default()
            };
        }
    };
    let test_scopes = index.test_scopes();
    let problems = scanner_problems(label, source, &index, &test_scopes);
    let mut sites = Vec::new();
    let mut function_match_counts = BTreeMap::new();
    for site in &index.matches {
        if test_scopes
            .iter()
            .any(|scope| scope.contains(&site.keyword))
            || !has_stream_event_arm(&source[site.body.clone()])
        {
            continue;
        }
        let function = enclosing_function(source, site.keyword);
        let count = function_match_counts
            .entry(function.to_owned())
            .or_insert(0usize);
        *count += 1;
        sites.push(format!("{function}#{count}"));
    }
    Scan {
        sites,
        test_scopes,
        problems,
    }
}

fn collect_match_sites(
    root: &Path,
    repo_root: &Path,
    found: &mut BTreeSet<String>,
    problems: &mut Vec<String>,
) {
    for entry in fs::read_dir(root).expect("read source directory") {
        let entry = entry.expect("directory entry");
        let path = entry.path();
        if path.is_dir() {
            collect_match_sites(&path, repo_root, found, problems);
        } else if path.extension().is_some_and(|extension| extension == "rs")
            && !is_test_only(&path)
        {
            let source = fs::read_to_string(&path).expect("read Rust source");
            let relative_path = path
                .strip_prefix(repo_root)
                .expect("path below repository")
                .to_string_lossy()
                .replace('\\', "/");
            let scan = scan_source(&relative_path, &source);
            problems.extend(scan.problems);
            found.extend(
                scan.sites
                    .into_iter()
                    .map(|site| format!("{relative_path}::{site}")),
            );
        }
    }
}

/// The exact shape that broke the hand-rolled scanner: an unbalanced `{` inside
/// an ordinary string literal in the test module.
const UNBALANCED_BRACE_IN_STRING: &str = r#"
use crate::StreamEvent;

pub fn production_consumer(event: StreamEvent) -> u8 {
    match event {
        StreamEvent::Done => 0,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_metric_family_prefix() {
        let rendered = String::new();
        assert!(!rendered.contains("djinn_taskrun_jobs_started_total{"));
    }

    #[test]
    fn test_only_consumer(event: StreamEvent) {
        match event {
            StreamEvent::Report { .. } => {}
            _ => {}
        }
    }
}
"#;

#[test]
fn unbalanced_brace_in_a_string_literal_cannot_swallow_the_test_scope() {
    let scan = scan_source("fixture.rs", UNBALANCED_BRACE_IN_STRING);

    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    assert_eq!(scan.test_scopes.len(), 1, "{:?}", scan.test_scopes);
    let scope = scan.test_scopes[0].clone();
    assert!(
        UNBALANCED_BRACE_IN_STRING[scope.clone()].starts_with(CFG_TEST),
        "scope must start at the attribute"
    );
    assert_eq!(
        UNBALANCED_BRACE_IN_STRING[scope.clone()].trim_end(),
        UNBALANCED_BRACE_IN_STRING[scope.start..].trim_end(),
        "scope must reach the closing brace of the test module"
    );
    assert_eq!(
        scan.sites,
        vec!["production_consumer#1".to_string()],
        "the test-only StreamEvent::Report match must stay out of the production inventory"
    );
}

/// Every lexical construct that can carry brace-like content. A hand-rolled
/// counter has to special-case each one; the lexer does not.
const LEXICAL_NOISE: &str = r####"
use crate::StreamEvent;

pub fn production_consumer(event: StreamEvent) -> u8 {
    // Braces in a line comment: { { {
    /* Braces in a block comment: } } /* nested: { */ still commented } */
    let open = '{';
    let close = '}';
    let byte_open = b'{';
    let raw = r#"raw string with "quotes" and an unbalanced {"#;
    let raw_hashes = r##"raw string with "# inside and }}} braces"##;
    let bytes = b"byte string {{{";
    let raw_bytes = br#"raw byte string {"#;
    let escaped = "escaped \" quote and \u{7b} brace";
    let _ = (open, close, byte_open, raw, raw_hashes, bytes, raw_bytes, escaped);
    match event {
        StreamEvent::Done => 0,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Braces in a line comment inside the test module: { { {
    /* and in a /* nested */ block comment: } } } */
    const OPEN: char = '{';
    const RAW: &str = r#"unbalanced { inside a raw string"#;
    const RAW_HASHES: &str = r##"a "# sequence and an unbalanced }"##;
    const BYTES: &[u8] = b"unbalanced { in a byte string";
    const RAW_BYTES: &[u8] = br##"unbalanced "# { in a raw byte string"##;

    #[test]
    fn test_only_consumer(event: StreamEvent) {
        let _ = (OPEN, RAW, RAW_HASHES, BYTES, RAW_BYTES);
        match event {
            StreamEvent::Report { .. } => {}
            _ => {}
        }
    }
}
"####;

#[test]
fn lexical_brace_like_content_cannot_perturb_scope_matching() {
    let scan = scan_source("fixture.rs", LEXICAL_NOISE);

    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    assert_eq!(scan.test_scopes.len(), 1, "{:?}", scan.test_scopes);
    let scope = scan.test_scopes[0].clone();
    assert!(LEXICAL_NOISE[scope.clone()].starts_with(CFG_TEST));
    assert_eq!(
        LEXICAL_NOISE[scope.clone()].trim_end(),
        LEXICAL_NOISE[scope.start..].trim_end(),
        "scope must reach the closing brace of the test module"
    );
    assert_eq!(
        scan.sites,
        vec!["production_consumer#1".to_string()],
        "brace-like literal and comment content must not move the production inventory"
    );
}

#[test]
fn unresolvable_test_scope_is_reported_against_its_own_file() {
    let source = "#[cfg(test)]\nmod tests\n";
    let scan = scan_source("truncated.rs", source);

    assert!(scan.test_scopes.is_empty());
    assert!(
        scan.problems
            .iter()
            .any(|problem| problem.starts_with("truncated.rs:1:")
                && problem.contains("cannot resolve the scope")),
        "{:?}",
        scan.problems
    );
    assert!(
        scan.problems
            .iter()
            .any(|problem| problem.contains("resolved no test scopes")),
        "{:?}",
        scan.problems
    );
}

#[test]
fn unlexable_source_is_reported_against_its_own_file() {
    let scan = scan_source("broken.rs", "fn production() { match event {\n");

    assert!(scan.sites.is_empty());
    assert!(
        scan.problems
            .iter()
            .any(|problem| problem.starts_with("broken.rs:") && problem.contains("cannot lex")),
        "{:?}",
        scan.problems
    );
}

#[test]
fn cfg_test_without_a_body_is_not_a_scanner_defect() {
    let source = r#"
#[cfg(test)]
mod slow_tests;

#[cfg(test)]
use std::fmt::{Debug, Display};

pub struct Handler {
    #[cfg(test)]
    hook: Option<u8>,
    #[cfg(test)]
    pub(crate) probe: Option<u8>,
}

pub enum Wire {
    #[cfg(test)]
    Probe { seq: u64 },
}

pub fn production_consumer(event: StreamEvent) -> u8 {
    match event {
        StreamEvent::Done => 0,
        _ => 1,
    }
}
"#;
    let scan = scan_source("declarations.rs", source);

    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    assert!(scan.test_scopes.is_empty(), "{:?}", scan.test_scopes);
    assert_eq!(scan.sites, vec!["production_consumer#1".to_string()]);
}

#[test]
fn visibility_before_a_gated_item_still_resolves_its_scope() {
    let source = r#"
pub fn production_consumer(event: StreamEvent) -> u8 {
    match event {
        StreamEvent::Done => 0,
        _ => 1,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    pub(super) fn test_only_consumer(event: StreamEvent) {
        match event {
            StreamEvent::Report { .. } => {}
            _ => {}
        }
    }
}
"#;
    let scan = scan_source("visibility.rs", source);

    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    assert_eq!(scan.test_scopes.len(), 1, "{:?}", scan.test_scopes);
    assert_eq!(scan.sites, vec!["production_consumer#1".to_string()]);
}

#[test]
fn cfg_test_text_in_a_literal_or_comment_is_not_an_attribute() {
    let source = r##"
// A comment mentioning #[cfg(test)] mod tests {
//! and a doc comment mentioning #[cfg(test)] mod tests {
const SAMPLE: &str = "#[cfg(test)]\nmod tests {";
const RAW_SAMPLE: &str = r#"#[cfg(test)] mod tests {"#;

pub fn production_consumer(event: StreamEvent) -> u8 {
    match event {
        StreamEvent::Done => 0,
        _ => 1,
    }
}
"##;
    let scan = scan_source("mentions.rs", source);

    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    assert!(scan.test_scopes.is_empty(), "{:?}", scan.test_scopes);
    assert_eq!(scan.sites, vec!["production_consumer#1".to_string()]);
}

#[test]
fn braces_in_a_format_string_scrutinee_do_not_hide_the_arm_body() {
    let source = r#"
pub async fn drain(mut stream: Stream) {
    while let Some(event) = stream.next().await {
        match event.map_err(|error| format!("provider stream error: {error}"))? {
            StreamEvent::Done => break,
            _ => continue,
        }
    }
}
"#;
    let scan = scan_source("scrutinee.rs", source);

    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    assert_eq!(scan.sites, vec!["drain#1".to_string()]);
}

#[test]
fn checked_in_classification_covers_every_production_stream_event_match() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let expected = FIXTURE
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            line.split_once('\t')
                .expect("site identifier and arm classification")
                .0
                .to_string()
        })
        .collect::<BTreeSet<_>>();
    let mut actual = BTreeSet::new();
    let mut problems = Vec::new();
    collect_match_sites(
        &repo_root.join("server/crates"),
        &repo_root,
        &mut actual,
        &mut problems,
    );
    collect_match_sites(
        &repo_root.join("server/src"),
        &repo_root,
        &mut actual,
        &mut problems,
    );

    assert!(
        problems.is_empty(),
        "StreamEvent audit scope detection is broken; fix the scanner rather than the inventory:\n{}",
        problems.join("\n")
    );
    assert_eq!(
        actual, expected,
        "update the classification fixture for every production StreamEvent match site; this audit is not behavioral coverage"
    );
    assert!(FIXTURE.contains("drain_provider_turn"));
    assert!(FIXTURE.contains("grouped ignore arm"));
    assert!(FIXTURE.contains("wildcard"));
    assert!(FIXTURE.contains(
        "server/crates/djinn-agent/src/direct_services.rs::append_direct_response_event#1\tshared direct-invocation and planner response match ownership"
    ));
    assert!(!FIXTURE.contains("direct_services.rs::invoke_llm#1"));
    assert!(!FIXTURE.contains("direct_services.rs::collect_planner_stream#1"));
    assert!(
        FIXTURE.contains("no behavioral claim")
            || include_str!("stream_event_consumer_audit.rs").contains("no behavioral claim")
    );
}
