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
        let parser = self.parsers.entry(lang).or_insert_with(|| {
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

/// Binding-site predicates for Rust.
fn rust_is_binding_site(parent: Node, current: Node) -> Option<AccessKind> {
    match parent.kind() {
        "let_declaration" => {
            if child_is_field(parent, "pattern", current) {
                return Some(AccessKind::NotAnAccess);
            }
            Some(AccessKind::Read)
        }
        "parameter"
        | "closure_parameters"
        | "tuple_pattern"
        | "tuple_struct_pattern"
        | "struct_pattern" => Some(AccessKind::NotAnAccess),
        _ => None,
    }
}

/// Assignment/compound-assignment classification for Rust.
fn rust_assignment_kind(parent: Node, current: Node) -> Option<AccessKind> {
    match parent.kind() {
        "assignment_expression" => {
            if child_is_field(parent, "left", current) {
                return Some(AccessKind::Write);
            }
            Some(AccessKind::Read)
        }
        "compound_assignment_expr" => {
            if child_is_field(parent, "left", current) {
                return Some(AccessKind::ReadWrite);
            }
            Some(AccessKind::Read)
        }
        _ => None,
    }
}

/// `&mut x` / `&mut self.field` — the borrow is mutating.
fn rust_reference_kind(parent: Node) -> Option<AccessKind> {
    if parent.kind() != "reference_expression" {
        return None;
    }
    let mut cursor = parent.walk();
    let mutable = parent
        .children(&mut cursor)
        .any(|c| c.kind() == "mutable_specifier");
    Some(if mutable {
        AccessKind::Write
    } else {
        AccessKind::Read
    })
}

/// Transparent wrappers for Rust — climb through these.
fn rust_is_transparent_wrapper(kind: &str) -> bool {
    matches!(
        kind,
        "parenthesized_expression" | "scoped_identifier" | "type_arguments"
    )
}

/// Field expression handling for Rust.
/// Returns `Some(None)` when we should keep climbing (current becomes parent),
/// `Some(Some(kind))` when we have a definitive classification,
/// `None` when this parent is not a field_expression.
fn rust_field_expression_step(parent: Node, current: Node) -> Option<Option<AccessKind>> {
    if parent.kind() != "field_expression" {
        return None;
    }
    if child_is_field(parent, "field", current) || child_is_field(parent, "value", current) {
        return Some(None);
    }
    Some(Some(AccessKind::Read))
}

fn classify_rust(node: &mut Node) -> AccessKind {
    let mut current = *node;
    while let Some(parent) = current.parent() {
        if let Some(kind) = rust_is_binding_site(parent, current) {
            return kind;
        }
        if let Some(kind) = rust_assignment_kind(parent, current) {
            return kind;
        }
        if let Some(step) = rust_field_expression_step(parent, current) {
            match step {
                Some(kind) => return kind,
                None => {
                    current = parent;
                    continue;
                }
            }
        }
        if let Some(kind) = rust_reference_kind(parent) {
            return kind;
        }
        if rust_is_transparent_wrapper(parent.kind()) {
            current = parent;
            continue;
        }
        return AccessKind::Read;
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
            "var_spec"
            | "const_spec"
            | "parameter_declaration"
            | "field_declaration"
            | "method_declaration"
            | "function_declaration"
            | "type_spec" => {
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
            "parameters"
            | "lambda_parameters"
            | "typed_parameter"
            | "default_parameter"
            | "typed_default_parameter" => {
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

/// Declaration / name-context predicates for TS-like languages.
fn ts_is_declaration_site(parent: Node, current: Node) -> Option<AccessKind> {
    match parent.kind() {
        "variable_declarator"
        | "function_declaration"
        | "function_expression"
        | "method_definition"
        | "class_declaration"
        | "required_parameter"
        | "optional_parameter" => {
            if child_is_field(parent, "name", current) {
                return Some(AccessKind::NotAnAccess);
            }
            Some(AccessKind::Read)
        }
        "formal_parameters" | "array_pattern" => Some(AccessKind::NotAnAccess),
        _ => None,
    }
}

/// Assignment / augmented-assignment / update-expression classification for TS-like languages.
fn ts_assignment_kind(parent: Node, current: Node) -> Option<AccessKind> {
    match parent.kind() {
        "assignment_expression" => {
            if child_is_field(parent, "left", current) {
                return Some(AccessKind::Write);
            }
            Some(AccessKind::Read)
        }
        "augmented_assignment_expression" => {
            if child_is_field(parent, "left", current) {
                return Some(AccessKind::ReadWrite);
            }
            Some(AccessKind::Read)
        }
        "update_expression" => {
            if child_is_field(parent, "argument", current) {
                return Some(AccessKind::ReadWrite);
            }
            Some(AccessKind::Read)
        }
        _ => None,
    }
}

/// Member / subscript expression handling for TS-like languages.
/// Returns `Some(None)` when we should keep climbing,
/// `Some(Some(kind))` when we have a definitive classification,
/// `None` when this parent is not a member/subscript expression.
fn ts_member_subscript_step(parent: Node, current: Node) -> Option<Option<AccessKind>> {
    match parent.kind() {
        "member_expression" => {
            if child_is_field(parent, "object", current) {
                return Some(Some(AccessKind::Read));
            }
            if child_is_field(parent, "property", current) {
                return Some(None);
            }
            Some(Some(AccessKind::Read))
        }
        "subscript_expression" => {
            if child_is_field(parent, "object", current) {
                return Some(Some(AccessKind::Read));
            }
            Some(None)
        }
        _ => None,
    }
}

/// Transparent wrappers for TS-like languages — climb through these.
fn ts_is_transparent_wrapper(kind: &str) -> bool {
    kind == "parenthesized_expression"
}

fn classify_ts_like(node: &mut Node) -> AccessKind {
    let mut current = *node;
    while let Some(parent) = current.parent() {
        if let Some(kind) = ts_is_declaration_site(parent, current) {
            return kind;
        }
        if let Some(kind) = ts_assignment_kind(parent, current) {
            return kind;
        }
        if let Some(step) = ts_member_subscript_step(parent, current) {
            match step {
                Some(kind) => return kind,
                None => {
                    current = parent;
                    continue;
                }
            }
        }
        if ts_is_transparent_wrapper(parent.kind()) {
            current = parent;
            continue;
        }
        return AccessKind::Read;
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
        for (count, (idx, _)) in source.match_indices(needle).enumerate() {
            if count == occurrence {
                let prefix = &source[..idx];
                let line = prefix.bytes().filter(|&b| b == b'\n').count() as u32;
                let column = prefix
                    .rsplit_once('\n')
                    .map(|(_, line)| line.len())
                    .unwrap_or(prefix.len()) as u32;
                return (line, column);
            }
        }
        panic!("needle {needle:?} occurrence {occurrence} not found in source");
    }

    #[test]
    fn rust_pure_read() {
        let mut c = AccessClassifier::new();
        let src = "fn main() { let x = 1; let y = x; }\n";
        let (line, col) = locate(src, "x", 1); // second occurrence (the read)
        assert_eq!(c.classify("rust", src, line, col), AccessKind::Read);
    }

    #[test]
    fn rust_pure_write() {
        let mut c = AccessClassifier::new();
        let src = "fn main() { let mut x = 1; x = 2; }\n";
        let (line, col) = locate(src, "x", 1); // second occurrence (the write)
        assert_eq!(c.classify("rust", src, line, col), AccessKind::Write);
    }

    #[test]
    fn rust_compound_assignment_is_read_write() {
        let mut c = AccessClassifier::new();
        let src = "fn main() { let mut x = 1; x += 1; }\n";
        let (line, col) = locate(src, "x", 1); // second occurrence (the compound)
        assert_eq!(c.classify("rust", src, line, col), AccessKind::ReadWrite);
    }

    #[test]
    fn rust_let_binding_is_not_an_access() {
        let mut c = AccessClassifier::new();
        let src = "fn main() { let x = 1; }\n";
        let (line, col) = locate(src, "x", 0);
        assert_eq!(c.classify("rust", src, line, col), AccessKind::NotAnAccess);
    }

    #[test]
    fn rust_mut_borrow_field_is_write() {
        let mut c = AccessClassifier::new();
        let src = "fn main() { let mut s = S::default(); let r = &mut s.field; }\n";
        let (line, col) = locate(src, "field", 0);
        assert_eq!(c.classify("rust", src, line, col), AccessKind::Write);
    }

    #[test]
    fn go_pure_read() {
        let mut c = AccessClassifier::new();
        let src = "package main\nfunc main() { x := 1; y := x }\n";
        let (line, col) = locate(src, "x", 1);
        assert_eq!(c.classify("go", src, line, col), AccessKind::Read);
    }

    #[test]
    fn go_pure_write() {
        let mut c = AccessClassifier::new();
        let src = "package main\nfunc main() { x := 1; x = 2 }\n";
        let (line, col) = locate(src, "x", 1);
        assert_eq!(c.classify("go", src, line, col), AccessKind::Write);
    }

    #[test]
    fn go_compound_assignment_is_read_write() {
        let mut c = AccessClassifier::new();
        let src = "package main\nfunc main() { x := 1; x += 1 }\n";
        let (line, col) = locate(src, "x", 1);
        assert_eq!(c.classify("go", src, line, col), AccessKind::ReadWrite);
    }

    #[test]
    fn go_inc_statement_is_read_write() {
        let mut c = AccessClassifier::new();
        let src = "package main\nfunc main() { x := 1; x++ }\n";
        let (line, col) = locate(src, "x", 1);
        assert_eq!(c.classify("go", src, line, col), AccessKind::ReadWrite);
    }

    #[test]
    fn go_short_var_decl_is_not_an_access() {
        let mut c = AccessClassifier::new();
        let src = "package main\nfunc main() { x := 1 }\n";
        let (line, col) = locate(src, "x", 0);
        assert_eq!(c.classify("go", src, line, col), AccessKind::NotAnAccess);
    }

    #[test]
    fn python_pure_read() {
        let mut c = AccessClassifier::new();
        let src = "x = 1\ny = x\n";
        let (line, col) = locate(src, "x", 1);
        assert_eq!(c.classify("python", src, line, col), AccessKind::Read);
    }

    #[test]
    fn python_pure_write() {
        let mut c = AccessClassifier::new();
        let src = "x = 1\nx = 2\n";
        let (line, col) = locate(src, "x", 1);
        assert_eq!(c.classify("python", src, line, col), AccessKind::Write);
    }

    #[test]
    fn python_augmented_assignment_is_read_write() {
        let mut c = AccessClassifier::new();
        let src = "x = 1\nx += 1\n";
        let (line, col) = locate(src, "x", 1);
        assert_eq!(c.classify("python", src, line, col), AccessKind::ReadWrite);
    }

    #[test]
    fn python_for_target_is_write() {
        let mut c = AccessClassifier::new();
        let src = "for x in range(10): pass\n";
        let (line, col) = locate(src, "x", 0);
        assert_eq!(c.classify("python", src, line, col), AccessKind::Write);
    }

    #[test]
    fn python_function_definition_is_not_an_access() {
        let mut c = AccessClassifier::new();
        let src = "def foo(): pass\n";
        let (line, col) = locate(src, "foo", 0);
        assert_eq!(
            c.classify("python", src, line, col),
            AccessKind::NotAnAccess
        );
    }

    #[test]
    fn python_attribute_read() {
        let mut c = AccessClassifier::new();
        let src = "obj = object()\nprint(obj.attr)\n";
        let (line, col) = locate(src, "attr", 0);
        assert_eq!(c.classify("python", src, line, col), AccessKind::Read);
    }

    #[test]
    fn python_attribute_write() {
        let mut c = AccessClassifier::new();
        let src = "obj = object()\nobj.attr = 1\n";
        let (line, col) = locate(src, "attr", 0);
        assert_eq!(c.classify("python", src, line, col), AccessKind::Write);
    }

    #[test]
    fn ts_pure_read() {
        let mut c = AccessClassifier::new();
        let src = "const x = 1; const y = x;\n";
        let (line, col) = locate(src, "x", 1);
        assert_eq!(c.classify("typescript", src, line, col), AccessKind::Read);
    }

    #[test]
    fn ts_pure_write() {
        let mut c = AccessClassifier::new();
        let src = "let x = 1; x = 2;\n";
        let (line, col) = locate(src, "x", 1);
        assert_eq!(c.classify("typescript", src, line, col), AccessKind::Write);
    }

    #[test]
    fn ts_augmented_assignment_is_read_write() {
        let mut c = AccessClassifier::new();
        let src = "let x = 1; x += 1;\n";
        let (line, col) = locate(src, "x", 1);
        assert_eq!(
            c.classify("typescript", src, line, col),
            AccessKind::ReadWrite
        );
    }

    #[test]
    fn ts_update_expression_is_read_write() {
        let mut c = AccessClassifier::new();
        let src = "let x = 1; x++;\n";
        let (line, col) = locate(src, "x", 1);
        assert_eq!(
            c.classify("typescript", src, line, col),
            AccessKind::ReadWrite
        );
    }

    #[test]
    fn ts_variable_declarator_is_not_an_access() {
        let mut c = AccessClassifier::new();
        let src = "const x = 1;\n";
        let (line, col) = locate(src, "x", 0);
        assert_eq!(
            c.classify("typescript", src, line, col),
            AccessKind::NotAnAccess
        );
    }

    #[test]
    fn ts_member_property_read() {
        let mut c = AccessClassifier::new();
        let src = "const obj = { a: 1 }; console.log(obj.a);\n";
        let (line, col) = locate(src, "a", 1);
        assert_eq!(c.classify("typescript", src, line, col), AccessKind::Read);
    }

    #[test]
    fn ts_member_property_write() {
        let mut c = AccessClassifier::new();
        let src = "const obj = { a: 1 }; obj.a = 2;\n";
        let (line, col) = locate(src, "a", 1);
        assert_eq!(c.classify("typescript", src, line, col), AccessKind::Write);
    }

    #[test]
    fn tsx_classifies_assignment() {
        let mut c = AccessClassifier::new();
        let src = "const [x, setX] = useState(0);\n";
        let (line, col) = locate(src, "x", 0);
        assert_eq!(c.classify("tsx", src, line, col), AccessKind::NotAnAccess);
    }

    #[test]
    fn javascript_uses_typescript_grammar() {
        let mut c = AccessClassifier::new();
        let src = "var x = 1; x = 2;\n";
        let (line, col) = locate(src, "x", 1);
        assert_eq!(c.classify("javascript", src, line, col), AccessKind::Write);
    }

    #[test]
    fn unknown_language_returns_unknown() {
        let mut c = AccessClassifier::new();
        assert_eq!(c.classify("brainfuck", "", 0, 0), AccessKind::Unknown);
    }

    #[test]
    fn out_of_range_position_returns_unknown() {
        let mut c = AccessClassifier::new();
        let src = "fn main() {}\n";
        assert_eq!(c.classify("rust", src, 100, 0), AccessKind::Unknown);
    }

    #[test]
    fn position_on_punctuation_returns_unknown() {
        let mut c = AccessClassifier::new();
        let src = "fn main() {}\n";
        // Position on the opening brace — not an identifier.
        assert_eq!(c.classify("rust", src, 0, 10), AccessKind::Unknown);
    }

    #[test]
    fn parser_pool_caches_across_calls() {
        let mut c = AccessClassifier::new();
        let src = "fn main() { let x = 1; }\n";
        let (line, col) = locate(src, "x", 0);
        // First call parses.
        assert_eq!(c.classify("rust", src, line, col), AccessKind::NotAnAccess);
        assert_eq!(c.parse_count(), 1);
        // Second call on identical (lang, source) hits the cache.
        assert_eq!(c.classify("rust", src, line, col), AccessKind::NotAnAccess);
        assert_eq!(c.parse_count(), 1);
    }
}
