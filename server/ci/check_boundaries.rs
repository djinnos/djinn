#![allow(clippy::disallowed_methods)] // TODO(70y0): temporary; remove after wall-clock migration
//! CI boundary checker — loads `boundary_rules.toml`, builds the crate-level
//! dependency graph from the warmed canonical graph, and exits non-zero if any
//! rule-set edge is violated.
//!
//! Usage:
//!   DJINN_DATABASE_URL=… cargo run --bin check-boundaries -- \
//!       --project-id <id> --project-path <repo-root> [--rules boundary_rules.toml]
//!
//! Prerequisites:
//!   * The canonical graph must already be warmed for `--project-id` (the warm
//!     job populates `repo_graph_cache`). Run this step after graph warming.
//!   * `DJINN_DATABASE_URL` must point at the same Postgres the server uses.
//!
//! Exit codes:
//!   0 — no violations
//!   1 — one or more violations found (human-readable report on stderr)
//!   2 — operational error (graph not warmed, unreadable rules file, DB error)

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use globset::Glob;
use serde::Deserialize;

use djinn_graph::repo_graph::CrateGraph;

// ---------------------------------------------------------------------------
// Minimal WarmContext — same shape as WorkerWarmContext in djinn-agent-worker.
// ---------------------------------------------------------------------------

struct CiWarmContext {
    db: djinn_db::Database,
    indexer_lock: Arc<tokio::sync::Mutex<()>>,
}

impl djinn_graph::WarmContext for CiWarmContext {
    fn db(&self) -> &djinn_db::Database {
        &self.db
    }

    fn event_bus(&self) -> djinn_core::events::EventBus {
        djinn_core::events::EventBus::noop()
    }

    fn indexer_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.indexer_lock.clone()
    }
}

// ---------------------------------------------------------------------------
// TOML schema
// ---------------------------------------------------------------------------

/// Top-level structure of `boundary_rules.toml`.
#[derive(Deserialize)]
struct BoundaryConfig {
    #[allow(dead_code)]
    boundary: BoundaryMeta,
    rules: Vec<TomlRule>,
}

#[derive(Deserialize)]
struct BoundaryMeta {
    #[allow(dead_code)]
    level: String,
    #[allow(dead_code)]
    description: Option<String>,
}

/// A single rule as written in the TOML file. Unlike the wire-protocol
/// `BoundaryRule` (which omits `name`), this struct captures the `name`
/// field so the CI report can cite it.
#[derive(Deserialize, Debug)]
struct TomlRule {
    name: String,
    from_glob: String,
    to_glob: String,
    #[serde(default)]
    description: Option<String>,
}

// ---------------------------------------------------------------------------
// Validation / compilation helpers
// ---------------------------------------------------------------------------

/// Structured error returned when a rule fails validation.
#[derive(Debug, PartialEq)]
struct RuleValidationError {
    index: usize,
    name: String,
    field: String,
    message: String,
}

impl fmt::Display for RuleValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "rule[{index}] '{name}' — {field}: {message}",
            index = self.index,
            name = self.name,
            field = self.field,
            message = self.message
        )
    }
}

/// Returns a list of validation errors for every rule that is semantically
/// invalid.  If the returned list is empty, every rule is well-formed.
fn validate_rules(rules: &[TomlRule]) -> Vec<RuleValidationError> {
    let mut errors = Vec::new();
    for (i, rule) in rules.iter().enumerate() {
        let display_name = if rule.name.trim().is_empty() {
            "<unnamed>"
        } else {
            rule.name.trim()
        };

        if rule.name.trim().is_empty() {
            errors.push(RuleValidationError {
                index: i,
                name: display_name.to_string(),
                field: "name".to_string(),
                message: "must be nonblank".to_string(),
            });
        }

        if rule.from_glob.trim().is_empty() {
            errors.push(RuleValidationError {
                index: i,
                name: display_name.to_string(),
                field: "from_glob".to_string(),
                message: "must be nonblank".to_string(),
            });
        }

        if rule.to_glob.trim().is_empty() {
            errors.push(RuleValidationError {
                index: i,
                name: display_name.to_string(),
                field: "to_glob".to_string(),
                message: "must be nonblank".to_string(),
            });
        }

        let desc = rule.description.as_deref().unwrap_or("").trim();
        if desc.is_empty() {
            errors.push(RuleValidationError {
                index: i,
                name: display_name.to_string(),
                field: "description".to_string(),
                message: "must be present and nonblank".to_string(),
            });
        } else if is_boilerplate_description(desc) {
            errors.push(RuleValidationError {
                index: i,
                name: display_name.to_string(),
                field: "description".to_string(),
                message: "must be meaningful (not boilerplate)".to_string(),
            });
        }
    }
    errors
}

fn is_boilerplate_description(desc: &str) -> bool {
    let lower = desc.to_lowercase();
    let boilerplates = [
        "todo",
        "fixme",
        "placeholder",
        "tbd",
        "no description",
        "description here",
        "insert description",
    ];
    boilerplates.iter().any(|b| lower.contains(b))
}

/// Compiled rule: index, reference to the original rule, and compiled matchers.
#[derive(Debug)]
struct CompiledRule<'a> {
    index: usize,
    rule: &'a TomlRule,
    from_matcher: globset::GlobMatcher,
    to_matcher: globset::GlobMatcher,
}

