mod deterministic_embeddings;
mod fixtures;
mod metrics;
mod report;

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
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Run => not_yet_implemented("run", "qmzw"),
        Commands::Compare => not_yet_implemented("compare", "zd4o"),
        Commands::MineMemoryRefs => not_yet_implemented("mine-memory-refs", "qmzw"),
        Commands::RefreshBaseline => not_yet_implemented("refresh-baseline", "zd4o"),
    }
}

/// Returns a clear not-yet-implemented error identifying the downstream
/// task that will provide the real implementation.
fn not_yet_implemented(subcommand: &str, task_ref: &str) -> anyhow::Result<()> {
    anyhow::bail!(
        "Subcommand `{subcommand}` is not yet implemented. \
         Tracked by task {task_ref} in epic nih4 (Phase 1). \
         See the djinn-memory-eval README for the implementation roadmap."
    )
}
