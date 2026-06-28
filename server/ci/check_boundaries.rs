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
#[derive(Deserialize)]
struct TomlRule {
    name: String,
    from_glob: String,
    to_glob: String,
    #[serde(default)]
    description: Option<String>,
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
        println!(
            "No boundary rules defined in '{}' — nothing to check.",
            cli.rules.display()
        );
        std::process::exit(0);
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
    let compiled: Vec<(usize, &TomlRule, globset::GlobMatcher, globset::GlobMatcher)> = config
        .rules
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let from = Glob::new(&normalise_crate_glob(&r.from_glob))
                .map_err(|e| {
                    eprintln!(
                        "Warning: rule[{}] '{}' has invalid from_glob '{}': {e}",
                        i, r.name, r.from_glob
                    );
                    e
                })
                .ok()?
                .compile_matcher();
            let to = Glob::new(&normalise_crate_glob(&r.to_glob))
                .map_err(|e| {
                    eprintln!(
                        "Warning: rule[{}] '{}' has invalid to_glob '{}': {e}",
                        i, r.name, r.to_glob
                    );
                    e
                })
                .ok()?
                .compile_matcher();
            Some((i, r, from, to))
        })
        .collect();

    let mut violations: Vec<(usize, &TomlRule, &str, &str)> = Vec::new();
    for edge in &crate_graph.edges {
        for &(i, rule, ref from_m, ref to_m) in &compiled {
            if from_m.is_match(&edge.source) && to_m.is_match(&edge.target) {
                violations.push((i, rule, &edge.source, &edge.target));
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
}
