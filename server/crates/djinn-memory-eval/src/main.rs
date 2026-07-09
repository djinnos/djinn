mod deterministic_embeddings;
mod fixtures;
mod loader;
mod metrics;
mod report;
mod run;

use std::collections::HashMap;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use djinn_core::clock::{Clock, SystemClock};
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
        Commands::Run => cmd_run(&crate_root).await,
        Commands::Compare => cmd_compare(&crate_root).await,
        Commands::MineMemoryRefs => not_yet_implemented("mine-memory-refs", "qmzw"),
        Commands::RefreshBaseline => cmd_refresh_baseline(&crate_root).await,
    }
}

/// Run the Phase 1 benchmark, compute metrics, write reports.
async fn cmd_run(crate_root: &std::path::Path) -> Result<()> {
    // 1. Execute the benchmark run
    let output = run::execute_run(crate_root).await?;

    // Log summary
    info!("=== Phase 1 Benchmark Run ===");
    info!(notes = output.corpus_note_count, "corpus");
    info!(queries = output.query_count, "queries executed");
    info!(bad_cases = output.bad_case_count, "bad cases executed");
    info!(
        comparisons = output.signal_comparisons.len(),
        "signal comparisons"
    );

    // 2. Build per-query result records with age data
    let fixtures =
        loader::load_fixtures_from_disk(crate_root).context("loading fixtures for age data")?;

    // Build age map: permalink → age in days (from last_accessed)
    let clock = SystemClock::new();
    let now = clock
        .now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut note_ages: HashMap<String, u32> = HashMap::new();
    for note in &fixtures.corpus_notes {
        if let Ok(last_accessed) = parse_iso8601_epoch(&note.timestamps.last_accessed) {
            let age_secs = now.saturating_sub(last_accessed);
            note_ages.insert(note.permalink.clone(), (age_secs / 86400) as u32);
        }
    }

    // Build query result records
    let all_records: Vec<&run::QueryRankRecord> = output
        .query_records
        .iter()
        .chain(output.bad_case_records.iter())
        .collect();

    let query_result_records: Vec<metrics::QueryResultRecord> = all_records
        .iter()
        .map(|r| build_query_result_record(r, &note_ages))
        .collect();

    // 3. Compute metrics
    let all_query_metrics = metrics::compute_suite_metrics(&output.query_records);
    let bad_case_metrics = metrics::compute_suite_metrics(&output.bad_case_records);

    let mut suite_metrics = HashMap::new();
    if !output.query_records.is_empty() {
        suite_metrics.insert("all_queries".to_string(), all_query_metrics.clone());
    }
    if !output.bad_case_records.is_empty() {
        suite_metrics.insert("bad_cases".to_string(), bad_case_metrics.clone());
    }

    let agg_suites: Vec<(&str, &metrics::SuiteMetrics)> =
        suite_metrics.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let aggregate_metrics = metrics::compute_aggregate_metrics(&agg_suites);

    // Age-bucket recall
    let age_bucket_recall = metrics::compute_age_bucket_recall(&output.query_records, &note_ages);

    // Directional metrics (non-gating)
    let directional = metrics::directional_metrics(&output.query_records);

    // 4. Log metrics
    info!("--- Metrics ---");
    info!(
        recall_at_1 = format!("{:.4}", aggregate_metrics.recall_at_1),
        "aggregate recall@1"
    );
    info!(
        recall_at_5 = format!("{:.4}", aggregate_metrics.recall_at_5),
        "aggregate recall@5"
    );
    info!(
        recall_at_10 = format!("{:.4}", aggregate_metrics.recall_at_10),
        "aggregate recall@10"
    );
    info!(
        mrr = format!("{:.4}", aggregate_metrics.mrr),
        "aggregate MRR"
    );
    info!(
        zr = format!("{:.4}", aggregate_metrics.zero_result_rate),
        "aggregate zero-result rate"
    );
    info!(
        label = %directional.label,
        precision = format!("{:.4}", directional.avg_precision_at_10),
        f1 = format!("{:.4}", directional.avg_f1_at_10),
        "directional (non-gating)"
    );

    // 5. Load fixture hashes if available
    let fixture_hashes = fixtures.manifest.as_ref().map(|m| m.file_hashes.clone());

    // 6. Build and write report
    let report = report::Phase1Report {
        suite_metrics,
        aggregate_metrics,
        age_bucket_recall,
        directional,
        query_records: query_result_records,
        signal_comparisons: output.signal_comparisons.clone(),
        compare_result: None,
        threshold_policy_version: metrics::THRESHOLD_POLICY_VERSION.to_string(),
        fixture_hashes,
    };

    let target_dir = std::path::PathBuf::from("target/memory-eval");
    report::write_reports(&report, &target_dir)?;
    info!(path = %target_dir.display(), "reports written");

    Ok(())
}

