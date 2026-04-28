//! Tree-sitter–backed read/write access classifier for SCIP occurrences.
//!
//! SCIP indexers disagree about how to populate `symbol_roles` for read/write
//! contexts — rust-analyzer omits the bits entirely, scip-go is partial, and
//! every JS/TS indexer we have tried diverges. To get a language-uniform
//! signal we re-parse the file with tree-sitter and inspect the AST context
//! around the occurrence's identifier.
//!
//! This module is intentionally self-contained: it owns its parser pool and
//! a small per-file tree cache, exposes a single [`AccessClassifier::classify`]
//! entry point, and never panics on a position mismatch (always falls back to
//! [`AccessKind::Unknown`] so callers can keep their existing classification).
//!
//! The wire-up into `repo_graph.rs` lives in a follow-up change.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use tree_sitter::{Language, Node, Parser, Point, Tree};

/// Result of classifying a SCIP occurrence's read/write context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessKind {
    /// Pure read of the symbol (e.g. `let y = x`, `print(obj.attr)`).
    Read,
    /// Pure write — the value at this site is being replaced wholesale
    /// (`x = 1`, `obj.field = v`).
    Write,
    /// Both read and write at the same site (`x += 1`, `x++`). Mutation is
    /// the more load-bearing signal — the caller will collapse to Write.
    ReadWrite,
    /// Not an access (definition, import, type-only reference, etc.) —
    /// the caller should fall back to its existing classification.
    NotAnAccess,
    /// Couldn't classify (unknown language, AST mismatch, off-by-one
    /// against the SCIP range). Caller falls back.
    Unknown,
}

/// Identifies which tree-sitter grammar to drive for a given SCIP
/// `Document.language` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum LangKind {
    Rust,
    Go,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
}

impl LangKind {
    fn from_scip(lang: &str) -> Option<LangKind> {
        // SCIP's Document.language is a free-form string. Normalise to lower.
        let normalised = lang.trim().to_ascii_lowercase();
        match normalised.as_str() {
            "rust" => Some(LangKind::Rust),
            "go" => Some(LangKind::Go),
            "python" | "py" => Some(LangKind::Python),
            "typescript" | "ts" => Some(LangKind::TypeScript),
            "typescriptreact" | "tsx" => Some(LangKind::Tsx),
            "javascript" | "js" | "javascriptreact" | "jsx" => Some(LangKind::JavaScript),
            _ => None,
        }
    }

    fn tree_sitter_language(self) -> Language {
        match self {
            LangKind::Rust => tree_sitter_rust::LANGUAGE.into(),
            LangKind::Go => tree_sitter_go::LANGUAGE.into(),
            LangKind::Python => tree_sitter_python::LANGUAGE.into(),
            // TypeScript grammar handles both .ts and .js (the latter has
            // a strict subset of the AST shape we care about).
            LangKind::TypeScript | LangKind::JavaScript => {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            }
            LangKind::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }
}

const TREE_CACHE_CAPACITY: usize = 64;

/// Cache key derived from `(language, source-hash)`.
type TreeKey = (LangKind, u64);

struct CacheEntry {
    tree: Tree,
    /// Monotonic tick used to evict the least-recently-used entry.
    last_used: u64,
}

/// Stateful classifier — keeps one parser per language plus a small bounded
/// cache of recently-parsed trees. Cheap to construct; intended to be reused
/// across many SCIP occurrences in the same indexing pass.
pub struct AccessClassifier {
    parsers: BTreeMap<LangKind, Parser>,
    cache: BTreeMap<TreeKey, CacheEntry>,
    tick: u64,
    #[cfg(test)]
    parse_count: u64,
}

impl Default for AccessClassifier {
    fn default() -> Self {
        Self::new()
    }
}

impl AccessClassifier {
    pub fn new() -> Self {
        Self {
            parsers: BTreeMap::new(),
            cache: BTreeMap::new(),
            tick: 0,
            #[cfg(test)]
            parse_count: 0,
        }
    }

