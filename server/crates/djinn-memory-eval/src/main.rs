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
        Commands::Run => cmd_run(&crate_root).await,
        Commands::Compare => cmd_compare(&crate_root).await,
        Commands::MineMemoryRefs => not_yet_implemented("mine-memory-refs", "qmzw"),
        Commands::RefreshBaseline => cmd_refresh_baseline(&crate_root).await,
        Commands::ValidateFixtures => cmd_validate_fixtures(&crate_root),
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
        &baseline.per_query_ranks,
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
/// Validate committed fixtures and baseline without running the pipeline.
///
/// This is a focused validation command that checks:
/// 1. All fixture JSONL files parse correctly under the loader schema.
/// 2. Fixture cross-references are valid (permalinks, graph edges).
/// 3. Signal coverage data is present for all claimed signals.
/// 4. The committed baseline exists and has correct structure.
/// 5. Fixture hashes in the baseline match the committed manifest.
///
/// No LLM calls or external network are required.
fn cmd_validate_fixtures(crate_root: &std::path::Path) -> Result<()> {
    info!("=== Validate Fixtures ===");

    // 1. Load fixtures from disk
    let fixtures =
        loader::load_fixtures_from_disk(crate_root).context("loading fixtures from disk")?;

    info!(
        corpus = fixtures.corpus_notes.len(),
        queries = fixtures.memory_ref_queries.len(),
        bad_cases = fixtures.bad_cases.len(),
        "fixtures loaded"
    );

    // 2. Validate fixtures (schema, cross-references, signal coverage)
    loader::validate_fixtures(&fixtures).context("fixture validation failed")?;

    info!("fixture validation passed");

    // 3. Check query counts
    let task_affinity_queries = fixtures
        .memory_ref_queries
        .iter()
        .filter(|q| q.task_id.is_some())
        .count();
    let total_queries = fixtures.memory_ref_queries.len() + fixtures.bad_cases.len();
    info!(
        total_labeled = total_queries,
        memory_ref = fixtures.memory_ref_queries.len(),
        task_affinity = task_affinity_queries,
        bad_cases = fixtures.bad_cases.len(),
        "query counts"
    );

    // Require at least 25 total labeled queries
    if total_queries < 25 {
        anyhow::bail!(
            "expected at least 25 total labeled queries, got {}",
            total_queries
        );
    }

    // Require at least 15 mined memory_refs rows
    if fixtures.memory_ref_queries.len() < 15 {
        anyhow::bail!(
            "expected at least 15 mined memory_refs queries, got {}",
            fixtures.memory_ref_queries.len()
        );
    }

    // Require at least 10 bad-case rows
    if fixtures.bad_cases.len() < 10 {
        anyhow::bail!(
            "expected at least 10 bad-case rows, got {}",
            fixtures.bad_cases.len()
        );
    }

    // 4. Check required coverage types
    let has_over_decay = fixtures
        .bad_cases
        .iter()
        .any(|c| c.case_type == fixtures::BadCaseType::OverDecayThreshold);
    let has_graph_entity = fixtures
        .bad_cases
        .iter()
        .any(|c| c.case_type == fixtures::BadCaseType::GraphEntityInfluenced);
    let has_task_affinity = fixtures
        .bad_cases
        .iter()
        .any(|c| c.case_type == fixtures::BadCaseType::TaskAffinityInfluenced);

    if !has_over_decay {
        anyhow::bail!("missing over-decay-threshold bad case");
    }
    if !has_graph_entity {
        anyhow::bail!("missing graph/entity-influenced bad case");
    }
    if !has_task_affinity {
        anyhow::bail!("missing task-affinity-influenced bad case");
    }

    info!(
        over_decay = has_over_decay,
        graph_entity = has_graph_entity,
        task_affinity = has_task_affinity,
        "required coverage types present"
    );

    // 5. Validate baseline
    let baseline_path = crate_root
        .join(fixtures::FixturePaths::BASELINES_DIR)
        .join("phase1.json");
    if !baseline_path.exists() {
        anyhow::bail!(
            "baseline not found at {}. Run `djinn-memory-eval run` then `refresh-baseline`.",
            baseline_path.display()
        );
    }

    let baseline = report::load_baseline(crate_root).context("loading baseline")?;

    // Verify baseline has required fields
    if baseline.metadata.refresh_commit.is_empty() {
        anyhow::bail!("baseline refresh_commit is empty");
    }
    if baseline.threshold_policy_version.is_empty() {
        anyhow::bail!("baseline threshold_policy_version is empty");
    }
    if baseline.suite_metrics.is_empty() {
        anyhow::bail!("baseline suite_metrics is empty");
    }
    if baseline.signal_comparisons.is_empty() {
        anyhow::bail!(
            "baseline signal_comparisons is empty; graph/entity and task-affinity \
             rank-change proof cases must be recorded. Re-run `run` then `refresh-baseline`."
        );
    }

    info!(
        baseline_commit = %baseline.metadata.refresh_commit,
        baseline_created = %baseline.metadata.created_at,
        suites = baseline.suite_metrics.len(),
        policy = %baseline.threshold_policy_version,
        signal_comparisons = baseline.signal_comparisons.len(),
        "baseline validated"
    );

    // 5a2. Log signal comparison summary
    let graph_changed = baseline
        .signal_comparisons
        .iter()
        .filter(|c| c.signal == "graph" && c.rank_changed)
        .count();
    let ta_changed = baseline
        .signal_comparisons
        .iter()
        .filter(|c| c.signal == "task_affinity" && c.rank_changed)
        .count();
    info!(
        graph_rank_changes = graph_changed,
        task_affinity_rank_changes = ta_changed,
        "signal comparison rank-change summary"
    );

    // 5b. Verify baseline aggregate query count matches total fixture count.
    // This prevents silent count mismatches between fixtures and baseline
    // (e.g., bad_cases being excluded from aggregate metrics).
    let expected_total = fixtures.memory_ref_queries.len() + fixtures.bad_cases.len();
    let baseline_total = baseline.aggregate_metrics.query_count;
    if baseline_total != expected_total {
        anyhow::bail!(
            "baseline aggregate query_count {} != total fixture queries {} \
             (memory_ref {} + bad_cases {}). \
             Re-run `run` then `refresh-baseline` to fix.",
            baseline_total,
            expected_total,
            fixtures.memory_ref_queries.len(),
            fixtures.bad_cases.len()
        );
    }
    info!(
        baseline_query_count = baseline_total,
        "baseline aggregate count matches total fixture queries"
    );

    // 5c. Verify baseline per_query_ranks covers both suites.
    let baseline_all_queries = baseline
        .per_query_ranks
        .get("all_queries")
        .map(|v| v.len())
        .unwrap_or(0);
    let baseline_bad_cases = baseline
        .per_query_ranks
        .get("bad_cases")
        .map(|v| v.len())
        .unwrap_or(0);
    if baseline_all_queries != fixtures.memory_ref_queries.len() {
        anyhow::bail!(
            "baseline per_query_ranks.all_queries count {} != memory_ref_queries {}",
            baseline_all_queries,
            fixtures.memory_ref_queries.len()
        );
    }
    if baseline_bad_cases != fixtures.bad_cases.len() {
        anyhow::bail!(
            "baseline per_query_ranks.bad_cases count {} != bad_cases {}",
            baseline_bad_cases,
            fixtures.bad_cases.len()
        );
    }
    info!(
        all_queries = baseline_all_queries,
        bad_cases = baseline_bad_cases,
        "baseline per_query_ranks covers all fixture queries"
    );

    // 6. Verify suite_metrics includes bad_cases suite when fixtures have bad cases
    if !fixtures.bad_cases.is_empty() && !baseline.suite_metrics.contains_key("bad_cases") {
        anyhow::bail!(
            "baseline suite_metrics is missing 'bad_cases' key but fixtures have {} bad-case rows. \
             Re-run `run` then `refresh-baseline` to fix.",
            fixtures.bad_cases.len()
        );
    }
    info!(
        has_bad_cases_suite = baseline.suite_metrics.contains_key("bad_cases"),
        "baseline suite_metrics bad_cases key check passed"
    );

    // 7. Reject all-zero/all-miss Phase 1 gating baseline unless test override is set.
    //
    // An all-miss baseline (recall@1/5/10 all zero and zero-result-rate 1.0) means
    // the retrieval pipeline returned no relevant results for any query — this is
    // either a broken pipeline, bad fixtures, or both. Committed baselines MUST
    // demonstrate meaningful retrieval.
    validate_baseline_not_all_miss(&baseline.aggregate_metrics, &baseline.suite_metrics)?;

    // 8. Verify fixture hashes match manifest (if both exist)
    if let Some(ref manifest) = fixtures.manifest {
        if let Some(ref baseline_hashes) = baseline.metadata.fixture_hashes {
            if manifest.file_hashes != *baseline_hashes {
                anyhow::bail!(
                    "fixture hashes in baseline do not match manifest. \
                     Re-run `refresh-baseline` after updating fixtures."
                );
            }
            info!("fixture hashes match baseline");
        } else {
            tracing::warn!("baseline has no fixture hashes; run `refresh-baseline` to add them");
        }
    }

    info!("=== All validations passed ===");
    Ok(())
}

