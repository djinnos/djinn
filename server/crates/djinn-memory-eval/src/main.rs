mod commands;
mod deterministic_embeddings;
mod fixtures;
pub mod injection_probe;
pub mod injection_ranking;
mod loader;
mod metrics;
pub mod qa;
pub mod qa_judge;
pub mod qa_run;
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
    /// Run the Phase 2 QA execution: extract QA pairs from pitfall/case
    /// notes, run each question through real NoteRepository::search with
    /// top-k 10, render results through format_knowledge_notes(2000), and
    /// record retrieval hit, gold rank, context recall, and age bucket.
    QaRun,
    /// Run the Phase 2 credentialed dual-pass QA judge.
    ///
    /// This command is intended for nightly/manual use only. It writes the
    /// Phase 2 JSON/Markdown artifacts even when credentials are missing, then
    /// exits non-zero so the non-gating workflow can surface the provider error
    /// without making Phase 2 a PR or merge-queue gate.
    QaJudge,
    /// Run the session-start injection probe: exercise
    /// `NoteRepository::query_by_scope_overlap` against the committed fixtures
    /// and render the result through `pack_ranked_knowledge_notes` under the
    /// shipped default injection settings, reporting the final packed prompt
    /// text rather than repository ranking alone. Deterministic, no LLM calls.
    InjectionProbe {
        /// Task scope paths the scope-overlap query runs with. Repeatable.
        #[arg(long = "task-path")]
        task_paths: Vec<String>,
    },
    /// Evaluate knowledge-injection ranking against a judged corpus
    /// (proposal `5205`).
    ///
    /// The corpus is deliberately NOT committed to this repository: relevance
    /// judgments, production trace IDs, and a captured baseline are
    /// per-deployment empirical artifacts. Each operator supplies their own via
    /// `--manifest`; the default location is git-ignored.
    ///
    /// Exits non-zero on any oracle-integrity failure (missing or malformed
    /// manifest, any recorded hash not matching the bytes on disk, missing
    /// provenance, a manifest recording the identity of its own commit, or a
    /// cutoff/window/budget outside the contract), on an nDCG@10 improvement
    /// below `--require-ndcg-delta`, on a Recall@10 drop above
    /// `--max-recall-drop`, on any repeated ordering or disposition mismatch,
    /// on a packed result above the manifest byte ceiling, and on a
    /// no-backfill regression.
    InjectionRanking {
        /// Path to the corpus manifest. Defaults to the git-ignored
        /// `fixtures/local/injection-ranking-v1.manifest.json`.
        #[arg(long)]
        manifest: Option<std::path::PathBuf>,
        /// How many times each case is replayed to prove determinism.
        #[arg(long, default_value_t = 1)]
        repeat: usize,
        /// Required absolute macro nDCG@10 improvement over the baseline.
        #[arg(long = "require-ndcg-delta", default_value_t = 0.10)]
        require_ndcg_delta: f64,
        /// Largest tolerated absolute macro Recall@10 drop below the baseline.
        #[arg(long = "max-recall-drop", default_value_t = 0.02)]
        max_recall_drop: f64,
    },
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
        Commands::QaRun => commands::cmd_qa_run(&crate_root).await,
        Commands::QaJudge => commands::cmd_qa_judge(&crate_root).await,
        Commands::InjectionProbe { task_paths } => {
            commands::cmd_injection_probe(&crate_root, &task_paths).await
        }
        Commands::InjectionRanking {
            manifest,
            repeat,
            require_ndcg_delta,
            max_recall_drop,
        } => injection_ranking::cmd_injection_ranking(
            &crate_root,
            manifest,
            injection_ranking::Thresholds {
                repeat,
                require_ndcg_delta,
                max_recall_drop,
            },
        ),
    }
}