    /// Classify the access kind at `(line, character)` (0-indexed, matching
    /// SCIP's range encoding) inside `source` for the given language.
    pub fn classify(
        &mut self,
        language: &str,
        source: &str,
        line: u32,
        character: u32,
    ) -> AccessKind {
        let Some(lang) = LangKind::from_scip(language) else {
            return AccessKind::Unknown;
        };

        let tree = match self.tree_for(lang, source) {
            Some(t) => t,
            None => return AccessKind::Unknown,
        };
        let root = tree.root_node();

        let pt = Point {
            row: line as usize,
            column: character as usize,
        };
        let Some(mut node) = root.named_descendant_for_point_range(pt, pt) else {
            return AccessKind::Unknown;
        };

        // We only ever want to classify identifier-shaped leaves. If the
        // descendant lookup returned an ancestor node (because the position
        // landed on whitespace/punctuation), bail out rather than guess.
        if !is_identifier_kind(node.kind()) {
            return AccessKind::Unknown;
        }

        // Walk through transparent wrapper nodes (field access chains,
        // pattern lists, parens) up to the meaningful syntactic context.
        match lang {
            LangKind::Rust => classify_rust(&mut node),
            LangKind::Go => classify_go(&mut node),
            LangKind::Python => classify_python(&mut node),
            LangKind::TypeScript | LangKind::Tsx | LangKind::JavaScript => {
                classify_ts_like(&mut node)
            }
        }
    }

    fn tree_for(&mut self, lang: LangKind, source: &str) -> Option<&Tree> {
        let key = (lang, hash_source(source));

        // Cache hit fast-path.
        if self.cache.contains_key(&key) {
            self.tick = self.tick.wrapping_add(1);
            let entry = self.cache.get_mut(&key).expect("just-checked");
            entry.last_used = self.tick;
            return Some(&entry.tree);
        }

        // Miss — parse, then evict if we're over capacity.
        let parser = self
            .parsers
            .entry(lang)
            .or_insert_with(|| {
                let mut p = Parser::new();
                let _ = p.set_language(&lang.tree_sitter_language());
                p
            });
        let tree = parser.parse(source, None)?;
        #[cfg(test)]
        {
            self.parse_count += 1;
        }
        self.tick = self.tick.wrapping_add(1);
        self.cache.insert(
            key,
            CacheEntry {
                tree,
                last_used: self.tick,
            },
        );
        if self.cache.len() > TREE_CACHE_CAPACITY {
            self.evict_lru();
        }
        self.cache.get(&key).map(|e| &e.tree)
    }

    fn evict_lru(&mut self) {
        if let Some(victim_key) = self
            .cache
            .iter()
            .min_by_key(|(_, e)| e.last_used)
            .map(|(k, _)| *k)
        {
            self.cache.remove(&victim_key);
        }
    }

    /// Test-only accessor — number of parser invocations the cache has
    /// performed. Used to assert cache hits across calls.
    #[cfg(test)]
    fn parse_count(&self) -> u64 {
        self.parse_count
    }
}

fn hash_source(source: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut h);
    h.finish()
}

fn is_identifier_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "field_identifier"
            | "type_identifier"
            | "shorthand_property_identifier"
            | "property_identifier"
            | "shorthand_property_identifier_pattern"
    )
}

/// True when `child` is the node reachable from `parent` via the field name
/// `field`.
fn child_is_field<'a>(parent: Node<'a>, field: &str, child: Node<'a>) -> bool {
    parent
        .child_by_field_name(field)
        .map(|n| n.id() == child.id())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Rust
// ---------------------------------------------------------------------------