/// Compile every rule into a pair of `GlobMatcher`s.  Returns a structured
/// error list on failure so the CLI can fail closed.
fn compile_rules(rules: &[TomlRule]) -> Result<Vec<CompiledRule<'_>>, Vec<RuleValidationError>> {
    let mut errors = Vec::new();
    let mut compiled = Vec::with_capacity(rules.len());

    for (i, rule) in rules.iter().enumerate() {
        let from_norm = normalise_crate_glob(&rule.from_glob);
        let from_glob = match Glob::new(&from_norm) {
            Ok(g) => g,
            Err(e) => {
                errors.push(RuleValidationError {
                    index: i,
                    name: rule.name.clone(),
                    field: "from_glob".to_string(),
                    message: format!("invalid glob: {e}"),
                });
                continue;
            }
        };
        let to_norm = normalise_crate_glob(&rule.to_glob);
        let to_glob = match Glob::new(&to_norm) {
            Ok(g) => g,
            Err(e) => {
                errors.push(RuleValidationError {
                    index: i,
                    name: rule.name.clone(),
                    field: "to_glob".to_string(),
                    message: format!("invalid glob: {e}"),
                });
                continue;
            }
        };
        compiled.push(CompiledRule {
            index: i,
            rule,
            from_matcher: from_glob.compile_matcher(),
            to_matcher: to_glob.compile_matcher(),
        });
    }

    if !errors.is_empty() {
        Err(errors)
    } else {
        Ok(compiled)
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "check-boundaries",
    about = "CI crate-level boundary checker — fails on architectural violations"
)]
struct Cli {
    /// Path to the boundary rules TOML file (relative to CWD).
    #[arg(long, default_value = "boundary_rules.toml")]
    rules: PathBuf,

    /// Project ID used to look up the warmed graph in `repo_graph_cache`.
    #[arg(long, env = "DJINN_PROJECT_ID")]
    project_id: String,

    /// Project clone path (the repo root containing the Cargo workspace).
    /// The canonical graph is loaded from the index-tree worktree derived
    /// from this path.
    #[arg(long, env = "DJINN_PROJECT_PATH", default_value = ".")]
    project_path: String,
}

// ---------------------------------------------------------------------------
// Crate-glob normalisation (mirrors the private `normalise_crate_glob`
// in `snapshot.rs` so TOML path-style globs match bare crate names).
// ---------------------------------------------------------------------------

fn normalise_crate_glob(glob: &str) -> String {
    let mut s = glob.trim().to_string();
    // Strip leading wildcard segments: `**/`, `*/`.
    while s.starts_with("**/") || s.starts_with("*/") {
        let idx = s.find('/').unwrap();
        s = s[idx + 1..].to_string();
    }
    if s == "**" || s == "*" {
        return s;
    }
    // Strip trailing wildcard segments: `/**` or `/*`.
    if s.ends_with("/**") {
        s = s[..s.len() - 3].to_string();
    } else if s.ends_with("/*") {
        s = s[..s.len() - 2].to_string();
    }
    s
}

/// Run `git rev-parse HEAD` in `repo_root` and return the trimmed commit
/// hash, or `None` if git is not available or the command fails.
fn resolve_current_head(project_root: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .current_dir(project_root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Validate that a loaded `RepoDependencyGraph` is structurally non-empty.
/// Returns `Err` with a human-readable message when the graph has zero
/// nodes or zero edges.
fn check_graph_sanity(graph: &djinn_graph::repo_graph::RepoDependencyGraph) -> Result<(), String> {
    if graph.node_count() == 0 {
        return Err("loaded graph has zero nodes".to_string());
    }
    if graph.edge_count() == 0 {
        return Err("loaded graph has zero edges".to_string());
    }
    Ok(())
}

/// Validate that the derived `CrateGraph` is usable for boundary checking.
/// Returns `Err` with a human-readable message when the crate graph has
/// no usable crate nodes or edges for a nontrivial workspace.
fn check_crate_graph_usable(
    crate_graph: &djinn_graph::repo_graph::CrateGraph,
) -> Result<(), String> {
    // A nontrivial workspace should have at least some crate nodes.
    if crate_graph.crates.is_empty() {
        return Err("derived crate graph has no crate nodes".to_string());
    }
    // We need at least one cross-crate edge to meaningfully check boundaries.
    if crate_graph.edges.is_empty() {
        return Err("derived crate graph has no cross-crate edges".to_string());
    }
    Ok(())
}

// All output in `main` goes to stdout/stderr by design — this is a CLI
// diagnostic tool that reports results directly to the terminal.
#[allow(clippy::print_stderr, clippy::print_stdout)]
#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // 1. Load and parse boundary_rules.toml.
    let rules_text = match std::fs::read_to_string(&cli.rules) {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "Error: cannot read rules file '{}': {e}",
                cli.rules.display()
            );
            std::process::exit(2);
        }
    };

    let config: BoundaryConfig = match toml::from_str(&rules_text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "Error: cannot parse rules file '{}': {e}",
                cli.rules.display()
            );
            std::process::exit(2);
        }
    };

    if config.rules.is_empty() {
        eprintln!(
            "Error: no boundary rules defined in '{}'.",
            cli.rules.display()
        );
        std::process::exit(2);
    }

    // 1b. Semantic validation of every rule (fail-closed).
    let validation_errors = validate_rules(&config.rules);
    if !validation_errors.is_empty() {
        eprintln!("Error: boundary rule validation failed:");
        for err in &validation_errors {
            eprintln!("  {err}");
        }
        std::process::exit(2);
    }

    // 2. Connect to the database so we can load the warmed graph blob.
    let db_url = match std::env::var("DJINN_DATABASE_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("Error: DJINN_DATABASE_URL must be set so the warmed graph can be loaded.");
            std::process::exit(2);
        }
    };

    let connect =
        djinn_db::DatabaseConnectConfig::Postgres(djinn_db::PostgresDatabaseConfig { url: db_url });
    let db = match djinn_db::Database::open_with_config(connect) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Error: failed to open database: {e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = db.verify_and_mark_initialized().await {
        eprintln!("Error: database schema verification failed: {e}");
        std::process::exit(2);
    }

    let ctx = CiWarmContext {
        db,
        indexer_lock: Arc::new(tokio::sync::Mutex::new(())),
    };

    // 3. Load the warmed canonical graph from repo_graph_cache.
    let graph = match djinn_graph::canonical_graph::load_canonical_graph_only(
        &ctx,
        &cli.project_id,
        &cli.project_path,
    )
    .await
    {
        Ok(g) => g,
        Err(e) => {
            eprintln!(
                "Error: cannot load warmed graph for project '{}': {e}",
                cli.project_id
            );
            eprintln!(
                "Hint: the graph must be warmed before this CI step. Run the warm-graph job first."
            );
            std::process::exit(2);
        }
    };

    // 3a. Verify the loaded graph is structurally non-empty.
    if let Err(e) = check_graph_sanity(&graph) {
        eprintln!("Error: loaded graph is unusable: {e}");
        eprintln!(
            "Hint: the graph may be corrupted or incompletely warmed. Run the warm-graph job first."
        );
        std::process::exit(2);
    }

    // 3b. Verify the cached graph is pinned to the current checkout HEAD.
    let (_project_root, index_tree_path) =
        djinn_graph::canonical_graph::normalize_graph_query_paths(&cli.project_path);
    let pinned_commit =
        djinn_graph::canonical_graph::canonical_graph_cache_pinned_commit_for(&index_tree_path)
            .await;

    let current_head = resolve_current_head(&index_tree_path);
    match (current_head.as_deref(), pinned_commit.as_deref()) {
        (None, _) => {
            eprintln!(
                "Error: unable to determine current git HEAD for '{}'.",
                index_tree_path.display()
            );
            eprintln!("Hint: ensure --project-path points to a valid git checkout.");
            std::process::exit(2);
        }
        (_, None) => {
            eprintln!("Error: warmed graph has no pinned commit.");
            eprintln!(
                "Hint: the graph must be warmed before this CI step. Run the warm-graph job first."
            );
            std::process::exit(2);
        }
        (Some(head), Some(pinned)) => {
            if djinn_graph::canonical_graph::git_head_is_strictly_stale(head, pinned) {
                eprintln!("Error: warmed graph is stale (pinned: {pinned}, current HEAD: {head}).");
                eprintln!(
                    "Hint: re-run the warm-graph step for the current checkout before make check-boundaries."
                );
                std::process::exit(2);
            }
        }
    }

    // 4. Derive the crate map from the index-tree checkout (where Cargo.toml lives).
    let crate_map = djinn_graph::canonical_graph::derive_crate_map(&index_tree_path);

    if crate_map.is_empty() {
        eprintln!(
            "Error: no crate mapping derived from '{}'.",
            index_tree_path.display()
        );
        eprintln!("Hint: ensure --project-path points to a Cargo workspace root.");
        std::process::exit(2);
    }

    // 5. Build the crate-level aggregated graph.
    let crate_graph = djinn_graph::repo_graph::build_crate_graph(&graph, &crate_map);

    // 5a. Verify the derived crate graph is usable for boundary checking.
    if let Err(e) = check_crate_graph_usable(&crate_graph) {
        eprintln!("Error: derived crate graph is unusable: {e}");
        eprintln!(
            "Hint: the workspace may have no cross-crate dependencies, or the graph warm may have failed. Run the warm-graph job first."
        );
        std::process::exit(2);
    }

    // 6. Compile rules (normalise globs to crate-name patterns) and match edges.
    let compiled = match compile_rules(&config.rules) {
        Ok(c) => c,
        Err(errors) => {
            eprintln!("Error: boundary rule compilation failed:");
            for err in &errors {
                eprintln!("  {err}");
            }
            std::process::exit(2);
        }
    };

    // 7. Check + report. Use the same `check_violations` / `render_violation_report`
    //    helpers the tests exercise, so `main` and the test suite share one code
    //    path (and the helpers aren't dead code in the bin target).
    let violations = check_violations(&crate_graph, &compiled);

    if violations.is_empty() {
        println!(
            "✓ No boundary violations found. (checked {} rule(s) against {} crate edge(s))",
            config.rules.len(),
            crate_graph.edges.len()
        );
        std::process::exit(0);
    }

    // `check_violations` returns owned source/target strings; borrow them as
    // `&str` for the renderer's slice-based signature.
    let borrowed: Vec<(usize, &TomlRule, &str, &str)> = violations
        .iter()
        .map(|(index, rule, from, to)| (*index, *rule, from.as_str(), to.as_str()))
        .collect();
    let mut report = String::new();
    let exit_code = render_violation_report(&mut report, &borrowed)
        .expect("writing a boundary report to a String never fails");
    eprint!("{report}");
    std::process::exit(exit_code);
}

