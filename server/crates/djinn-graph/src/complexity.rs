//! Cognitive + cyclomatic complexity walker.
//!
//! Per-function metrics computed from a tree-sitter AST. Cognitive
//! complexity follows the Sonar 2018 spec (G. Ann Campbell); cyclomatic
//! is McCabe's classic decision-point count. Both come out of one walk.
//!
//! Languages are added by extending [`ComplexityLang`] and pointing it
//! at a new [`LangRules`] const. The walker itself is language-agnostic
//! — only the rule set knows about node names.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tree_sitter::{Language, Node, Parser, Tree};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ComplexityMetrics {
    /// McCabe cyclomatic complexity = 1 + decision-point count.
    pub cyclomatic: u16,
    /// Sonar cognitive complexity. Penalises nesting, flat-rates
    /// `else-if`, counts boolean-operator switches.
    pub cognitive: u16,
    /// Non-blank lines inside the body block. Comment-stripping is a
    /// later refinement.
    pub nloc: u16,
    /// Deepest nesting level reached inside the function body.
    pub max_nesting: u8,
    /// Number of formal parameters (includes `self`-receivers).
    pub param_count: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ComplexityLang {
    Rust,
}

impl ComplexityLang {
    pub fn from_scip(lang: &str) -> Option<Self> {
        match lang.trim().to_ascii_lowercase().as_str() {
            "rust" => Some(Self::Rust),
            _ => None,
        }
    }

    fn ts_language(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
        }
    }

    fn rules(self) -> &'static LangRules {
        match self {
            Self::Rust => &RULES_RUST,
        }
    }
}

/// Per-language config consumed by the generic walker. New languages
/// add a const and a [`ComplexityLang`] variant.
pub(crate) struct LangRules {
    /// Function-like declarations whose bodies become independent
    /// metric entries.
    pub function_kinds: &'static [&'static str],
    /// Field name on function nodes carrying the body.
    pub body_field: Option<&'static str>,
    /// Field name on function nodes carrying the parameter list.
    pub parameters_field: Option<&'static str>,
    /// Decision-point kinds whose cost = 1 + nesting; their children
    /// recurse with nesting+1.
    pub nest_increments: &'static [&'static str],
    /// Kinds that bump nesting depth without their own cost (lambdas,
    /// nested closures).
    pub nest_only: &'static [&'static str],
    /// Binary-expression kind for `&&`/`||` chain handling.
    pub binary_kind: Option<&'static str>,
    /// Field name on `binary_expression` carrying the operator token,
    /// or None to fall back to scanning unnamed children.
    pub operator_field: Option<&'static str>,
    /// Operator strings recognised as logical AND / OR.
    pub logical_ops: &'static [&'static str],
    /// `if`-expression kind. When `Some`, an `if` whose parent kind
    /// matches `else_if_parent_kind` is treated as a flat `else if`
    /// increment instead of a nesting one.
    pub if_kind: Option<&'static str>,
    pub else_if_parent_kind: Option<&'static str>,
}

const RULES_RUST: LangRules = LangRules {
    function_kinds: &["function_item"],
    body_field: Some("body"),
    parameters_field: Some("parameters"),
    nest_increments: &[
        "if_expression",
        "match_expression",
        "for_expression",
        "while_expression",
        "loop_expression",
    ],
    nest_only: &["closure_expression"],
    binary_kind: Some("binary_expression"),
    operator_field: Some("operator"),
    logical_ops: &["&&", "||"],
    if_kind: Some("if_expression"),
    else_if_parent_kind: Some("else_clause"),
};

pub struct ComplexityWalker {
    parsers: BTreeMap<ComplexityLang, Parser>,
}

impl Default for ComplexityWalker {
    fn default() -> Self {
        Self::new()
    }
}

impl ComplexityWalker {
    pub fn new() -> Self {
        Self {
            parsers: BTreeMap::new(),
        }
    }

    /// Parse `source` once and emit one [`FunctionMetrics`] per
    /// function declaration encountered. The caller pairs these with
    /// SCIP definition ranges to attach metrics to graph nodes.
    pub fn analyze_file(&mut self, language: &str, source: &str) -> Vec<FunctionMetrics> {
        let Some(lang) = ComplexityLang::from_scip(language) else {
            return Vec::new();
        };
        let Some(tree) = self.parse(lang, source) else {
            return Vec::new();
        };
        let rules = lang.rules();
        let bytes = source.as_bytes();
        let mut out = Vec::new();
        collect_functions(tree.root_node(), rules, bytes, &mut out);
        out
    }

    fn parse(&mut self, lang: ComplexityLang, source: &str) -> Option<Tree> {
        let parser = self.parsers.entry(lang).or_insert_with(|| {
            let mut p = Parser::new();
            let _ = p.set_language(&lang.ts_language());
            p
        });
        parser.parse(source, None)
    }
}

