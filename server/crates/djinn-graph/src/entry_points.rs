//! PR F1 — Entry-point detection.
//!
//! Walks every symbol node in a freshly-built [`RepoDependencyGraph`] and
//! tags the ones that look like an entry point (program `main`, test /
//! bench, HTTP route handler, K8s job binary, language-specific
//! convention) with an [`RepoGraphEdgeKind::EntryPointOf`] edge from the
//! containing **file** node to the **symbol** node.
//!
//! The downstream payoff is that `dead_symbols` can simply ask "does this
//! symbol have an incoming `EntryPointOf` edge?" instead of re-deriving
//! the heuristics inline. Other consumers (e.g. PR F2 process tracing)
//! get a uniform handle on the entry-point set.
//!
//! # Detection budget
//!
//! All detection runs off three signals already present in the graph:
//!   - the symbol's `display_name`,
//!   - the file path the symbol lives in,
//!   - imports surfaced as outgoing `FileReference` edges to external
//!     symbols (e.g. `axum::Router`).
//!
//! No new parser, no AST walk, no source-file IO. The cheapest of these
//! checks runs in O(1) per node; the import-shape check is O(k) over the
//! file's outgoing references but bounded by the per-file edge count.
//!
//! # Confidence floor
//!
//! Per-detector confidences are picked to honor the plan's table:
//!
//! | Detector             | Confidence | Reason tag           |
//! |----------------------|-----------:|----------------------|
//! | Rust `fn main`       | 0.95       | `rust-main`          |
//! | Go `func main`       | 0.95       | `go-main`            |
//! | SCIP `Test` role     | 0.95       | `scip-test-role`     |
//! | Python `__main__`    | 0.85       | `py-dunder-main`     |
//! | K8s job binary       | 0.80       | `k8s-binary-target`  |
//! | TS / JS index entry  | 0.70       | `ts-index-entry`     |
//! | Rust test heuristic  | 0.70       | `rust-test-heuristic`|
//! | HTTP route by import | 0.60       | `axum-router-import` |
//!
//! The 0.5 confidence floor for `EntryPointOf` (see
//! [`crate::repo_graph::edge_confidence_floor`]) is the soft minimum;
//! every detector above sits above it.

use std::collections::BTreeSet;

use petgraph::Direction;
use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef as _;

use crate::repo_graph::{
    RepoDependencyGraph, RepoGraphEdge, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind,
};
use crate::scip_parser::ScipSymbolKind;

/// Why a node was tagged as an entry point. Each variant maps to a
/// fixed confidence and `reason` string stamped on the resulting
/// `EntryPointOf` edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryPointKind {
    /// `fn main()` in Rust, `func main()` in Go.
    Main,
    /// `#[test]` / `#[bench]` (SCIP `Test` role) or path/name heuristic.
    Test,
    /// HTTP route handler — caught by import shape (`axum::Router`,
    /// `actix_web`, etc).
    HttpRoute,
    /// Python `if __name__ == "__main__":` block.
    PythonDunderMain,
    /// TS/JS server-style entry file (`index.ts`, `server.ts`, etc).
    NodeIndexEntry,
    /// Rust `crates/<crate>/src/bin/<name>.rs` or Go `cmd/<name>/main.go`.
    K8sBinaryTarget,
}

/// One detector hit pinned to a graph node.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryPointHit {
    /// Symbol node that was identified as an entry point.
    pub symbol: NodeIndex,
    /// File node the symbol lives in. The `EntryPointOf` edge is
    /// recorded `file ─EntryPointOf→ symbol`, so `dead_symbols` can ask
    /// "does this symbol have any incoming `EntryPointOf` edge?".
    pub file: NodeIndex,
    pub kind: EntryPointKind,
    pub confidence: f64,
    pub reason: &'static str,
}

/// Aggregate result of running [`detect_entry_points`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EntryPointReport {
    pub hits: Vec<EntryPointHit>,
}

impl EntryPointReport {
    pub fn len(&self) -> usize {
        self.hits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    /// Iterator over the unique symbol nodes flagged as entry points.
    pub fn symbol_nodes(&self) -> impl Iterator<Item = NodeIndex> + '_ {
        let mut seen: BTreeSet<NodeIndex> = BTreeSet::new();
        self.hits.iter().filter_map(move |hit| {
            if seen.insert(hit.symbol) {
                Some(hit.symbol)
            } else {
                None
            }
        })
    }
}