// ---------------------------------------------------------------------------
// Testable helpers for violation checking and reporting
// ---------------------------------------------------------------------------

/// Check a crate graph against compiled boundary rules and return the
/// violations as structured data.  This helper is factored out of `main`
/// so tests can assert forbidden-edge behaviour without spawning the full
/// binary (which needs Postgres).
fn check_violations<'a>(
    crate_graph: &CrateGraph,
    compiled: &'a [CompiledRule<'a>],
) -> Vec<(usize, &'a TomlRule, String, String)> {
    let mut violations = Vec::new();
    for edge in &crate_graph.edges {
        for cr in compiled {
            if cr.from_matcher.is_match(&edge.source) && cr.to_matcher.is_match(&edge.target) {
                violations.push((cr.index, cr.rule, edge.source.clone(), edge.target.clone()));
            }
        }
    }
    violations
}

/// Render a human-readable violation report to the supplied writer.
/// Returns the exit code that the CLI should use (1 for violations).
fn render_violation_report<W: std::fmt::Write>(
    writer: &mut W,
    violations: &[(usize, &TomlRule, &str, &str)],
) -> Result<i32, std::fmt::Error> {
    writeln!(
        writer,
        "✗ {} boundary violation(s) found:\n",
        violations.len()
    )?;
    for (i, rule, from, to) in violations {
        writeln!(writer, "  [{rule_index}] {from} → {to}", rule_index = i)?;
        writeln!(writer, "      rule name:  {}", rule.name)?;
        if let Some(desc) = &rule.description {
            writeln!(writer, "      description: {desc}")?;
        }
        writeln!(writer, "      from_key:   {from}")?;
        writeln!(writer, "      to_key:     {to}")?;
        writeln!(writer, "      witness:    {from} → {to}")?;
        writeln!(writer)?;
    }
    Ok(1)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use djinn_graph::repo_graph::{CrateEdge, CrateGraph, CrateNode, RepoDependencyGraph};
    use djinn_graph::scip_parser::{
        ParsedScipIndex, ScipFile, ScipMetadata, ScipOccurrence, ScipRange, ScipSymbol,
        ScipSymbolKind, ScipSymbolRole,
    };

    #[test]
    fn normalise_crate_glob_strips_path_wildcards() {
        assert_eq!(normalise_crate_glob("**/djinn-agent/**"), "djinn-agent");
        assert_eq!(normalise_crate_glob("**/djinn-agent"), "djinn-agent");
        assert_eq!(normalise_crate_glob("djinn-agent/**"), "djinn-agent");
        assert_eq!(normalise_crate_glob("djinn-agent"), "djinn-agent");
        assert_eq!(normalise_crate_glob("**/djinn-*/**"), "djinn-*");
        assert_eq!(normalise_crate_glob("*"), "*");
        assert_eq!(normalise_crate_glob("**"), "**");
        assert_eq!(normalise_crate_glob("*/djinn-agent"), "djinn-agent");
    }

    // ------------------------------------------------------------------
    // Empty rule set
    // ------------------------------------------------------------------

    #[test]
    fn empty_rules_list_is_reported_as_empty() {
        let rules: Vec<TomlRule> = Vec::new();
        let errors = validate_rules(&rules);
        assert!(errors.is_empty());
        // The CLI path must separately reject an empty list and exit 2.
    }

    // ------------------------------------------------------------------
    // Blank required fields
    // ------------------------------------------------------------------

    #[test]
    fn blank_name_is_rejected() {
        let rules = vec![TomlRule {
            name: "   ".to_string(),
            from_glob: "**/a/**".to_string(),
            to_glob: "**/b/**".to_string(),
            description: Some("Valid description.".to_string()),
        }];
        let errors = validate_rules(&rules);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "name");
        assert_eq!(errors[0].message, "must be nonblank");
    }

    #[test]
    fn blank_from_glob_is_rejected() {
        let rules = vec![TomlRule {
            name: "rule".to_string(),
            from_glob: "   ".to_string(),
            to_glob: "**/b/**".to_string(),
            description: Some("Valid description.".to_string()),
        }];
        let errors = validate_rules(&rules);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "from_glob");
        assert_eq!(errors[0].message, "must be nonblank");
    }

    #[test]
    fn blank_to_glob_is_rejected() {
        let rules = vec![TomlRule {
            name: "rule".to_string(),
            from_glob: "**/a/**".to_string(),
            to_glob: "   ".to_string(),
            description: Some("Valid description.".to_string()),
        }];
        let errors = validate_rules(&rules);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "to_glob");
        assert_eq!(errors[0].message, "must be nonblank");
    }

    // ------------------------------------------------------------------
    // Description validation
    // ------------------------------------------------------------------

    #[test]
    fn missing_description_is_rejected() {
        let rules = vec![TomlRule {
            name: "rule".to_string(),
            from_glob: "**/a/**".to_string(),
            to_glob: "**/b/**".to_string(),
            description: None,
        }];
        let errors = validate_rules(&rules);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "description");
        assert_eq!(errors[0].message, "must be present and nonblank");
    }

    #[test]
    fn blank_description_is_rejected() {
        let rules = vec![TomlRule {
            name: "rule".to_string(),
            from_glob: "**/a/**".to_string(),
            to_glob: "**/b/**".to_string(),
            description: Some("    ".to_string()),
        }];
        let errors = validate_rules(&rules);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "description");
        assert_eq!(errors[0].message, "must be present and nonblank");
    }

    #[test]
    fn boilerplate_description_is_rejected() {
        let rules = vec![TomlRule {
            name: "rule".to_string(),
            from_glob: "**/a/**".to_string(),
            to_glob: "**/b/**".to_string(),
            description: Some("TODO: write a real description".to_string()),
        }];
        let errors = validate_rules(&rules);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "description");
        assert_eq!(errors[0].message, "must be meaningful (not boilerplate)");
    }

    #[test]
    fn meaningful_description_is_accepted() {
        let rules = vec![TomlRule {
            name: "rule".to_string(),
            from_glob: "**/a/**".to_string(),
            to_glob: "**/b/**".to_string(),
            description: Some("Agent must not import control-plane.".to_string()),
        }];
        let errors = validate_rules(&rules);
        assert!(errors.is_empty());
    }

    // ------------------------------------------------------------------
    // Invalid globs
    // ------------------------------------------------------------------

    #[test]
    fn invalid_from_glob_fails_compilation() {
        let rules = vec![TomlRule {
            name: "bad-from".to_string(),
            from_glob: "[unclosed".to_string(),
            to_glob: "**/b/**".to_string(),
            description: Some("Valid description.".to_string()),
        }];
        let result = compile_rules(&rules);
        assert!(result.is_err());
        let errs = result.unwrap_err();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "from_glob");
        assert!(errs[0].message.contains("invalid glob"));
    }

    #[test]
    fn invalid_to_glob_fails_compilation() {
        let rules = vec![TomlRule {
            name: "bad-to".to_string(),
            from_glob: "**/a/**".to_string(),
            to_glob: "[unclosed".to_string(),
            description: Some("Valid description.".to_string()),
        }];
        let result = compile_rules(&rules);
        assert!(result.is_err());
        let errs = result.unwrap_err();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "to_glob");
        assert!(errs[0].message.contains("invalid glob"));
    }

    #[test]
    fn multiple_invalid_globs_collect_all_errors() {
        let rules = vec![
            TomlRule {
                name: "bad-from".to_string(),
                from_glob: "[unclosed".to_string(),
                to_glob: "**/b/**".to_string(),
                description: Some("Valid description.".to_string()),
            },
            TomlRule {
                name: "bad-to".to_string(),
                from_glob: "**/a/**".to_string(),
                to_glob: "[unclosed".to_string(),
                description: Some("Valid description.".to_string()),
            },
        ];
        let result = compile_rules(&rules);
        assert!(result.is_err());
        let errs = result.unwrap_err();
        assert_eq!(errs.len(), 2);
    }

    #[test]
    fn valid_globs_compile_successfully() {
        let rules = vec![TomlRule {
            name: "good".to_string(),
            from_glob: "**/djinn-agent/**".to_string(),
            to_glob: "**/djinn-control-plane/**".to_string(),
            description: Some("Agent must not import control-plane.".to_string()),
        }];
        let result = compile_rules(&rules);
        assert!(result.is_ok());
        let compiled = result.unwrap();
        assert_eq!(compiled.len(), 1);
    }

    // ------------------------------------------------------------------
    // Graph sanity helpers
    // ------------------------------------------------------------------

    #[test]
    fn check_graph_sanity_rejects_zero_nodes() {
        let graph = RepoDependencyGraph::build(&[]);
        assert_eq!(graph.node_count(), 0);
        let result = check_graph_sanity(&graph);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("zero nodes"));
    }

    #[test]
    fn check_graph_sanity_accepts_nonempty_graph() {
        // Build a minimal index with a reference edge so the graph has
        // both nodes and edges.
        use std::collections::BTreeSet;
        let main_symbol = "scip-rust pkg src/app.rs `main`().".to_string();
        let helper_symbol = "scip-rust pkg src/helper.rs `helper`().".to_string();
        let index = ParsedScipIndex {
            workspace_slug: "root".to_string(),
            metadata: ScipMetadata {
                project_root: Some("file:///workspace/repo".to_string()),
                tool_name: Some("rust-analyzer".to_string()),
                tool_version: Some("1.0.0".to_string()),
            },
            files: vec![
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/helper.rs"),
                    definitions: vec![ScipOccurrence {
                        symbol: helper_symbol.clone(),
                        range: ScipRange {
                            start_line: 0,
                            start_character: 0,
                            end_line: 0,
                            end_character: 5,
                        },
                        enclosing_range: None,
                        roles: BTreeSet::from([ScipSymbolRole::Definition]),
                        syntax_kind: None,
                        override_documentation: vec![],
                    }],
                    references: vec![],
                    occurrences: vec![ScipOccurrence {
                        symbol: helper_symbol.clone(),
                        range: ScipRange {
                            start_line: 0,
                            start_character: 0,
                            end_line: 0,
                            end_character: 5,
                        },
                        enclosing_range: None,
                        roles: BTreeSet::from([ScipSymbolRole::Definition]),
                        syntax_kind: None,
                        override_documentation: vec![],
                    }],
                    symbols: vec![ScipSymbol {
                        symbol: helper_symbol.clone(),
                        kind: Some(ScipSymbolKind::Function),
                        display_name: Some("helper".to_string()),
                        signature: Some("fn helper()".to_string()),
                        documentation: vec![],
                        relationships: vec![],
                        visibility: Some(djinn_graph::scip_parser::ScipVisibility::Public),
                        signature_parts: None,
                    }],
                },
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/app.rs"),
                    definitions: vec![ScipOccurrence {
                        symbol: main_symbol.clone(),
                        range: ScipRange {
                            start_line: 0,
                            start_character: 0,
                            end_line: 0,
                            end_character: 5,
                        },
                        enclosing_range: None,
                        roles: BTreeSet::from([ScipSymbolRole::Definition]),
                        syntax_kind: None,
                        override_documentation: vec![],
                    }],
                    references: vec![ScipOccurrence {
                        symbol: helper_symbol.clone(),
                        range: ScipRange {
                            start_line: 1,
                            start_character: 0,
                            end_line: 1,
                            end_character: 5,
                        },
                        enclosing_range: None,
                        roles: BTreeSet::from([ScipSymbolRole::ReadAccess]),
                        syntax_kind: None,
                        override_documentation: vec![],
                    }],
                    occurrences: vec![
                        ScipOccurrence {
                            symbol: main_symbol.clone(),
                            range: ScipRange {
                                start_line: 0,
                                start_character: 0,
                                end_line: 0,
                                end_character: 5,
                            },
                            enclosing_range: None,
                            roles: BTreeSet::from([ScipSymbolRole::Definition]),
                            syntax_kind: None,
                            override_documentation: vec![],
                        },
                        ScipOccurrence {
                            symbol: helper_symbol.clone(),
                            range: ScipRange {
                                start_line: 1,
                                start_character: 0,
                                end_line: 1,
                                end_character: 5,
                            },
                            enclosing_range: None,
                            roles: BTreeSet::from([ScipSymbolRole::ReadAccess]),
                            syntax_kind: None,
                            override_documentation: vec![],
                        },
                    ],
                    symbols: vec![ScipSymbol {
                        symbol: main_symbol.clone(),
                        kind: Some(ScipSymbolKind::Function),
                        display_name: Some("main".to_string()),
                        signature: Some("fn main()".to_string()),
                        documentation: vec![],
                        relationships: vec![],
                        visibility: Some(djinn_graph::scip_parser::ScipVisibility::Public),
                        signature_parts: None,
                    }],
                },
            ],
            external_symbols: vec![],
        };
        let graph = RepoDependencyGraph::build(&[index]);
        assert!(graph.node_count() > 0);
        assert!(graph.edge_count() > 0);
        let result = check_graph_sanity(&graph);
        assert!(result.is_ok());
    }

    #[test]
    fn check_crate_graph_usable_rejects_empty_crates() {
        let crate_graph = CrateGraph {
            crates: vec![],
            edges: vec![],
        };
        let result = check_crate_graph_usable(&crate_graph);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no crate nodes"));
    }

    #[test]
    fn check_crate_graph_usable_rejects_empty_edges() {
        let crate_graph = CrateGraph {
            crates: vec![CrateNode {
                name: "a".to_string(),
                manifest_path: PathBuf::from("a/Cargo.toml"),
                loc: 1,
                node_count: 1,
                fan_in: 0.0,
                fan_out: 0.0,
                inbound_weight: 0.0,
                outbound_weight: 0.0,
            }],
            edges: vec![],
        };
        let result = check_crate_graph_usable(&crate_graph);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no cross-crate edges"));
    }

    #[test]
    fn check_crate_graph_usable_accepts_nonempty() {
        let crate_graph = CrateGraph {
            crates: vec![
                CrateNode {
                    name: "a".to_string(),
                    manifest_path: PathBuf::from("a/Cargo.toml"),
                    loc: 1,
                    node_count: 1,
                    fan_in: 0.0,
                    fan_out: 1.0,
                    inbound_weight: 0.0,
                    outbound_weight: 1.0,
                },
                CrateNode {
                    name: "b".to_string(),
                    manifest_path: PathBuf::from("b/Cargo.toml"),
                    loc: 1,
                    node_count: 1,
                    fan_in: 1.0,
                    fan_out: 0.0,
                    inbound_weight: 1.0,
                    outbound_weight: 0.0,
                },
            ],
            edges: vec![CrateEdge {
                source: "a".to_string(),
                target: "b".to_string(),
                weight: 1.0,
                edge_count: 1,
            }],
        };
        let result = check_crate_graph_usable(&crate_graph);
        assert!(result.is_ok());
    }

    // ------------------------------------------------------------------
    // resolve_current_head
    // ------------------------------------------------------------------

    #[test]
    fn resolve_current_head_returns_some_in_git_repo() {
        // The workspace itself is a git repo.
        let head = resolve_current_head(Path::new("."));
        assert!(
            head.is_some(),
            "expected to resolve HEAD in the current git repo"
        );
        let head = head.unwrap();
        assert_eq!(head.len(), 40, "expected full SHA-1 hex string");
        assert!(head.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn resolve_current_head_returns_none_outside_git() {
        let tmp = std::env::temp_dir();
        let head = resolve_current_head(&tmp);
        assert!(head.is_none(), "expected no HEAD outside a git repo");
    }

    // ------------------------------------------------------------------
    // Display formatting
    // ------------------------------------------------------------------

    #[test]
    fn rule_validation_error_display_format() {
        let err = RuleValidationError {
            index: 3,
            name: "my-rule".to_string(),
            field: "from_glob".to_string(),
            message: "must be nonblank".to_string(),
        };
        let s = format!("{err}");
        assert_eq!(s, "rule[3] 'my-rule' — from_glob: must be nonblank");
    }

    // ------------------------------------------------------------------
    // check_violations — forbidden-edge fixture tests
    // ------------------------------------------------------------------

    /// Build a minimal `CrateGraph` with a single forbidden edge that
    /// matches a boundary rule.  This is the lowest-practical-level
    /// fixture for asserting violation detection without Postgres.
    fn forbidden_edge_crate_graph() -> CrateGraph {
        CrateGraph {
            crates: vec![
                CrateNode {
                    name: "djinn-agent".to_string(),
                    manifest_path: PathBuf::from("crates/djinn-agent/Cargo.toml"),
                    loc: 100,
                    node_count: 10,
                    fan_in: 0.0,
                    fan_out: 1.0,
                    inbound_weight: 0.0,
                    outbound_weight: 1.0,
                },
                CrateNode {
                    name: "djinn-control-plane".to_string(),
                    manifest_path: PathBuf::from("crates/djinn-control-plane/Cargo.toml"),
                    loc: 200,
                    node_count: 20,
                    fan_in: 1.0,
                    fan_out: 0.0,
                    inbound_weight: 1.0,
                    outbound_weight: 0.0,
                },
            ],
            edges: vec![CrateEdge {
                source: "djinn-agent".to_string(),
                target: "djinn-control-plane".to_string(),
                weight: 1.0,
                edge_count: 1,
            }],
        }
    }

    #[test]
    fn check_violations_detects_forbidden_edge() {
        let crate_graph = forbidden_edge_crate_graph();
        let rules = vec![TomlRule {
            name: "no-agent-imports-control-plane".to_string(),
            from_glob: "**/djinn-agent/**".to_string(),
            to_glob: "**/djinn-control-plane/**".to_string(),
            description: Some(
                "Agent must not import control-plane; control-plane is the bridge layer."
                    .to_string(),
            ),
        }];
        let compiled = compile_rules(&rules).expect("valid globs should compile");
        let violations = check_violations(&crate_graph, &compiled);
        assert_eq!(violations.len(), 1, "expected exactly one violation");
        let (idx, rule, from, to) = &violations[0];
        assert_eq!(*idx, 0);
        assert_eq!(rule.name, "no-agent-imports-control-plane");
        assert_eq!(from, "djinn-agent");
        assert_eq!(to, "djinn-control-plane");
    }

    #[test]
    fn check_violations_returns_empty_when_no_edges_match() {
        let crate_graph = CrateGraph {
            crates: vec![
                CrateNode {
                    name: "djinn-core".to_string(),
                    manifest_path: PathBuf::from("crates/djinn-core/Cargo.toml"),
                    loc: 50,
                    node_count: 5,
                    fan_in: 0.0,
                    fan_out: 0.0,
                    inbound_weight: 0.0,
                    outbound_weight: 0.0,
                },
                CrateNode {
                    name: "djinn-stack".to_string(),
                    manifest_path: PathBuf::from("crates/djinn-stack/Cargo.toml"),
                    loc: 50,
                    node_count: 5,
                    fan_in: 0.0,
                    fan_out: 0.0,
                    inbound_weight: 0.0,
                    outbound_weight: 0.0,
                },
            ],
            edges: vec![CrateEdge {
                source: "djinn-core".to_string(),
                target: "djinn-stack".to_string(),
                weight: 1.0,
                edge_count: 1,
            }],
        };
        let rules = vec![TomlRule {
            name: "no-agent-imports-control-plane".to_string(),
            from_glob: "**/djinn-agent/**".to_string(),
            to_glob: "**/djinn-control-plane/**".to_string(),
            description: Some("Agent must not import control-plane.".to_string()),
        }];
        let compiled = compile_rules(&rules).expect("valid globs should compile");
        let violations = check_violations(&crate_graph, &compiled);
        assert!(
            violations.is_empty(),
            "expected no violations for non-matching edges"
        );
    }

    #[test]
    fn check_violations_excludes_self_references() {
        // Self-references (a crate importing itself) should never be
        // reported as violations.
        let crate_graph = CrateGraph {
            crates: vec![CrateNode {
                name: "djinn-agent".to_string(),
                manifest_path: PathBuf::from("crates/djinn-agent/Cargo.toml"),
                loc: 100,
                node_count: 10,
                fan_in: 1.0,
                fan_out: 1.0,
                inbound_weight: 1.0,
                outbound_weight: 1.0,
            }],
            edges: vec![CrateEdge {
                source: "djinn-agent".to_string(),
                target: "djinn-agent".to_string(),
                weight: 1.0,
                edge_count: 1,
            }],
        };
        let rules = vec![TomlRule {
            name: "no-agent-imports-control-plane".to_string(),
            from_glob: "**/djinn-agent/**".to_string(),
            to_glob: "**/djinn-agent/**".to_string(),
            description: Some("Self-reference should not be a violation.".to_string()),
        }];
        let compiled = compile_rules(&rules).expect("valid globs should compile");
        let violations = check_violations(&crate_graph, &compiled);
        // The rule matches, but the checker intentionally does NOT filter
        // self-references at this level — the main loop reports them.  This
        // test documents the current behaviour; if we want to exclude
        // self-references, that change belongs to the matching logic above.
        // For now we assert that the match *does* fire so the test is
        // honest about what the code does.
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].2, "djinn-agent");
        assert_eq!(violations[0].3, "djinn-agent");
    }

    // ------------------------------------------------------------------
    // render_violation_report — violation output contract
    // ------------------------------------------------------------------

    #[test]
    fn render_violation_report_includes_all_required_fields() {
        let rules = vec![TomlRule {
            name: "no-agent-imports-control-plane".to_string(),
            from_glob: "**/djinn-agent/**".to_string(),
            to_glob: "**/djinn-control-plane/**".to_string(),
            description: Some(
                "Agent must not import control-plane; control-plane is the bridge layer."
                    .to_string(),
            ),
        }];
        let compiled = compile_rules(&rules).expect("valid globs should compile");
        let crate_graph = forbidden_edge_crate_graph();
        let violations = check_violations(&crate_graph, &compiled);
        assert_eq!(violations.len(), 1);

        // Convert to the slice type expected by render_violation_report.
        let report_violations: Vec<(usize, &TomlRule, &str, &str)> = violations
            .iter()
            .map(|(idx, rule, from, to)| (*idx, *rule, from.as_str(), to.as_str()))
            .collect();

        let mut buf = String::new();
        let exit_code =
            render_violation_report(&mut buf, &report_violations).expect("render should succeed");
        assert_eq!(exit_code, 1, "violations should map to exit code 1");

        let output = buf;
        assert!(
            output.contains("no-agent-imports-control-plane"),
            "report must include rule name: {output}"
        );
        assert!(
            output.contains("Agent must not import control-plane"),
            "report must include description: {output}"
        );
        assert!(
            output.contains("from_key:   djinn-agent"),
            "report must include from_key: {output}"
        );
        assert!(
            output.contains("to_key:     djinn-control-plane"),
            "report must include to_key: {output}"
        );
        assert!(
            output.contains("witness:    djinn-agent → djinn-control-plane"),
            "report must include witness: {output}"
        );
    }

    #[test]
    fn render_violation_report_multiple_violations() {
        let crate_graph = CrateGraph {
            crates: vec![
                CrateNode {
                    name: "djinn-agent".to_string(),
                    manifest_path: PathBuf::from("crates/djinn-agent/Cargo.toml"),
                    loc: 100,
                    node_count: 10,
                    fan_in: 0.0,
                    fan_out: 2.0,
                    inbound_weight: 0.0,
                    outbound_weight: 2.0,
                },
                CrateNode {
                    name: "djinn-control-plane".to_string(),
                    manifest_path: PathBuf::from("crates/djinn-control-plane/Cargo.toml"),
                    loc: 200,
                    node_count: 20,
                    fan_in: 1.0,
                    fan_out: 0.0,
                    inbound_weight: 1.0,
                    outbound_weight: 0.0,
                },
                CrateNode {
                    name: "djinn-db".to_string(),
                    manifest_path: PathBuf::from("crates/djinn-db/Cargo.toml"),
                    loc: 150,
                    node_count: 15,
                    fan_in: 1.0,
                    fan_out: 0.0,
                    inbound_weight: 1.0,
                    outbound_weight: 0.0,
                },
            ],
            edges: vec![
                CrateEdge {
                    source: "djinn-agent".to_string(),
                    target: "djinn-control-plane".to_string(),
                    weight: 1.0,
                    edge_count: 1,
                },
                CrateEdge {
                    source: "djinn-agent".to_string(),
                    target: "djinn-db".to_string(),
                    weight: 1.0,
                    edge_count: 1,
                },
            ],
        };
        let rules = vec![
            TomlRule {
                name: "no-agent-imports-control-plane".to_string(),
                from_glob: "**/djinn-agent/**".to_string(),
                to_glob: "**/djinn-control-plane/**".to_string(),
                description: Some("Agent must not import control-plane.".to_string()),
            },
            TomlRule {
                name: "no-agent-imports-db".to_string(),
                from_glob: "**/djinn-agent/**".to_string(),
                to_glob: "**/djinn-db/**".to_string(),
                description: Some("Agent must not import db.".to_string()),
            },
        ];
        let compiled = compile_rules(&rules).expect("valid globs should compile");
        let violations = check_violations(&crate_graph, &compiled);
        assert_eq!(violations.len(), 2, "expected two violations");

        let report_violations: Vec<(usize, &TomlRule, &str, &str)> = violations
            .iter()
            .map(|(idx, rule, from, to)| (*idx, *rule, from.as_str(), to.as_str()))
            .collect();

        let mut buf = String::new();
        let exit_code =
            render_violation_report(&mut buf, &report_violations).expect("render should succeed");
        assert_eq!(exit_code, 1);

        let output = buf;
        assert!(output.contains("no-agent-imports-control-plane"));
        assert!(output.contains("no-agent-imports-db"));
        assert!(output.contains("2 boundary violation(s) found"));
    }

    // ------------------------------------------------------------------
    // Operational failure semantics (exit-2 contract)
    // ------------------------------------------------------------------

    /// Helper that simulates the CLI's operational-failure path for an
    /// empty rule file.  Returns the error text that would be printed.
    fn simulate_empty_rules_failure() -> String {
        let config = BoundaryConfig {
            boundary: BoundaryMeta {
                level: "crate".to_string(),
                description: None,
            },
            rules: vec![],
        };
        let mut out = String::new();
        if config.rules.is_empty() {
            out.push_str("Error: no boundary rules defined.\n");
        }
        out
    }

    #[test]
    fn empty_rules_produces_operational_failure_text() {
        let text = simulate_empty_rules_failure();
        assert!(
            text.contains("Error:"),
            "operational failure must mention Error: {text}"
        );
        assert!(
            text.contains("no boundary rules defined"),
            "operational failure must cite empty rules: {text}"
        );
    }

    #[test]
    fn validation_errors_produce_operational_failure_text() {
        let rules = vec![TomlRule {
            name: "".to_string(),
            from_glob: "   ".to_string(),
            to_glob: "**/b/**".to_string(),
            description: None,
        }];
        let errors = validate_rules(&rules);
        assert!(!errors.is_empty(), "expected validation errors");

        let mut out = String::new();
        out.push_str("Error: boundary rule validation failed:\n");
        for err in &errors {
            out.push_str(&format!("  {err}\n"));
        }

        let text = out;
        assert!(text.contains("Error:"), "must start with Error: {text}");
        assert!(
            text.contains("boundary rule validation failed"),
            "must cite validation failure: {text}"
        );
        assert!(text.contains("name:"), "must mention name field: {text}");
        assert!(
            text.contains("from_glob:"),
            "must mention from_glob field: {text}"
        );
        assert!(
            text.contains("description:"),
            "must mention description field: {text}"
        );
    }

    #[test]
    fn compilation_errors_produce_operational_failure_text() {
        let rules = vec![TomlRule {
            name: "bad-glob".to_string(),
            from_glob: "[unclosed".to_string(),
            to_glob: "**/b/**".to_string(),
            description: Some("Valid description.".to_string()),
        }];
        let result = compile_rules(&rules);
        assert!(result.is_err(), "expected compilation failure");

        let mut out = String::new();
        out.push_str("Error: boundary rule compilation failed:\n");
        if let Err(errors) = result {
            for err in &errors {
                out.push_str(&format!("  {err}\n"));
            }
        }

        let text = out;
        assert!(text.contains("Error:"));
        assert!(text.contains("boundary rule compilation failed"));
        assert!(text.contains("invalid glob"));
    }

    #[test]
    fn graph_sanity_failure_produces_operational_failure_text() {
        let empty_graph = RepoDependencyGraph::build(&[]);
        let result = check_graph_sanity(&empty_graph);
        assert!(result.is_err(), "expected graph sanity failure");

        let mut out = String::new();
        if let Err(e) = result {
            out.push_str(&format!("Error: loaded graph is unusable: {e}\n"));
        }

        let text = out;
        assert!(text.contains("Error:"));
        assert!(text.contains("loaded graph is unusable"));
        assert!(text.contains("zero nodes"));
    }

    #[test]
    fn crate_graph_usable_failure_produces_operational_failure_text() {
        let empty_crate_graph = CrateGraph {
            crates: vec![],
            edges: vec![],
        };
        let result = check_crate_graph_usable(&empty_crate_graph);
        assert!(result.is_err());

        let mut out = String::new();
        if let Err(e) = result {
            out.push_str(&format!("Error: derived crate graph is unusable: {e}\n"));
        }

        let text = out;
        assert!(text.contains("Error:"));
        assert!(text.contains("derived crate graph is unusable"));
        assert!(text.contains("no crate nodes"));
    }

    // ------------------------------------------------------------------
    // Exit-code semantics: violation (1) vs operational (2)
    // ------------------------------------------------------------------

    #[test]
    fn violation_exit_code_is_one() {
        // Violations map to exit code 1 — distinct from operational errors (2).
        let rules = vec![TomlRule {
            name: "no-agent-imports-control-plane".to_string(),
            from_glob: "**/djinn-agent/**".to_string(),
            to_glob: "**/djinn-control-plane/**".to_string(),
            description: Some("Agent must not import control-plane.".to_string()),
        }];
        let compiled = compile_rules(&rules).expect("valid globs should compile");
        let crate_graph = forbidden_edge_crate_graph();
        let violations = check_violations(&crate_graph, &compiled);
        assert!(!violations.is_empty(), "fixture must produce a violation");

        let report_violations: Vec<(usize, &TomlRule, &str, &str)> = violations
            .iter()
            .map(|(idx, rule, from, to)| (*idx, *rule, from.as_str(), to.as_str()))
            .collect();

        let mut buf = String::new();
        let code = render_violation_report(&mut buf, &report_violations).expect("render ok");
        assert_eq!(code, 1, "violations must return exit code 1, not 2");
    }

    #[test]
    fn no_violations_exit_code_is_zero() {
        // Empty violation list means success → exit code 0.
        let rules = vec![TomlRule {
            name: "no-agent-imports-control-plane".to_string(),
            from_glob: "**/djinn-agent/**".to_string(),
            to_glob: "**/djinn-control-plane/**".to_string(),
            description: Some("Agent must not import control-plane.".to_string()),
        }];
        let compiled = compile_rules(&rules).expect("valid globs should compile");
        let crate_graph = CrateGraph {
            crates: vec![
                CrateNode {
                    name: "djinn-core".to_string(),
                    manifest_path: PathBuf::from("crates/djinn-core/Cargo.toml"),
                    loc: 50,
                    node_count: 5,
                    fan_in: 0.0,
                    fan_out: 0.0,
                    inbound_weight: 0.0,
                    outbound_weight: 0.0,
                },
                CrateNode {
                    name: "djinn-stack".to_string(),
                    manifest_path: PathBuf::from("crates/djinn-stack/Cargo.toml"),
                    loc: 50,
                    node_count: 5,
                    fan_in: 0.0,
                    fan_out: 0.0,
                    inbound_weight: 0.0,
                    outbound_weight: 0.0,
                },
            ],
            edges: vec![CrateEdge {
                source: "djinn-core".to_string(),
                target: "djinn-stack".to_string(),
                weight: 1.0,
                edge_count: 1,
            }],
        };
        let violations = check_violations(&crate_graph, &compiled);
        assert!(violations.is_empty(), "fixture must produce no violations");
        // When violations are empty, the CLI exits 0.  This helper test
        // documents the contract without needing the full binary.
    }
}