#[derive(Debug, Clone)]
pub struct FunctionMetrics {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: u32,
    pub end_line: u32,
    pub name: Option<String>,
    pub metrics: ComplexityMetrics,
}

pub(crate) fn collect_functions(
    node: Node,
    rules: &LangRules,
    src: &[u8],
    out: &mut Vec<FunctionMetrics>,
) {
    if rules.function_kinds.contains(&node.kind()) {
        out.push(analyze_function(node, rules, src));
    }
    let mut walker = node.walk();
    for child in node.named_children(&mut walker) {
        collect_functions(child, rules, src, out);
    }
}

fn analyze_function(fn_node: Node, rules: &LangRules, src: &[u8]) -> FunctionMetrics {
    let body = rules
        .body_field
        .and_then(|f| fn_node.child_by_field_name(f))
        .unwrap_or(fn_node);
    let params = rules
        .parameters_field
        .and_then(|f| fn_node.child_by_field_name(f));
    let name = fn_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(src).ok())
        .map(str::to_string);

    let mut state = WalkState::default();
    walk(body, rules, src, 0, &mut state);

    let metrics = ComplexityMetrics {
        cyclomatic: state.cyclomatic_decisions.saturating_add(1),
        cognitive: state.cognitive,
        nloc: count_nloc(body, src),
        max_nesting: state.max_nesting,
        param_count: params.map(count_params).unwrap_or(0),
    };

    FunctionMetrics {
        start_byte: fn_node.start_byte(),
        end_byte: fn_node.end_byte(),
        start_line: fn_node.start_position().row as u32,
        end_line: fn_node.end_position().row as u32,
        name,
        metrics,
    }
}

#[derive(Default)]
struct WalkState {
    cognitive: u16,
    cyclomatic_decisions: u16,
    max_nesting: u8,
}

fn walk(node: Node, rules: &LangRules, src: &[u8], nesting: u8, state: &mut WalkState) {
    if state.max_nesting < nesting {
        state.max_nesting = nesting;
    }

    let kind = node.kind();

    // Don't descend into nested function declarations — they get their
    // own entry from `collect_functions`.
    if rules.function_kinds.contains(&kind) {
        return;
    }

    let mut child_nesting = nesting;

    let is_else_if = match (rules.if_kind, rules.else_if_parent_kind, node.parent()) {
        (Some(if_k), Some(parent_k), Some(parent)) => kind == if_k && parent.kind() == parent_k,
        _ => false,
    };

    if is_else_if {
        state.cognitive = state.cognitive.saturating_add(1);
        state.cyclomatic_decisions = state.cyclomatic_decisions.saturating_add(1);
        // The if's body still introduces nesting for things inside it.
        child_nesting = nesting.saturating_add(1);
    } else if rules.nest_increments.contains(&kind) {
        let cost = 1u16.saturating_add(nesting as u16);
        state.cognitive = state.cognitive.saturating_add(cost);
        state.cyclomatic_decisions = state.cyclomatic_decisions.saturating_add(1);
        child_nesting = nesting.saturating_add(1);
    } else if rules.nest_only.contains(&kind) {
        child_nesting = nesting.saturating_add(1);
    } else if rules.binary_kind == Some(kind) {
        if let Some(op) = binary_op(node, rules, src) {
            if rules.logical_ops.contains(&op.as_str()) {
                let parent_is_logical_binary = node
                    .parent()
                    .filter(|p| Some(p.kind()) == rules.binary_kind)
                    .and_then(|p| binary_op(p, rules, src))
                    .map(|po| rules.logical_ops.contains(&po.as_str()))
                    .unwrap_or(false);
                if !parent_is_logical_binary {
                    let switches = count_logical_switches(node, rules, src);
                    state.cognitive = state.cognitive.saturating_add(switches);
                    state.cyclomatic_decisions =
                        state.cyclomatic_decisions.saturating_add(switches);
                }
            }
        }
    }

    let mut walker = node.walk();
    for child in node.named_children(&mut walker) {
        walk(child, rules, src, child_nesting, state);
    }
}

