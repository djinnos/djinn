mod deterministic_embeddings;
mod fixtures;
mod loader;
mod metrics;
mod report;
mod run;

use clap::{Parser, Subcommand};
use tracing::info;

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
        Commands::Run => {
            let output = run::execute_run(&crate_root).await?;

            // Log summary
            info!("=== Phase 1 Benchmark Run ===");
            info!(notes = output.corpus_note_count, "corpus");
            info!(queries = output.query_count, "queries executed");
            info!(bad_cases = output.bad_case_count, "bad cases executed");
            info!(
                comparisons = output.signal_comparisons.len(),
                "signal comparisons"
            );

            info!("--- Query Results ---");
            for record in &output.query_records {
                let found = record.relevant_ranks.iter().filter(|r| r.is_some()).count();
                info!(
                    query_id = %record.query_id,
                    found,
                    expected = record.expected_permalinks.len(),
                    top = ?record.result_permalinks.first(),
                    "query result"
                );
                for (i, rank) in record.relevant_ranks.iter().enumerate() {
                    if let Some(r) = rank {
                        info!(
                            idx = i,
                            permalink = %record.expected_permalinks[i],
                            rank = r,
                            "relevant note found"
                        );
                    } else {
                        info!(
                            idx = i,
                            permalink = %record.expected_permalinks[i],
                            "relevant note NOT FOUND"
                        );
                    }
                }
            }

            info!("--- Bad Case Results ---");
            for record in &output.bad_case_records {
                let found = record.relevant_ranks.iter().filter(|r| r.is_some()).count();
                info!(
                    case_id = %record.query_id,
                    case_type = ?record.bad_case_type,
                    found,
                    expected = record.expected_permalinks.len(),
                    "bad case result"
                );
            }

            info!("--- Signal Comparisons ---");
            for comp in &output.signal_comparisons {
                info!(
                    query_id = %comp.query_id,
                    signal = %comp.signal,
                    rank_with = ?comp.rank_with_signal,
                    rank_without = ?comp.rank_without_signal,
                    changed = comp.rank_changed,
                    "signal comparison"
                );
            }

            // Write JSON output to target directory
            let target_dir = std::path::PathBuf::from("target/memory-eval");
            std::fs::create_dir_all(&target_dir)?;
            let report_path = target_dir.join("phase1-run-output.json");
            let json = serde_json::to_string_pretty(&output)?;
            std::fs::write(&report_path, &json)?;
            info!(path = %report_path.display(), "run output written");

            Ok(())
        }
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
