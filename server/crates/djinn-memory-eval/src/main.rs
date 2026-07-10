mod commands;
mod deterministic_embeddings;
mod fixtures;
mod loader;
mod metrics;
mod report;
mod run;

use clap::{Parser, Subcommand};

/// Deterministic real-pipeline memory rank benchmark and PR gate.
///
/// Phase 1: loads committed JSONL fixtures into dedicated Postgres,
/// exercises the real NoteRepository::search and build_context paths,
/// computes rank metrics, and compares against a committed baseline.
#[derive(Parser, Debug)]
#[command(name = "djinn-memory-eval", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the Phase 1 benchmark against fixtures and produce metrics.
    Run,
    /// Compare current metrics against the committed baseline.
    Compare,
    /// Mine memory_refs rows from tasks and proposals for fixture generation.
    MineMemoryRefs,
    /// Refresh the committed baseline with current metric results.
    RefreshBaseline,
    /// Validate committed fixtures and baseline without running the pipeline.
    /// No LLM calls or external network required.
    ValidateFixtures,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // Resolve crate root (directory containing Cargo.toml)
    let crate_root = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()),
    );

    match cli.command {
        Commands::Run => commands::cmd_run(&crate_root).await,
        Commands::Compare => commands::cmd_compare(&crate_root).await,
        Commands::MineMemoryRefs => commands::not_yet_implemented("mine-memory-refs", "qmzw"),
        Commands::RefreshBaseline => commands::cmd_refresh_baseline(&crate_root).await,
        Commands::ValidateFixtures => commands::cmd_validate_fixtures(&crate_root),
    }
}