fn classify_rust(node: &mut Node) -> AccessKind {
    // Walk up through identifier-wrapping nodes (field_expression chains)
    // to the assignment / borrow context. We track the "current" node so we
    // can ask "is the current subtree the LHS of an assignment?".
    let mut current = *node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            // `pattern` of `let_declaration` — pure binding, not an access.
            "let_declaration" => {
                if child_is_field(parent, "pattern", current) {
                    return AccessKind::NotAnAccess;
                }
                // value side — keep climbing (it's a read of inner ids).
                return AccessKind::Read;
            }
            // Function/closure parameter binding sites are also pure defs.
            "parameter" | "closure_parameters" | "tuple_pattern"
            | "tuple_struct_pattern" | "struct_pattern" => {
                return AccessKind::NotAnAccess;
            }
            "assignment_expression" => {
                if child_is_field(parent, "left", current) {
                    return AccessKind::Write;
                }
                return AccessKind::Read;
            }
            "compound_assignment_expr" => {
                if child_is_field(parent, "left", current) {
                    // Per spec: collapse compound to Write at the call site,
                    // but report ReadWrite faithfully here.
                    return AccessKind::ReadWrite;
                }
                return AccessKind::Read;
            }
            // `&mut x` / `&mut self.field` — the borrow is mutating. Only
            // climb through `field_expression` when we are the "value"
            // side; otherwise we're a field name being looked up (read).
            "field_expression" => {
                if child_is_field(parent, "field", current) {
                    // identifier here is the field name — keep climbing
                    // because the field-expression as a whole could still
                    // be on the LHS of an assignment.
                    current = parent;
                    continue;
                }
                if child_is_field(parent, "value", current) {
                    current = parent;
                    continue;
                }
                return AccessKind::Read;
            }
            "reference_expression" => {
                // Mutability marker is a sibling token (not a named field).
                let mut mutable = false;
                let mut cursor = parent.walk();
                for child in parent.children(&mut cursor) {
                    if child.kind() == "mutable_specifier" {
                        mutable = true;
                        break;
                    }
                }
                if mutable {
                    return AccessKind::Write;
                }
                return AccessKind::Read;
            }
            // Transparent wrappers — keep climbing.
            "parenthesized_expression" | "scoped_identifier" | "type_arguments" => {
                current = parent;
                continue;
            }
            _ => return AccessKind::Read,
        }
    }
    AccessKind::Read
}

// ---------------------------------------------------------------------------
// Go
// ---------------------------------------------------------------------------

fn classify_go(node: &mut Node) -> AccessKind {
    let mut current = *node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            // Definition sites — function/method/parameter/short-var-decl
            // LHS positions are not "accesses" of an existing symbol.
            "short_var_declaration" => {
                if expression_list_contains(parent, "left", current) {
                    return AccessKind::NotAnAccess;
                }
                return AccessKind::Read;
            }
            "var_spec" | "const_spec" | "parameter_declaration" | "field_declaration"
            | "method_declaration" | "function_declaration" | "type_spec" => {
                // identifier appearing as the bound name is a definition.
                if child_is_field(parent, "name", current) {
                    return AccessKind::NotAnAccess;
                }
                return AccessKind::Read;
            }
            "assignment_statement" => {
                if expression_list_contains(parent, "left", current) {
                    // Compound assignments tag their operator field with the
                    // augmented token (`+=`, `-=`, …). Plain `=` is a pure
                    // write.
                    if let Some(op) = parent.child_by_field_name("operator") {
                        let op_text = op.kind();
                        if op_text == "=" {
                            return AccessKind::Write;
                        }
                        return AccessKind::ReadWrite;
                    }
                    return AccessKind::Write;
                }
                return AccessKind::Read;
            }
            "inc_statement" | "dec_statement" => {
                return AccessKind::ReadWrite;
            }
            // Selector (`x.Field`) — unwrap to the parent context. Field
            // identifiers themselves can be on the LHS of an assignment.
            "selector_expression" => {
                current = parent;
                continue;
            }
            "expression_list" | "parenthesized_expression" => {
                current = parent;
                continue;
            }
            _ => return AccessKind::Read,
        }
    }
    AccessKind::Read
}