/// Compare current metrics against the committed baseline.
///
/// Loads the current report from `target/memory-eval/phase1-report.json`
/// (produced by a prior `run` command), loads the committed baseline,
/// evaluates the compare policy, and writes updated reports.
///
/// Exits non-zero (via Err) if any gating threshold fails.
async fn cmd_compare(crate_root: &std::path::Path) -> Result<()> {
    // 1. Load current report
    let target_dir = std::path::PathBuf::from("target/memory-eval");
    let report_path = target_dir.join("phase1-report.json");
    let report_data = std::fs::read_to_string(&report_path).with_context(|| {
        format!(
            "reading current report at {}. Run `djinn-memory-eval run` first.",
            report_path.display()
        )
    })?;
    let mut current_report: report::Phase1Report =
        serde_json::from_str(&report_data).context("parsing current report JSON")?;

    // 2. Load baseline
    let baseline = report::load_baseline(crate_root).context("loading committed baseline")?;

    info!("=== Phase 1 Compare ===");
    info!(
        baseline_commit = %baseline.metadata.refresh_commit,
        baseline_created = %baseline.metadata.created_at,
        "loaded baseline"
    );

    // 3. Compute current aggregate metrics from report
    let current_aggregate = &current_report.aggregate_metrics;

    // 4. Get bad-case records from the report
    let bad_case_records: Vec<run::QueryRankRecord> = current_report
        .query_records
        .iter()
        .filter(|r| r.is_bad_case)
        .map(|r| run::QueryRankRecord {
            query_id: r.query_id.clone(),
            query_text: r.query_text.clone(),
            task_id: None,
            result_permalinks: r.result_permalinks.clone(),
            relevant_ranks: r.relevant_ranks.clone(),
            expected_permalinks: r.expected_permalinks.clone(),
            is_bad_case: r.is_bad_case,
            bad_case_type: r.bad_case_type.clone(),
        })
        .collect();

    // 5. Evaluate compare policy
    let compare_result = metrics::evaluate_compare_policy(
        &current_report.suite_metrics,
        current_aggregate,
        &bad_case_records,
        &baseline.suite_metrics,
        &baseline.aggregate_metrics,
        baseline
            .suite_metrics
            .get("bad_cases")
            .map(|m| m.zero_result_rate)
            .unwrap_or(0.0),
    );

    info!(
        passed = compare_result.passed,
        failures = compare_result.failures.len(),
        query_regressions = compare_result.query_regressions.len(),
        "compare result"
    );

    for failure in &compare_result.failures {
        tracing::warn!(
            metric = %failure.metric,
            suite = %failure.suite,
            baseline = failure.baseline_value,
            current = failure.current_value,
            delta = failure.delta,
            "gating failure"
        );
    }

    // 6. Annotate report with compare result and re-write
    current_report.compare_result = Some(compare_result.clone());
    report::write_reports(&current_report, &target_dir)?;

    info!(path = %target_dir.display(), "updated reports written");

    // 7. Exit non-zero if compare failed
    if !compare_result.passed {
        anyhow::bail!(
            "Phase 1 compare FAILED with {} gating failure(s). See {} for details.",
            compare_result.failures.len(),
            target_dir.join("phase1-summary.md").display()
        );
    }

    info!("Phase 1 compare PASSED");
    Ok(())
}