/// Reject an all-zero/all-miss Phase 1 gating baseline.
///
/// A baseline where recall@1/5/10 are all zero and zero-result-rate is 1.0
/// indicates the retrieval pipeline returned no relevant results for any query.
/// This is never a valid committed baseline — it represents a broken pipeline,
/// bad fixtures, or both.
///
/// Test override: set `DJINN_MEMORY_EVAL_TEST_OVERRIDE=allow_all_miss_baseline`
/// to bypass this check (only outside committed CI paths).
pub fn validate_baseline_not_all_miss(
    aggregate: &metrics::AggregateMetrics,
    suite_metrics: &HashMap<String, metrics::SuiteMetrics>,
) -> Result<()> {
    // Check if the test override is set
    if std::env::var("DJINN_MEMORY_EVAL_TEST_OVERRIDE").as_deref() == Ok("allow_all_miss_baseline")
    {
        tracing::warn!(
            "DJINN_MEMORY_EVAL_TEST_OVERRIDE=allow_all_miss_baseline is set — \
             skipping all-miss baseline check"
        );
        return Ok(());
    }

    let all_recall_zero = aggregate.recall_at_1.abs() < 1e-10
        && aggregate.recall_at_5.abs() < 1e-10
        && aggregate.recall_at_10.abs() < 1e-10;
    let all_miss = all_recall_zero && (aggregate.zero_result_rate - 1.0).abs() < 1e-10;

    if all_miss {
        anyhow::bail!(
            "committed baseline is all-miss: aggregate recall@1/5/10 all zero \
             and zero-result-rate is 1.0. This indicates the retrieval pipeline \
             returned no relevant results for any query. \
             Re-run against a working pipeline with meaningful fixtures. \
             (Test override: DJINN_MEMORY_EVAL_TEST_OVERRIDE=allow_all_miss_baseline)"
        );
    }

    // Also reject if any non-empty suite has all-zero recall and 1.0 zero-result-rate
    for (suite_name, sm) in suite_metrics {
        if sm.query_count == 0 {
            continue;
        }
        let suite_all_recall_zero = sm.recall_at_1.abs() < 1e-10
            && sm.recall_at_5.abs() < 1e-10
            && sm.recall_at_10.abs() < 1e-10;
        let suite_all_miss = suite_all_recall_zero && (sm.zero_result_rate - 1.0).abs() < 1e-10;
        if suite_all_miss {
            anyhow::bail!(
                "committed baseline suite '{}' is all-miss ({} queries, \
                 recall@1/5/10 all zero, zero-result-rate 1.0). \
                 This indicates no relevant results were returned for this suite. \
                 Re-run against a working pipeline with meaningful fixtures. \
                 (Test override: DJINN_MEMORY_EVAL_TEST_OVERRIDE=allow_all_miss_baseline)",
                suite_name,
                sm.query_count,
            );
        }
    }

    Ok(())
}