/// Returns true when `target` is one of the children reachable through the
/// named `field` of `parent`, even if the field wraps an `expression_list`.
fn expression_list_contains<'a>(parent: Node<'a>, field: &str, target: Node<'a>) -> bool {
    let Some(field_node) = parent.child_by_field_name(field) else {
        return false;
    };
    if field_node.id() == target.id() {
        return true;
    }
    if field_node.kind() == "expression_list" {
        let mut cursor = field_node.walk();
        for child in field_node.named_children(&mut cursor) {
            if child.id() == target.id() {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------

fn classify_python(node: &mut Node) -> AccessKind {
    let mut current = *node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            // Definition sites — function/class/parameter names. Keep these
            // distinct from regular identifier uses.
            "function_definition" | "class_definition" => {
                if child_is_field(parent, "name", current) {
                    return AccessKind::NotAnAccess;
                }
                return AccessKind::Read;
            }
            "parameters" | "lambda_parameters" | "typed_parameter"
            | "default_parameter" | "typed_default_parameter" => {
                return AccessKind::NotAnAccess;
            }
            "assignment" => {
                if pattern_field_contains(parent, "left", current) {
                    return AccessKind::Write;
                }
                return AccessKind::Read;
            }
            "augmented_assignment" => {
                if pattern_field_contains(parent, "left", current) {
                    return AccessKind::ReadWrite;
                }
                return AccessKind::Read;
            }
            "for_statement" | "for_in_clause" => {
                if pattern_field_contains(parent, "left", current) {
                    return AccessKind::Write;
                }
                return AccessKind::Read;
            }
            "delete_statement" => return AccessKind::Write,
            // Attribute access (`obj.attr`). Only the OUTERMOST attribute
            // node represents the actual write target on the LHS of an
            // assignment; inner identifiers (`obj`) are reads.
            "attribute" => {
                if child_is_field(parent, "object", current) {
                    // We are the receiver — pure read regardless of the
                    // outer assignment context.
                    return AccessKind::Read;
                }
                if child_is_field(parent, "attribute", current) {
                    current = parent;
                    continue;
                }
                return AccessKind::Read;
            }
            "subscript" => {
                if child_is_field(parent, "value", current) {
                    return AccessKind::Read;
                }
                current = parent;
                continue;
            }
            "parenthesized_expression" => {
                current = parent;
                continue;
            }
            _ => return AccessKind::Read,
        }
    }
    AccessKind::Read
}

/// True when `target` is the LHS field of a Python assignment, including
/// when the field wraps a `pattern_list` / `tuple_pattern` (e.g. `a, b = …`).
fn pattern_field_contains<'a>(parent: Node<'a>, field: &str, target: Node<'a>) -> bool {
    let Some(field_node) = parent.child_by_field_name(field) else {
        return false;
    };
    if field_node.id() == target.id() {
        return true;
    }
    match field_node.kind() {
        "pattern_list" | "tuple_pattern" | "list_pattern" | "expression_list" => {
            let mut cursor = field_node.walk();
            for child in field_node.named_children(&mut cursor) {
                if child.id() == target.id() {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// TypeScript / JavaScript
// ---------------------------------------------------------------------------

fn classify_ts_like(node: &mut Node) -> AccessKind {
    let mut current = *node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "variable_declarator" | "function_declaration" | "function_expression"
            | "method_definition" | "class_declaration" | "formal_parameters"
            | "required_parameter" | "optional_parameter" => {
                if child_is_field(parent, "name", current) {
                    return AccessKind::NotAnAccess;
                }
                // Parameters' identifier-as-name lives directly under
                // formal_parameters without a field name in some shapes.
                if matches!(parent.kind(), "formal_parameters") {
                    return AccessKind::NotAnAccess;
                }
                return AccessKind::Read;
            }
            "assignment_expression" => {
                if child_is_field(parent, "left", current) {
                    return AccessKind::Write;
                }
                return AccessKind::Read;
            }
            "augmented_assignment_expression" => {
                if child_is_field(parent, "left", current) {
                    return AccessKind::ReadWrite;
                }
                return AccessKind::Read;
            }
            "update_expression" => {
                if child_is_field(parent, "argument", current) {
                    return AccessKind::ReadWrite;
                }
                return AccessKind::Read;
            }
            // Member expression `obj.prop`. The `object` side (`obj`) is a
            // pure read regardless of LHS context; the `property` side
            // inherits the surrounding write context — keep climbing.
            "member_expression" => {
                if child_is_field(parent, "object", current) {
                    return AccessKind::Read;
                }
                if child_is_field(parent, "property", current) {
                    current = parent;
                    continue;
                }
                return AccessKind::Read;
            }
            "subscript_expression" => {
                if child_is_field(parent, "object", current) {
                    return AccessKind::Read;
                }
                current = parent;
                continue;
            }
            "parenthesized_expression" => {
                current = parent;
                continue;
            }
            _ => return AccessKind::Read,
        }
    }
    AccessKind::Read
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Locate the (line, column) of the first occurrence of `needle` in
    /// `source`, returning the position of its first byte. Convenient for
    /// keeping test sources small without hard-coding offsets.
    fn locate(source: &str, needle: &str, occurrence: usize) -> (u32, u32) {
        let mut count = 0;
        for (idx, _) in source.match_indices(needle) {
            if count == occurrence {
                let prefix = &source[..idx];
                let line = prefix.matches('\n').count() as u32;
                let col = prefix.rfind('\n').map(|n| idx - n - 1).unwrap_or(idx) as u32;
                return (line, col);
            }
            count += 1;
        }
        panic!("needle {:?} not found at occurrence {}", needle, occurrence);
    }

    fn cls(lang: &str, src: &str, needle: &str, occurrence: usize) -> AccessKind {
        let (line, col) = locate(src, needle, occurrence);
        AccessClassifier::new().classify(lang, src, line, col)
    }

    // ---- Rust ----------------------------------------------------------

    #[test]
    fn rust_pure_read() {
        let src = "fn f() { let mut x = 0; let y = x + 1; }";
        // second `x` (the `+ 1` site)
        assert_eq!(cls("rust", src, "x", 1), AccessKind::Read);
    }

    #[test]
    fn rust_pure_write() {
        let src = "fn f() { let mut x = 0; x = 5; }";
        // second `x` is the LHS of `x = 5`
        assert_eq!(cls("rust", src, "x", 1), AccessKind::Write);
    }

    #[test]
    fn rust_compound_assignment_is_read_write() {
        let src = "fn f() { let mut x = 0; x += 5; }";
        assert_eq!(cls("rust", src, "x", 1), AccessKind::ReadWrite);
    }

    #[test]
    fn rust_let_binding_is_not_an_access() {
        let src = "fn f() { let x = 1; }";
        assert_eq!(cls("rust", src, "x", 0), AccessKind::NotAnAccess);
    }

    #[test]
    fn rust_mut_borrow_field_is_write() {
        let src = "fn f(s: &mut S) { let r = &mut s.field; }";
        // `field` (the field identifier inside &mut s.field)
        assert_eq!(cls("rust", src, "field", 0), AccessKind::Write);
    }

    // ---- Go ------------------------------------------------------------

    #[test]
    fn go_pure_read() {
        let src = "package m\nfunc f() { x := 0; y := x + 1; _ = y }";
        // third occurrence of `x` (decl, none, read)
        assert_eq!(cls("go", src, "x", 1), AccessKind::Read);
    }

    #[test]
    fn go_pure_write() {
        let src = "package m\nfunc f() { var x int; x = 5; _ = x }";
        // second occurrence — the `x = 5` LHS
        assert_eq!(cls("go", src, "x", 1), AccessKind::Write);
    }

    #[test]
    fn go_compound_assignment_is_read_write() {
        let src = "package m\nfunc f() { x := 0; x += 5; _ = x }";
        assert_eq!(cls("go", src, "x", 1), AccessKind::ReadWrite);
    }

    #[test]
    fn go_inc_statement_is_read_write() {
        let src = "package m\nfunc f() { x := 0; x++; _ = x }";
        assert_eq!(cls("go", src, "x", 1), AccessKind::ReadWrite);
    }

    #[test]
    fn go_short_var_decl_is_not_an_access() {
        let src = "package m\nfunc f() { x := 0; _ = x }";
        assert_eq!(cls("go", src, "x", 0), AccessKind::NotAnAccess);
    }

    // ---- Python --------------------------------------------------------

    #[test]
    fn python_pure_read() {
        let src = "x = 1\nprint(x)\n";
        // second occurrence — the `print(x)` arg
        assert_eq!(cls("python", src, "x", 1), AccessKind::Read);
    }

    #[test]
    fn python_pure_write() {
        let src = "x = 1\nx = 2\n";
        // second occurrence — the `x = 2` LHS
        assert_eq!(cls("python", src, "x", 1), AccessKind::Write);
    }

    #[test]
    fn python_augmented_assignment_is_read_write() {
        let src = "x = 0\nx += 1\n";
        assert_eq!(cls("python", src, "x", 1), AccessKind::ReadWrite);
    }

    #[test]
    fn python_for_target_is_write() {
        let src = "for x in [1, 2]:\n    pass\n";
        assert_eq!(cls("python", src, "x", 0), AccessKind::Write);
    }

    #[test]
    fn python_attribute_write() {
        let src = "obj.attr = 1\n";
        // `attr` identifier on the LHS
        assert_eq!(cls("python", src, "attr", 0), AccessKind::Write);
    }

    #[test]
    fn python_attribute_read() {
        let src = "y = obj.attr\n";
        assert_eq!(cls("python", src, "attr", 0), AccessKind::Read);
    }

    #[test]
    fn python_function_definition_is_not_an_access() {
        let src = "def foo():\n    pass\n";
        assert_eq!(cls("python", src, "foo", 0), AccessKind::NotAnAccess);
    }

    // ---- TypeScript / JavaScript --------------------------------------

    #[test]
    fn ts_pure_read() {
        let src = "let x = 1; console.log(x);";
        // second `x` — the log arg
        assert_eq!(cls("typescript", src, "x", 1), AccessKind::Read);
    }

    #[test]
    fn ts_pure_write() {
        let src = "let x = 1; x = 2;";
        assert_eq!(cls("typescript", src, "x", 1), AccessKind::Write);
    }

    #[test]
    fn ts_augmented_assignment_is_read_write() {
        let src = "let x = 1; x += 2;";
        assert_eq!(cls("typescript", src, "x", 1), AccessKind::ReadWrite);
    }

    #[test]
    fn ts_update_expression_is_read_write() {
        let src = "let x = 1; x++;";
        assert_eq!(cls("typescript", src, "x", 1), AccessKind::ReadWrite);
    }

    #[test]
    fn ts_member_property_write() {
        let src = "obj.field = 4;";
        assert_eq!(cls("typescript", src, "field", 0), AccessKind::Write);
    }

    #[test]
    fn ts_member_property_read() {
        let src = "let v = obj.field;";
        assert_eq!(cls("typescript", src, "field", 0), AccessKind::Read);
    }

    #[test]
    fn ts_variable_declarator_is_not_an_access() {
        let src = "let x = 1;";
        assert_eq!(cls("typescript", src, "x", 0), AccessKind::NotAnAccess);
    }

    #[test]
    fn javascript_uses_typescript_grammar() {
        let src = "let x = 1; x = 2;";
        assert_eq!(cls("javascript", src, "x", 1), AccessKind::Write);
    }

    #[test]
    fn tsx_classifies_assignment() {
        let src = "let x = 1; x = 2;";
        assert_eq!(cls("tsx", src, "x", 1), AccessKind::Write);
    }

    // ---- Edge cases ----------------------------------------------------

    #[test]
    fn unknown_language_returns_unknown() {
        let mut c = AccessClassifier::new();
        assert_eq!(
            c.classify("brainfuck", "+++>+++", 0, 0),
            AccessKind::Unknown
        );
    }

    #[test]
    fn out_of_range_position_returns_unknown() {
        let mut c = AccessClassifier::new();
        // Source has 1 line; ask far past it.
        assert_eq!(
            c.classify("rust", "fn f() {}", 999, 0),
            AccessKind::Unknown
        );
    }

    #[test]
    fn position_on_punctuation_returns_unknown() {
        let mut c = AccessClassifier::new();
        // Column 7 is the `(` in `fn f() {}` — not an identifier.
        assert_eq!(c.classify("rust", "fn f() {}", 0, 5), AccessKind::Unknown);
    }

    #[test]
    fn parser_pool_caches_across_calls() {
        let mut c = AccessClassifier::new();
        let src = "fn f() { let mut x = 0; x = 5; let y = x; }";
        let _ = c.classify("rust", src, 0, 17);
        assert_eq!(c.parse_count(), 1);
        let _ = c.classify("rust", src, 0, 24);
        assert_eq!(c.parse_count(), 1, "second call must hit the tree cache");
        // A different source string must trigger a fresh parse.
        let other = "fn g() {}";
        let _ = c.classify("rust", other, 0, 0);
        assert_eq!(c.parse_count(), 2);
    }
}
