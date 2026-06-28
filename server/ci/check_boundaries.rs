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
fn compile_rules(
    rules: &[TomlRule],
) -> Result<Vec<CompiledRule<'_>>, Vec<RuleValidationError>> {
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

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

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

    // 4. Derive the crate map from the index-tree checkout (where Cargo.toml lives).
    let (_project_root, index_tree_path) =
        djinn_graph::canonical_graph::normalize_graph_query_paths(&cli.project_path);
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

    let mut violations: Vec<(usize, &TomlRule, &str, &str)> = Vec::new();
    for edge in &crate_graph.edges {
        for cr in &compiled {
            if cr.from_matcher.is_match(&edge.source) && cr.to_matcher.is_match(&edge.target) {
                violations.push((cr.index, cr.rule, &edge.source, &edge.target));
            }
        }
    }

    // 7. Report.
    if violations.is_empty() {
        println!(
            "✓ No boundary violations found. (checked {} rule(s) against {} crate edge(s))",
            config.rules.len(),
            crate_graph.edges.len()
        );
        std::process::exit(0);
    }

    eprintln!("✗ {} boundary violation(s) found:\n", violations.len());
    for (i, rule, from, to) in &violations {
        eprintln!("  [{rule_index}] {from} → {to}", rule_index = i);
        eprintln!("      rule name:  {}", rule.name);
        if let Some(desc) = &rule.description {
            eprintln!("      description: {desc}");
        }
        eprintln!("      from_key:   {from}");
        eprintln!("      to_key:     {to}");
        eprintln!("      witness:    {from} → {to}");
        eprintln!();
    }
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