pub(crate) fn binary_op(node: Node, rules: &LangRules, src: &[u8]) -> Option<String> {
    if let Some(field) = rules.operator_field {
        if let Some(op_node) = node.child_by_field_name(field) {
            if let Ok(t) = op_node.utf8_text(src) {
                return Some(t.to_string());
            }
        }
    }
    let mut walker = node.walk();
    for child in node.children(&mut walker) {
        if !child.is_named() {
            if let Ok(t) = child.utf8_text(src) {
                if rules.logical_ops.contains(&t) {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

fn count_logical_switches(node: Node, rules: &LangRules, src: &[u8]) -> u16 {
    let mut ops = Vec::<String>::new();
    walk_logical_chain(node, rules, src, &mut ops);
    if ops.is_empty() {
        return 0;
    }
    let mut switches = 1u16;
    for w in ops.windows(2) {
        if w[0] != w[1] {
            switches = switches.saturating_add(1);
        }
    }
    switches
}

fn walk_logical_chain(node: Node, rules: &LangRules, src: &[u8], out: &mut Vec<String>) {
    if rules.binary_kind != Some(node.kind()) {
        return;
    }
    let Some(op) = binary_op(node, rules, src) else {
        return;
    };
    if !rules.logical_ops.contains(&op.as_str()) {
        return;
    }
    if let Some(left) = node.child_by_field_name("left") {
        walk_logical_chain(left, rules, src, out);
    }
    out.push(op);
    if let Some(right) = node.child_by_field_name("right") {
        walk_logical_chain(right, rules, src, out);
    }
}

fn count_nloc(body: Node, src: &[u8]) -> u16 {
    let body_text = body.utf8_text(src).unwrap_or("");
    body_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
        .min(u16::MAX as usize) as u16
}

fn count_params(params_node: Node) -> u8 {
    let mut walker = params_node.walk();
    params_node
        .named_children(&mut walker)
        .count()
        .min(u8::MAX as usize) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust(source: &str) -> Vec<ComplexityMetrics> {
        ComplexityWalker::new()
            .analyze_file("rust", source)
            .into_iter()
            .map(|f| f.metrics)
            .collect()
    }

    #[test]
    fn empty_function_is_one_cyclo_zero_cog() {
        let m = &rust("fn f() {}")[0];
        assert_eq!(m.cyclomatic, 1);
        assert_eq!(m.cognitive, 0);
    }

    #[test]
    fn single_if() {
        let m = &rust("fn f(x: i32) { if x > 0 { } }")[0];
        assert_eq!(m.cognitive, 1);
        assert_eq!(m.cyclomatic, 2);
    }

    #[test]
    fn nested_if_grows_with_nesting() {
        let m = &rust("fn f(x: i32, y: i32) { if x > 0 { if y > 0 { } } }")[0];
        assert_eq!(m.cognitive, 1 + 2);
        assert_eq!(m.max_nesting, 2);
    }

    #[test]
    fn else_if_is_flat() {
        let m = &rust(
            "fn f(x: i32) { if x == 0 { } else if x == 1 { } else if x == 2 { } else { } }",
        )[0];
        assert_eq!(m.cognitive, 3);
    }

    #[test]
    fn logical_or_chain() {
        let m = &rust("fn f(a: bool, b: bool, c: bool) { if a || b || c { } }")[0];
        assert_eq!(m.cognitive, 2);
    }

    #[test]
    fn mixed_logical_ops() {
        let m = &rust("fn f(a: bool, b: bool, c: bool) { if a && b || c { } }")[0];
        assert_eq!(m.cognitive, 3);
    }

    #[test]
    fn parenthesized_logical_groups() {
        let m =
            &rust("fn f(a: bool, b: bool, c: bool, d: bool) { if a && (b || c) && d { } }")[0];
        assert_eq!(m.cognitive, 3);
    }

    #[test]
    fn match_is_one_increment() {
        let m = &rust("fn f(x: i32) -> i32 { match x { 0 => 0, 1 => 1, _ => 2 } }")[0];
        assert_eq!(m.cognitive, 1);
    }

    #[test]
    fn for_inside_if_doubles() {
        let m = &rust(
            "fn f(xs: &[i32]) { if !xs.is_empty() { for x in xs { let _ = x; } } }",
        )[0];
        assert_eq!(m.cognitive, 1 + 2);
    }

    #[test]
    fn closure_raises_nesting() {
        let m = &rust("fn f(xs: &[i32]) { xs.iter().for_each(|x| { if *x > 0 { } }); }")[0];
        assert_eq!(m.cognitive, 2);
    }

    #[test]
    fn nested_function_emits_separately() {
        let metrics = rust("fn outer() { fn inner(x: i32) { if x > 0 { } } }");
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].cognitive, 0);
        assert_eq!(metrics[1].cognitive, 1);
    }

    #[test]
    fn param_count() {
        let m = &rust("fn f(a: i32, b: &str, c: bool) { }")[0];
        assert_eq!(m.param_count, 3);
    }

    #[test]
    fn method_with_self_param_counts_self() {
        let metrics = rust("impl S { fn f(&self, a: i32) { } }");
        let m = &metrics[0];
        assert_eq!(m.param_count, 2);
    }

    #[test]
    fn deeply_nested_chains_correctly() {
        let src = r#"
            fn f(a: i32, b: i32) {
                if a > 0 {
                    if b > 0 {
                        for _ in 0..a {
                            if b == 1 {
                            }
                        }
                    }
                }
            }
        "#;
        let m = &rust(src)[0];
        // 1 + 2 + 3 + 4 = 10
        assert_eq!(m.cognitive, 10);
        assert_eq!(m.max_nesting, 4);
    }
}