fn not_yet_implemented(subcommand: &str, task_ref: &str) -> anyhow::Result<()> {
    anyhow::bail!(
        "Subcommand `{subcommand}` is not yet implemented. \
         Tracked by task {task_ref} in epic nih4 (Phase 1). \
         See the djinn-memory-eval README for the implementation roadmap."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::*;
    use crate::report::{BaselineMetadata, Phase1Baseline};

    /// Helper: write a Phase1Fixtures to a temp directory structure on disk.
    fn write_fixtures_to_disk(crate_root: &std::path::Path, fixtures: &Phase1Fixtures) {
        let fixtures_dir = crate_root.join(FixturePaths::FIXTURES_DIR);
        std::fs::create_dir_all(&fixtures_dir).unwrap();

        let corpus_jsonl = fixtures::to_jsonl(&fixtures.corpus_notes).unwrap();
        std::fs::write(fixtures_dir.join("corpus-notes.jsonl"), &corpus_jsonl).unwrap();

        let queries_jsonl = fixtures::to_jsonl(&fixtures.memory_ref_queries).unwrap();
        std::fs::write(
            fixtures_dir.join("memory-ref-queries.jsonl"),
            &queries_jsonl,
        )
        .unwrap();

        let bad_cases_jsonl = fixtures::to_jsonl(&fixtures.bad_cases).unwrap();
        std::fs::write(fixtures_dir.join("bad-cases.jsonl"), &bad_cases_jsonl).unwrap();
    }

    /// Helper: write a Phase1Baseline to a temp directory on disk.
    fn write_baseline_to_disk(crate_root: &std::path::Path, baseline: &Phase1Baseline) {
        let baselines_dir = crate_root.join(FixturePaths::BASELINES_DIR);
        std::fs::create_dir_all(&baselines_dir).unwrap();
        let json = serde_json::to_string_pretty(baseline).unwrap();
        std::fs::write(baselines_dir.join("phase1.json"), &json).unwrap();
    }

    /// Minimal corpus notes for testing (enough for fixture validation).
    fn minimal_corpus_notes() -> Vec<CorpusNoteRow> {
        vec![
            serde_json::from_str(
                r#"{"permalink":"notes/a","title":"Note A","content":"Note A content about searching","note_type":"case","folder":"notes","status":"active","tags":[],"timestamps":{"created_at":"2026-01-01T00:00:00.000Z","updated_at":"2026-01-01T00:00:00.000Z","last_accessed":"2026-06-01T00:00:00.000Z"},"confidence":0.8,"embedding":{"content_hash":"h1","model_version":"deterministic-v1","embedding_dim":3,"vector":[0.1,0.2,0.3]},"labels":[{"entity_type":"concept","name":"search"}],"graph_edges":[],"expected_signals":{"vector":true,"lexical":true,"temporal":false,"graph":false,"entity":false,"task_affinity":false}}"#,
            ).unwrap(),
            serde_json::from_str(
                r#"{"permalink":"notes/b","title":"Note B","content":"Note B content about ranking","note_type":"pattern","folder":"notes","status":"active","tags":[],"timestamps":{"created_at":"2026-01-01T00:00:00.000Z","updated_at":"2026-01-01T00:00:00.000Z","last_accessed":"2026-06-01T00:00:00.000Z"},"confidence":0.8,"embedding":{"content_hash":"h2","model_version":"deterministic-v1","embedding_dim":3,"vector":[0.4,0.5,0.6]},"labels":[{"entity_type":"concept","name":"ranking"}],"graph_edges":[],"expected_signals":{"vector":true,"lexical":true,"temporal":false,"graph":false,"entity":false,"task_affinity":false}}"#,
            ).unwrap(),
        ]
    }

    /// Generate N memory-ref query rows.
    fn make_n_queries(n: usize) -> Vec<MinedMemoryRefRow> {
        (0..n)
            .map(|i| {
                serde_json::from_str(&format!(
                    r#"{{"query_id":"q-{:03}","query_text":"query {}","task_id":null,"memory_refs":["notes/a"],"expected_signals":{{"vector":true,"lexical":true,"temporal":false,"graph":false,"entity":false,"task_affinity":false}}}}"#,
                    i, i
                ))
                .unwrap()
            })
            .collect()
    }

    /// Generate N bad-case rows (alternating types to satisfy coverage requirements).
    fn make_n_bad_cases(n: usize) -> Vec<BadCaseRow> {
        let case_types = [
            BadCaseType::OverDecayThreshold,
            BadCaseType::GraphEntityInfluenced,
            BadCaseType::TaskAffinityInfluenced,
        ];
        let permalinks = ["notes/a", "notes/b", "notes/a"];
        (0..n)
            .map(|i| {
                let idx = i % case_types.len();
                BadCaseRow {
                    case_id: format!("bc-{:03}", i),
                    query_text: format!("bad case {}", i),
                    case_type: case_types[idx].clone(),
                    expected_behavior: "test".to_string(),
                    task_id: if case_types[idx] == BadCaseType::TaskAffinityInfluenced {
                        Some("test-task".to_string())
                    } else {
                        None
                    },
                    relevant_note_permalinks: vec![permalinks[idx].to_string()],
                    expected_signals: SignalCoverage {
                        vector: false,
                        lexical: true,
                        temporal: false,
                        graph: false,
                        entity: false,
                        task_affinity: false,
                    },
                    tags: vec![],
                }
            })
            .collect()
    }

    /// Make a baseline with explicit suite counts (matching fixture counts).
    fn make_baseline_with_counts(all_queries: usize, bad_cases: usize) -> Phase1Baseline {
        let total = all_queries + bad_cases;

        let mut suite_metrics = HashMap::new();
        suite_metrics.insert(
            "all_queries".to_string(),
            metrics::SuiteMetrics {
                recall_at_1: 0.5,
                recall_at_5: 0.7,
                recall_at_10: 0.9,
                mrr: 0.6,
                zero_result_rate: 0.1,
                query_count: all_queries,
            },
        );
        suite_metrics.insert(
            "bad_cases".to_string(),
            metrics::SuiteMetrics {
                recall_at_1: 0.3,
                recall_at_5: 0.5,
                recall_at_10: 0.7,
                mrr: 0.4,
                zero_result_rate: 0.2,
                query_count: bad_cases,
            },
        );

        let mut per_query_ranks = HashMap::new();
        per_query_ranks.insert(
            "all_queries".to_string(),
            (0..all_queries)
                .map(|i| report::QueryRankBaseline {
                    query_id: format!("q-{:03}", i),
                    query_text: format!("query {}", i),
                    result_permalinks: vec!["notes/a".to_string()],
                    relevant_ranks: vec![Some(1)],
                    best_rank: Some(1),
                })
                .collect(),
        );
        per_query_ranks.insert(
            "bad_cases".to_string(),
            (0..bad_cases)
                .map(|i| report::QueryRankBaseline {
                    query_id: format!("bc-{:03}", i),
                    query_text: format!("bad case {}", i),
                    result_permalinks: vec!["notes/a".to_string()],
                    relevant_ranks: vec![Some(1)],
                    best_rank: Some(1),
                })
                .collect(),
        );

        Phase1Baseline {
            metadata: BaselineMetadata {
                fixture_hashes: None,
                refresh_commit: "test-commit-sha".to_string(),
                created_at: "2026-07-10T00:00:00Z".to_string(),
            },
            suite_metrics,
            aggregate_metrics: metrics::AggregateMetrics {
                recall_at_1: 0.5,
                recall_at_5: 0.7,
                recall_at_10: 0.9,
                mrr: 0.6,
                zero_result_rate: 0.1,
                query_count: total,
            },
            age_bucket_recall: HashMap::new(),
            per_query_ranks,
            threshold_policy_version: metrics::THRESHOLD_POLICY_VERSION.to_string(),
        }
    }

    // ── AC: Low query count ──────────────────────────────────────────────────

    #[test]
    fn validate_fixtures_rejects_low_total_query_count() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_root = tmp.path();

        // Only 10 queries + 3 bad cases = 13 total (< 25)
        let corpus = minimal_corpus_notes();
        let queries = make_n_queries(10);
        let bad_cases = make_n_bad_cases(3);

        let fixtures = Phase1Fixtures {
            corpus_notes: corpus,
            memory_ref_queries: queries,
            bad_cases,
            manifest: None,
        };
        write_fixtures_to_disk(crate_root, &fixtures);

        let baseline = make_baseline_with_counts(10, 3);
        write_baseline_to_disk(crate_root, &baseline);

        let result = cmd_validate_fixtures(crate_root);
        assert!(result.is_err(), "should fail for < 25 total queries");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("expected at least 25 total labeled queries"),
            "error should mention 25 total queries: {}",
            err
        );
    }

    #[test]
    fn validate_fixtures_rejects_low_memory_ref_count() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_root = tmp.path();

        // 12 queries (< 15) + 13 bad cases = 25 total (≥ 25) but memory_ref < 15
        let corpus = minimal_corpus_notes();
        let queries = make_n_queries(12);
        let bad_cases = make_n_bad_cases(13);

        let fixtures = Phase1Fixtures {
            corpus_notes: corpus,
            memory_ref_queries: queries,
            bad_cases,
            manifest: None,
        };
        write_fixtures_to_disk(crate_root, &fixtures);

        let baseline = make_baseline_with_counts(12, 13);
        write_baseline_to_disk(crate_root, &baseline);

        let result = cmd_validate_fixtures(crate_root);
        assert!(result.is_err(), "should fail for < 15 memory_ref queries");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("expected at least 15 mined memory_refs queries"),
            "error should mention 15 memory_refs: {}",
            err
        );
    }

    #[test]
    fn validate_fixtures_rejects_low_bad_case_count() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_root = tmp.path();

        // 20 queries + 5 bad cases (< 10) = 25 total (≥ 25) but bad_cases < 10
        let corpus = minimal_corpus_notes();
        let queries = make_n_queries(20);
        let bad_cases = make_n_bad_cases(5);

        let fixtures = Phase1Fixtures {
            corpus_notes: corpus,
            memory_ref_queries: queries,
            bad_cases,
            manifest: None,
        };
        write_fixtures_to_disk(crate_root, &fixtures);

        let baseline = make_baseline_with_counts(20, 5);
        write_baseline_to_disk(crate_root, &baseline);

        let result = cmd_validate_fixtures(crate_root);
        assert!(result.is_err(), "should fail for < 10 bad cases");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("expected at least 10 bad-case rows"),
            "error should mention 10 bad-case rows: {}",
            err
        );
    }

    // ── AC: Dropped bad-case aggregation ─────────────────────────────────────

    #[test]
    fn validate_fixtures_rejects_missing_bad_cases_in_suite_metrics() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_root = tmp.path();

        let corpus = minimal_corpus_notes();
        let queries = make_n_queries(20);
        let bad_cases = make_n_bad_cases(10);

        let fixtures = Phase1Fixtures {
            corpus_notes: corpus,
            memory_ref_queries: queries,
            bad_cases,
            manifest: None,
        };
        write_fixtures_to_disk(crate_root, &fixtures);

        // Build baseline with NO bad_cases key in suite_metrics
        let mut baseline = make_baseline_with_counts(20, 10);
        baseline.suite_metrics.remove("bad_cases");
        write_baseline_to_disk(crate_root, &baseline);

        let result = cmd_validate_fixtures(crate_root);
        assert!(
            result.is_err(),
            "should fail when bad_cases absent from suite_metrics"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("missing 'bad_cases' key"),
            "error should mention missing bad_cases key: {}",
            err
        );
    }

    // ── AC: All-miss baseline ────────────────────────────────────────────────

    #[test]
    fn validate_fixtures_rejects_all_miss_aggregate_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_root = tmp.path();

        let corpus = minimal_corpus_notes();
        let queries = make_n_queries(20);
        let bad_cases = make_n_bad_cases(10);

        let fixtures = Phase1Fixtures {
            corpus_notes: corpus,
            memory_ref_queries: queries,
            bad_cases,
            manifest: None,
        };
        write_fixtures_to_disk(crate_root, &fixtures);

        // Build baseline with all-miss aggregate metrics
        let mut baseline = make_baseline_with_counts(20, 10);
        baseline.aggregate_metrics = metrics::AggregateMetrics {
            recall_at_1: 0.0,
            recall_at_5: 0.0,
            recall_at_10: 0.0,
            mrr: 0.0,
            zero_result_rate: 1.0,
            query_count: 30,
        };
        write_baseline_to_disk(crate_root, &baseline);

        let result = cmd_validate_fixtures(crate_root);
        assert!(
            result.is_err(),
            "should fail for all-miss aggregate baseline"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("all-miss"),
            "error should mention all-miss: {}",
            err
        );
    }

    #[test]
    fn validate_fixtures_rejects_all_miss_suite_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_root = tmp.path();

        let corpus = minimal_corpus_notes();
        let queries = make_n_queries(20);
        let bad_cases = make_n_bad_cases(10);

        let fixtures = Phase1Fixtures {
            corpus_notes: corpus,
            memory_ref_queries: queries,
            bad_cases,
            manifest: None,
        };
        write_fixtures_to_disk(crate_root, &fixtures);

        // Build baseline with all-miss bad_cases suite
        let mut baseline = make_baseline_with_counts(20, 10);
        baseline.suite_metrics.insert(
            "bad_cases".to_string(),
            metrics::SuiteMetrics {
                recall_at_1: 0.0,
                recall_at_5: 0.0,
                recall_at_10: 0.0,
                mrr: 0.0,
                zero_result_rate: 1.0,
                query_count: 10,
            },
        );
        write_baseline_to_disk(crate_root, &baseline);

        let result = cmd_validate_fixtures(crate_root);
        assert!(result.is_err(), "should fail for all-miss suite baseline");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("all-miss"),
            "error should mention all-miss: {}",
            err
        );
    }

    #[test]
    fn validate_baseline_not_all_miss_allows_test_override() {
        let aggregate = metrics::AggregateMetrics {
            recall_at_1: 0.0,
            recall_at_5: 0.0,
            recall_at_10: 0.0,
            mrr: 0.0,
            zero_result_rate: 1.0,
            query_count: 30,
        };
        let suite_metrics = HashMap::new();

        // Without override, should fail
        let result = validate_baseline_not_all_miss(&aggregate, &suite_metrics);
        assert!(result.is_err(), "should fail without test override");

        // With override, should pass
        // SAFETY: this test runs single-threaded and only touches the specific env var.
        unsafe {
            std::env::set_var("DJINN_MEMORY_EVAL_TEST_OVERRIDE", "allow_all_miss_baseline");
        }
        let result = validate_baseline_not_all_miss(&aggregate, &suite_metrics);
        assert!(result.is_ok(), "should pass with test override");
        // SAFETY: see above.
        unsafe {
            std::env::remove_var("DJINN_MEMORY_EVAL_TEST_OVERRIDE");
        }
    }

    // ── AC: Missing hard signal coverage ─────────────────────────────────────

    /// Fixture validation fails when graph signal is claimed on a note
    /// but the note has no graph edges (hard error, not warn-only).
    #[test]
    fn validate_fixtures_hard_fails_on_missing_graph_signal_data() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_root = tmp.path();

        let mut corpus = minimal_corpus_notes();
        // Claim graph signal on note A but provide no graph edges
        corpus[0].expected_signals.graph = true;
        corpus[0].graph_edges = vec![];

        let fixtures = Phase1Fixtures {
            corpus_notes: corpus,
            memory_ref_queries: make_n_queries(1),
            bad_cases: make_n_bad_cases(1),
            manifest: None,
        };
        write_fixtures_to_disk(crate_root, &fixtures);

        let baseline = make_baseline_with_counts(1, 1);
        write_baseline_to_disk(crate_root, &baseline);

        let result = cmd_validate_fixtures(crate_root);
        assert!(
            result.is_err(),
            "should hard-fail when graph signal claimed but no edges"
        );
        let err = format!("{:?}", result.unwrap_err());
        assert!(
            err.contains("graph signal claimed but no graph_edges"),
            "error should mention missing graph_edges: {}",
            err
        );
    }

    /// Fixture validation fails when entity signal is claimed but no labels.
    #[test]
    fn validate_fixtures_hard_fails_on_missing_entity_signal_data() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_root = tmp.path();

        let mut corpus = minimal_corpus_notes();
        corpus[1].expected_signals.entity = true;
        corpus[1].labels = vec![];

        let fixtures = Phase1Fixtures {
            corpus_notes: corpus,
            memory_ref_queries: make_n_queries(1),
            bad_cases: make_n_bad_cases(1),
            manifest: None,
        };
        write_fixtures_to_disk(crate_root, &fixtures);

        let baseline = make_baseline_with_counts(1, 1);
        write_baseline_to_disk(crate_root, &baseline);

        let result = cmd_validate_fixtures(crate_root);
        assert!(
            result.is_err(),
            "should hard-fail when entity signal claimed but no labels"
        );
        let err = format!("{:?}", result.unwrap_err());
        assert!(
            err.contains("entity signal claimed but no labels"),
            "error should mention missing labels: {}",
            err
        );
    }

    /// Fixture validation fails when task-affinity signal is claimed on a
    /// query but no task_id is provided.
    #[test]
    fn validate_fixtures_hard_fails_on_missing_task_affinity_signal_data() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_root = tmp.path();

        let corpus = minimal_corpus_notes();
        let mut queries = make_n_queries(1);
        queries[0].expected_signals.task_affinity = true;
        queries[0].task_id = None;

        let fixtures = Phase1Fixtures {
            corpus_notes: corpus,
            memory_ref_queries: queries,
            bad_cases: make_n_bad_cases(1),
            manifest: None,
        };
        write_fixtures_to_disk(crate_root, &fixtures);

        let baseline = make_baseline_with_counts(1, 1);
        write_baseline_to_disk(crate_root, &baseline);

        let result = cmd_validate_fixtures(crate_root);
        assert!(
            result.is_err(),
            "should hard-fail when task_affinity signal claimed but no task_id"
        );
        let err = format!("{:?}", result.unwrap_err());
        assert!(
            err.contains("task_affinity signal claimed but no task_id"),
            "error should mention missing task_id: {}",
            err
        );
    }

    /// Fixture validation passes with minimum valid fixtures and baseline.
    #[test]
    fn validate_fixtures_passes_with_minimum_valid_set() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_root = tmp.path();

        let corpus = minimal_corpus_notes();
        let queries = make_n_queries(20);
        let bad_cases = make_n_bad_cases(10);

        let fixtures = Phase1Fixtures {
            corpus_notes: corpus,
            memory_ref_queries: queries,
            bad_cases,
            manifest: None,
        };
        write_fixtures_to_disk(crate_root, &fixtures);

        let baseline = make_baseline_with_counts(20, 10);
        write_baseline_to_disk(crate_root, &baseline);

        let result = cmd_validate_fixtures(crate_root);
        assert!(
            result.is_ok(),
            "should pass with valid fixtures and baseline: {:?}",
            result.err()
        );
    }
}