/// Refresh the committed baseline with current metric results.
///
/// Loads the current report from `target/memory-eval/phase1-report.json`
/// and writes it as the new baseline in `baselines/phase1.json`.
async fn cmd_refresh_baseline(crate_root: &std::path::Path) -> Result<()> {
    // 1. Load current report
    let target_dir = std::path::PathBuf::from("target/memory-eval");
    let report_path = target_dir.join("phase1-report.json");
    let report_data = std::fs::read_to_string(&report_path).with_context(|| {
        format!(
            "reading current report at {}. Run `djinn-memory-eval run` first.",
            report_path.display()
        )
    })?;
    let current_report: report::Phase1Report =
        serde_json::from_str(&report_data).context("parsing current report JSON")?;

    // 2. Determine refresh commit via djinn-git (capability boundary)
    let repo_root = crate_root
        .ancestors()
        .nth(3)
        .unwrap_or(crate_root)
        .to_path_buf();
    let refresh_commit = djinn_git::head_commit_sha(&repo_root)
        .await
        .unwrap_or_else(|_| "unknown".to_string());

    info!("=== Refresh Baseline ===");
    info!(commit = %refresh_commit, "refresh commit");

    // 3. Build baseline
    let clock = SystemClock::new();
    let epoch_secs = clock
        .now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let baseline = report::build_baseline(
        &current_report,
        current_report.fixture_hashes.clone(),
        refresh_commit,
        epoch_secs,
    );

    // 4. Write baseline
    report::write_baseline(crate_root, &baseline)?;

    info!(
        path = %crate_root.join("baselines/phase1.json").display(),
        "baseline refreshed"
    );

    Ok(())
}

/// Build a `QueryResultRecord` from a `QueryRankRecord` with age data.
fn build_query_result_record(
    record: &run::QueryRankRecord,
    note_ages: &HashMap<String, u32>,
) -> metrics::QueryResultRecord {
    let note_ages_days: Vec<u32> = record
        .expected_permalinks
        .iter()
        .map(|p| note_ages.get(p).copied().unwrap_or(0))
        .collect();

    let best_rank = record.relevant_ranks.iter().filter_map(|r| *r).min();

    metrics::QueryResultRecord {
        query_id: record.query_id.clone(),
        query_text: record.query_text.clone(),
        expected_permalinks: record.expected_permalinks.clone(),
        result_permalinks: record.result_permalinks.clone(),
        best_rank,
        relevant_ranks: record.relevant_ranks.clone(),
        is_bad_case: record.is_bad_case,
        bad_case_type: record.bad_case_type.clone(),
        note_ages_days,
    }
}

/// Parse an ISO-8601 timestamp to epoch seconds.
///
/// Supports formats: "2026-01-01T00:00:00.000Z", "2026-01-01T00:00:00Z"
fn parse_iso8601_epoch(s: &str) -> Result<u64> {
    // Strip trailing Z and milliseconds
    let s = s.trim_end_matches('Z');
    let s = if let Some(dot_pos) = s.find('.') {
        &s[..dot_pos]
    } else {
        s
    };

    // Parse "YYYY-MM-DDTHH:MM:SS"
    let parts: Vec<&str> = s.split('T').collect();
    if parts.len() != 2 {
        anyhow::bail!("invalid ISO-8601 format: {}", s);
    }

    let date_parts: Vec<&str> = parts[0].split('-').collect();
    let time_parts: Vec<&str> = parts[1].split(':').collect();

    if date_parts.len() != 3 || time_parts.len() != 3 {
        anyhow::bail!("invalid ISO-8601 format: {}", s);
    }

    let year: i64 = date_parts[0].parse().context("parsing year")?;
    let month: u64 = date_parts[1].parse().context("parsing month")?;
    let day: u64 = date_parts[2].parse().context("parsing day")?;
    let hour: u64 = time_parts[0].parse().context("parsing hour")?;
    let minute: u64 = time_parts[1].parse().context("parsing minute")?;
    let second: u64 = time_parts[2].parse().context("parsing second")?;

    // Civil date to days since epoch (Howard Hinnant algorithm)
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 12 } else { month };
    let d = day as i64;

    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (153 * (m - 3) + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe as i64 - 719468;

    let epoch = days as u64 * 86400 + hour * 3600 + minute * 60 + second;
    Ok(epoch)
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