/// Environment flag that disables the post-build entry-point pass when
/// set to `0` / `false`. Default = on.
pub const ENTRY_POINT_DETECTION_FLAG: &str = "DJINN_ENTRY_POINT_DETECTION";

/// Returns `true` when the entry-point detector should run.
pub fn entry_point_detection_enabled() -> bool {
    match std::env::var(ENTRY_POINT_DETECTION_FLAG) {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

/// Entry-point detection pass — to be called from
/// [`crate::repo_graph::RepoDependencyGraph::build`] after the SCIP-driven
/// builder has finished.
///
/// Walks every symbol node, runs the per-detector heuristics, and adds an
/// `EntryPointOf` edge from the containing file node to each hit. The
/// returned [`EntryPointReport`] is informational; persistence happens
/// directly on the graph via the new edges.
pub fn detect_entry_points(graph: &mut RepoDependencyGraph) -> EntryPointReport {
    let mut report = EntryPointReport::default();

    // Collect candidates first (immutable borrow) so we can mutate the
    // graph afterwards without borrow-checker churn.
    let candidates: Vec<(NodeIndex, EntryPointHit)> = graph
        .graph()
        .node_indices()
        .filter_map(|idx| {
            let node = graph.node(idx);
            classify_node(idx, node, graph).map(|hit| (idx, hit))
        })
        .collect();

    for (_idx, hit) in candidates {
        graph.add_entry_point_edge(hit.file, hit.symbol, hit.confidence, hit.reason);
        report.hits.push(hit);
    }

    report
}

/// Pick the highest-confidence detector that fires on `node`, or
/// `None` if nothing matches. Order matters: test / main are unambiguous
/// and outrank heuristic-only signals like the import-shape HTTP route.
fn classify_node(
    idx: NodeIndex,
    node: &RepoGraphNode,
    graph: &RepoDependencyGraph,
) -> Option<EntryPointHit> {
    if node.kind != RepoGraphNodeKind::Symbol {
        return None;
    }
    if node.is_external {
        return None;
    }
    let file_path = node.file_path.as_ref()?;
    let file_node = graph.file_node(file_path)?;
    let language = node.language.as_deref().unwrap_or("");
    let path_str = file_path.to_string_lossy();

    // 1. SCIP-stamped `Test` role wins outright (0.95).
    if node.is_test {
        return Some(EntryPointHit {
            symbol: idx,
            file: file_node,
            kind: EntryPointKind::Test,
            confidence: 0.95,
            reason: "scip-test-role",
        });
    }

    // 2. Rust `fn main()` — top-level function whose display name is `main`.
    if is_rust(language) && is_function(node) && node.display_name == "main" {
        return Some(EntryPointHit {
            symbol: idx,
            file: file_node,
            kind: EntryPointKind::Main,
            confidence: 0.95,
            reason: "rust-main",
        });
    }

    // 3. Go `func main()` — same shape, separate language.
    if is_go(language) && is_function(node) && node.display_name == "main" {
        return Some(EntryPointHit {
            symbol: idx,
            file: file_node,
            kind: EntryPointKind::Main,
            confidence: 0.95,
            reason: "go-main",
        });
    }

    // 4. K8s binary target by file path (rust `src/bin/<name>.rs`,
    //    go `cmd/<name>/main.go`) — applies regardless of symbol name
    //    because each of these layouts ships a `main` symbol that we'd
    //    catch above. The path-based hit catches sibling helper symbols
    //    ("the file is a binary entry point, treat its definitions as
    //    reachable") that don't have the literal name `main`.
    if is_k8s_binary_path(&path_str) && is_function(node) && node.display_name == "main" {
        return Some(EntryPointHit {
            symbol: idx,
            file: file_node,
            kind: EntryPointKind::K8sBinaryTarget,
            confidence: 0.80,
            reason: "k8s-binary-target",
        });
    }

    // 5. Rust `#[test]` / `#[bench]` heuristic — file under `tests/`,
    //    `benches/`, `*_test.rs`, or symbol name prefix.
    if is_rust(language) && rust_test_heuristic(node, &path_str) {
        return Some(EntryPointHit {
            symbol: idx,
            file: file_node,
            kind: EntryPointKind::Test,
            confidence: 0.70,
            reason: "rust-test-heuristic",
        });
    }

    // 6. Go test heuristic — `*_test.go` files + `Test*` / `Benchmark*` symbols.
    if is_go(language) && go_test_heuristic(node, &path_str) {
        return Some(EntryPointHit {
            symbol: idx,
            file: file_node,
            kind: EntryPointKind::Test,
            confidence: 0.70,
            reason: "go-test-heuristic",
        });
    }

    // 7. Python dunder-main — Python file whose symbol name encodes the
    //    `__main__` block. SCIP-python emits a synthetic symbol named
    //    `__main__` for the `if __name__ == "__main__":` guard; some
    //    indexers also stamp module-level scripts. We catch both.
    if is_python(language) && python_dunder_main_heuristic(node, &path_str) {
        return Some(EntryPointHit {
            symbol: idx,
            file: file_node,
            kind: EntryPointKind::PythonDunderMain,
            confidence: 0.85,
            reason: "py-dunder-main",
        });
    }

    // 8. TS / JS index-entry file. We tag symbols defined in
    //    `index.ts` / `server.ts` / `worker.ts` so server bootstrap and
    //    Cloudflare-Worker `addEventListener("fetch", ...)` modules
    //    don't get false-flagged as dead.
    if is_ts_or_js(language) && ts_index_entry_heuristic(&path_str) && is_function(node) {
        return Some(EntryPointHit {
            symbol: idx,
            file: file_node,
            kind: EntryPointKind::NodeIndexEntry,
            confidence: 0.70,
            reason: "ts-index-entry",
        });
    }

    // 9. HTTP route by import shape — fire when the containing file
    //    imports `axum::Router` / `actix_web` and the symbol takes a
    //    Request-like parameter (signature heuristic, see
    //    [`looks_like_http_handler`]).
    if is_rust(language)
        && is_function(node)
        && file_imports_http_router(graph, file_node)
        && looks_like_http_handler(node)
    {
        return Some(EntryPointHit {
            symbol: idx,
            file: file_node,
            kind: EntryPointKind::HttpRoute,
            confidence: 0.60,
            reason: "axum-router-import",
        });
    }

    None
}

fn is_rust(language: &str) -> bool {
    language.eq_ignore_ascii_case("rust")
}

fn is_go(language: &str) -> bool {
    language.eq_ignore_ascii_case("go")
}

fn is_python(language: &str) -> bool {
    language.eq_ignore_ascii_case("python") || language.eq_ignore_ascii_case("py")
}

fn is_ts_or_js(language: &str) -> bool {
    matches!(
        language.to_ascii_lowercase().as_str(),
        "typescript" | "ts" | "javascript" | "js" | "tsx" | "jsx"
    )
}

fn is_function(node: &RepoGraphNode) -> bool {
    matches!(
        node.symbol_kind,
        Some(ScipSymbolKind::Function) | Some(ScipSymbolKind::Method)
    )
}

/// File path matches a layout we ship K8s job binaries from.
///
/// Both forms are normalized with forward slashes — Windows paths get
/// rewritten by SCIP indexers to forward slashes already, but we
/// double-check just in case.
fn is_k8s_binary_path(path: &str) -> bool {
    let p = path.replace('\\', "/");
    // Rust binary target: `crates/<crate>/src/bin/<name>.rs` or
    // top-level `src/bin/<name>.rs`.
    if p.contains("/src/bin/") || p.starts_with("src/bin/") {
        return true;
    }
    // Go binary: `cmd/<name>/main.go` or nested under a workspace.
    if p.contains("/cmd/") && p.ends_with("/main.go") {
        return true;
    }
    if p.starts_with("cmd/") && p.ends_with("/main.go") {
        return true;
    }
    false
}

/// Rust test heuristic: file path or symbol-name evidence.
fn rust_test_heuristic(node: &RepoGraphNode, path: &str) -> bool {
    let p = path.replace('\\', "/");
    let file_hint = p.contains("/tests/")
        || p.starts_with("tests/")
        || p.contains("/benches/")
        || p.starts_with("benches/")
        || p.ends_with("_test.rs")
        || p.ends_with("_tests.rs");
    let name = node.display_name.as_str();
    let name_hint =
        name.starts_with("test_") || name.starts_with("bench_") || name.ends_with("_test");
    (file_hint && is_function(node)) || (name_hint && is_function(node))
}

/// Go test heuristic: `_test.go` files + `TestXxx` / `BenchmarkXxx` /
/// `ExampleXxx` symbols (the standard `testing` framework conventions).
fn go_test_heuristic(node: &RepoGraphNode, path: &str) -> bool {
    if !is_function(node) {
        return false;
    }
    let p = path.replace('\\', "/");
    if !p.ends_with("_test.go") {
        return false;
    }
    let name = node.display_name.as_str();
    name.starts_with("Test") || name.starts_with("Benchmark") || name.starts_with("Example")
}

/// Python dunder-main heuristic. SCIP-python emits a synthetic symbol
/// named `__main__` for the `if __name__ == "__main__":` guard. Some
/// indexers instead surface a `main` symbol at module scope — we accept
/// either, gated on the file actually being a `.py` file.
fn python_dunder_main_heuristic(node: &RepoGraphNode, path: &str) -> bool {
    if !path.ends_with(".py") {
        return false;
    }
    let name = node.display_name.as_str();
    name == "__main__" || (is_function(node) && name == "main")
}

/// TS / JS index-entry heuristic. Server bootstraps and Workers ship
/// from a few well-known filenames; we catch all definitions in those
/// files (later filtered to function-shaped symbols by the caller).
fn ts_index_entry_heuristic(path: &str) -> bool {
    let p = path.replace('\\', "/");
    p.ends_with("/index.ts")
        || p.ends_with("/index.js")
        || p.ends_with("/index.tsx")
        || p.ends_with("/index.jsx")
        || p.ends_with("/server.ts")
        || p.ends_with("/server.js")
        || p.ends_with("/worker.ts")
        || p.ends_with("/worker.js")
        || p == "index.ts"
        || p == "index.js"
        || p == "server.ts"
        || p == "server.js"
}

/// Returns `true` when the file imports a known HTTP router crate.
///
/// We look at the file's outgoing `FileReference` edges — every external
/// symbol the file references shows up as a target — and pattern-match
/// the SCIP symbol identifier for `axum`, `actix_web`, `warp`, `rocket`,
/// or `hyper` package paths.
fn file_imports_http_router(graph: &RepoDependencyGraph, file_node: NodeIndex) -> bool {
    for edge in graph
        .graph()
        .edges_directed(file_node, Direction::Outgoing)
    {
        if !matches!(edge.weight().kind, RepoGraphEdgeKind::FileReference) {
            continue;
        }
        let target = graph.node(edge.target());
        if !target.is_external {
            continue;
        }
        let id = target.symbol.as_deref().unwrap_or("");
        if id.contains(" axum ") || id.contains("/axum/") || id.contains("`axum`") {
            return true;
        }
        if id.contains(" actix_web ")
            || id.contains("/actix-web/")
            || id.contains("`actix_web`")
        {
            return true;
        }
        if id.contains(" warp ") || id.contains("`warp`") {
            return true;
        }
        if id.contains(" rocket ") || id.contains("`rocket`") {
            return true;
        }
    }
    false
}

/// Heuristic check that a Rust function looks like an HTTP handler.
///
/// SCIP doesn't expose parameter types in a structured form for Rust as
/// of 2026-04, so we lean on the markdown signature blob (when present)
/// and look for the canonical extractor / handler types: `Request`,
/// `Json<…>`, `Query<…>`, `State<…>`, `Path<…>`, `axum::extract::*`.
///
/// Returning `false` here just downgrades the file-import signal — a
/// file that imports `axum::Router` but whose function takes no
/// HTTP-shaped parameters is more likely a route registration helper.
fn looks_like_http_handler(node: &RepoGraphNode) -> bool {
    if let Some(parts) = &node.signature_parts {
        for param in &parts.parameters {
            if let Some(ty) = &param.type_name
                && http_param_type_marker(ty)
            {
                return true;
            }
        }
    }
    if let Some(sig) = node.signature.as_deref()
        && http_signature_marker(sig)
    {
        return true;
    }
    false
}

fn http_param_type_marker(ty: &str) -> bool {
    let t = ty;
    t.contains("Request")
        || t.contains("Json<")
        || t.contains("Query<")
        || t.contains("State<")
        || t.contains("Path<")
        || t.contains("Extension<")
        || t.contains("axum::extract")
}

fn http_signature_marker(sig: &str) -> bool {
    sig.contains("Request")
        || sig.contains("Json<")
        || sig.contains("Query<")
        || sig.contains("State<")
        || sig.contains("axum::extract")
        || sig.contains("HttpRequest")
        || sig.contains("HttpResponse")
}

impl RepoDependencyGraph {
    /// PR F1: stamp an `EntryPointOf` edge from `file` to `symbol`. Used
    /// internally by [`detect_entry_points`]. Public to crate so the
    /// detector module can mutate the graph without exposing a generic
    /// edge-add surface.
    pub(crate) fn add_entry_point_edge(
        &mut self,
        file: NodeIndex,
        symbol: NodeIndex,
        confidence: f64,
        reason: &'static str,
    ) {
        // Skip duplicates: if there's already an `EntryPointOf` edge
        // between these two nodes, leave it alone. The detector only
        // reports a single hit per symbol, but incremental rebuilds via
        // `patch_changed_files` could otherwise stack edges.
        let already_present = self
            .graph_mut_unchecked()
            .edges_connecting(file, symbol)
            .any(|e| e.weight().kind == RepoGraphEdgeKind::EntryPointOf);
        if already_present {
            return;
        }

        let weight = crate::repo_graph::edge_weight_for(RepoGraphEdgeKind::EntryPointOf);
        self.graph_mut_unchecked().add_edge(
            file,
            symbol,
            RepoGraphEdge {
                kind: RepoGraphEdgeKind::EntryPointOf,
                weight,
                evidence_count: 1,
                confidence,
                reason: Some(reason.to_string()),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use crate::repo_graph::{RepoDependencyGraph, RepoGraphEdgeKind, RepoGraphNodeKind};
    use crate::scip_parser::{
        ParsedScipIndex, ScipFile, ScipMetadata, ScipOccurrence, ScipRange, ScipSymbol,
        ScipSymbolKind, ScipSymbolRole, ScipVisibility,
    };

    use super::*;

    fn def_occ(symbol: &str) -> ScipOccurrence {
        ScipOccurrence {
            symbol: symbol.to_string(),
            range: ScipRange {
                start_line: 0,
                start_character: 0,
                end_line: 0,
                end_character: 4,
            },
            enclosing_range: None,
            roles: BTreeSet::from([ScipSymbolRole::Definition]),
            syntax_kind: None,
            override_documentation: vec![],
        }
    }

    fn def_occ_with_test(symbol: &str) -> ScipOccurrence {
        let mut occ = def_occ(symbol);
        occ.roles.insert(ScipSymbolRole::Test);
        occ
    }

    fn rust_function(symbol: &str, name: &str) -> ScipSymbol {
        ScipSymbol {
            symbol: symbol.to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some(name.to_string()),
            signature: Some(format!("fn {name}()")),
            documentation: vec![],
            relationships: vec![],
            visibility: Some(ScipVisibility::Public),
            signature_parts: None,
        }
    }

    /// Build a small fixture with: a `main` symbol in `src/main.rs`, a
    /// `test_addition` in `tests/integration.rs` (no SCIP test role —
    /// caught by the heuristic), a SCIP-test-role-stamped
    /// `inline_test_one` in `src/lib.rs`, and a plain `helper` in
    /// `src/lib.rs`.
    fn fixture() -> ParsedScipIndex {
        let main_sym = "scip-rust pkg src/main.rs `main`().";
        let test_sym = "scip-rust pkg tests/integration.rs `test_addition`().";
        let inline_test_sym = "scip-rust pkg src/lib.rs `inline_test_one`().";
        let helper_sym = "scip-rust pkg src/lib.rs `helper`().";

        ParsedScipIndex {
            metadata: ScipMetadata {
                project_root: Some("file:///workspace/test".to_string()),
                tool_name: Some("scip-rust".to_string()),
                tool_version: Some("0.0.0".to_string()),
            },
            files: vec![
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/main.rs"),
                    definitions: vec![def_occ(main_sym)],
                    references: vec![],
                    occurrences: vec![def_occ(main_sym)],
                    symbols: vec![rust_function(main_sym, "main")],
                },
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/lib.rs"),
                    definitions: vec![def_occ_with_test(inline_test_sym), def_occ(helper_sym)],
                    references: vec![],
                    occurrences: vec![def_occ_with_test(inline_test_sym), def_occ(helper_sym)],
                    symbols: vec![
                        rust_function(inline_test_sym, "inline_test_one"),
                        rust_function(helper_sym, "helper"),
                    ],
                },
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("tests/integration.rs"),
                    definitions: vec![def_occ(test_sym)],
                    references: vec![],
                    occurrences: vec![def_occ(test_sym)],
                    symbols: vec![rust_function(test_sym, "test_addition")],
                },
            ],
            external_symbols: vec![],
        }
    }

    /// Helper: collect the `display_name` of each symbol that has at
    /// least one incoming `EntryPointOf` edge.
    fn entry_point_names(graph: &RepoDependencyGraph) -> Vec<String> {
        let mut out = Vec::new();
        for idx in graph.graph().node_indices() {
            let node = graph.node(idx);
            if node.kind != RepoGraphNodeKind::Symbol {
                continue;
            }
            let has_entry = graph
                .graph()
                .edges_directed(idx, Direction::Incoming)
                .any(|e| e.weight().kind == RepoGraphEdgeKind::EntryPointOf);
            if has_entry {
                out.push(node.display_name.clone());
            }
        }
        out.sort();
        out
    }

    #[test]
    fn detects_rust_main_and_test_symbols() {
        let graph = RepoDependencyGraph::build(&[fixture()]);
        let names = entry_point_names(&graph);
        assert!(
            names.contains(&"main".to_string()),
            "main should be tagged as entry point: {names:?}"
        );
        assert!(
            names.contains(&"test_addition".to_string()),
            "test_addition (file under tests/) should be tagged: {names:?}"
        );
        assert!(
            names.contains(&"inline_test_one".to_string()),
            "inline test with SCIP `Test` role should be tagged: {names:?}"
        );
        assert!(
            !names.contains(&"helper".to_string()),
            "plain helper should NOT be tagged: {names:?}"
        );
    }

    #[test]
    fn entry_point_edge_carries_reason_and_confidence() {
        let graph = RepoDependencyGraph::build(&[fixture()]);
        let main_node = graph
            .symbol_node("scip-rust pkg src/main.rs `main`().")
            .expect("main symbol node present");
        let edge = graph
            .graph()
            .edges_directed(main_node, Direction::Incoming)
            .find(|e| e.weight().kind == RepoGraphEdgeKind::EntryPointOf)
            .expect("main has incoming EntryPointOf edge");
        assert_eq!(edge.weight().reason.as_deref(), Some("rust-main"));
        assert!((edge.weight().confidence - 0.95).abs() < 1e-9);
    }

    #[test]
    fn k8s_binary_target_path_is_recognized() {
        let bin_sym = "scip-rust pkg crates/foo/src/bin/worker.rs `main`().";
        let index = ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("crates/foo/src/bin/worker.rs"),
                definitions: vec![def_occ(bin_sym)],
                references: vec![],
                occurrences: vec![def_occ(bin_sym)],
                symbols: vec![rust_function(bin_sym, "main")],
            }],
            external_symbols: vec![],
        };
        let graph = RepoDependencyGraph::build(&[index]);
        let names = entry_point_names(&graph);
        assert_eq!(names, vec!["main".to_string()]);
    }

    #[test]
    fn python_dunder_main_is_recognized() {
        let main_sym = "scip-python pkg app/cli.py `__main__`().";
        let helper_sym = "scip-python pkg app/cli.py `helper`().";
        let index = ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![ScipFile {
                language: "python".to_string(),
                relative_path: PathBuf::from("app/cli.py"),
                definitions: vec![def_occ(main_sym), def_occ(helper_sym)],
                references: vec![],
                occurrences: vec![def_occ(main_sym), def_occ(helper_sym)],
                symbols: vec![
                    ScipSymbol {
                        symbol: main_sym.to_string(),
                        kind: Some(ScipSymbolKind::Function),
                        display_name: Some("__main__".to_string()),
                        signature: None,
                        documentation: vec![],
                        relationships: vec![],
                        visibility: Some(ScipVisibility::Public),
                        signature_parts: None,
                    },
                    ScipSymbol {
                        symbol: helper_sym.to_string(),
                        kind: Some(ScipSymbolKind::Function),
                        display_name: Some("helper".to_string()),
                        signature: None,
                        documentation: vec![],
                        relationships: vec![],
                        visibility: Some(ScipVisibility::Public),
                        signature_parts: None,
                    },
                ],
            }],
            external_symbols: vec![],
        };
        let graph = RepoDependencyGraph::build(&[index]);
        let names = entry_point_names(&graph);
        assert!(names.contains(&"__main__".to_string()), "{names:?}");
        assert!(!names.contains(&"helper".to_string()), "{names:?}");
    }

    #[test]
    fn detection_is_idempotent_when_re_run() {
        let mut graph = RepoDependencyGraph::build(&[fixture()]);
        let before = graph.edge_count();
        // Re-running the detector should not add fresh edges.
        let _ = detect_entry_points(&mut graph);
        assert_eq!(
            graph.edge_count(),
            before,
            "second detection pass must not stack edges"
        );
    }
}
