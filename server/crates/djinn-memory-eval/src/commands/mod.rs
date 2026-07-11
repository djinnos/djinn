//! CLI command implementations for djinn-memory-eval.
//!
//! Each `cmd_*` function is dispatched from `main()` and contains the
//! full implementation of one CLI subcommand.
//!
//! ## Phase 1 invariants
//!
//! `cmd_validate_fixtures` and `assert_signal_effects` (in `run.rs`) both
//! require at least one graph/entity and at least one task-affinity signal
//! comparison with `rank_changed=true`.  Absence of either required family
//! is a hard failure, not a warning.

#[cfg(test)]
mod test_helpers;

use std::collections::HashMap;

use anyhow::{Context, Result};
use djinn_core::clock::{Clock, SystemClock};
use tracing::info;

use crate::fixtures::{self, FixturePaths};
use crate::loader;
use crate::metrics;
use crate::report;
use crate::run;

/// Run the Phase 1 benchmark, compute metrics, write reports.
pub async fn cmd_run(crate_root: &std::path::Path) -> Result<()> {
    let output = run::execute_run(crate_root).await?;

    info!("=== Phase 1 Benchmark Run ===");
    info!(notes = output.corpus_note_count, "corpus");
    info!(queries = output.query_count, "queries executed");
    info!(bad_cases = output.bad_case_count, "bad cases executed");
    info!(
        comparisons = output.signal_comparisons.len(),
        "signal comparisons"
    );

    let fixtures =
        loader::load_fixtures_from_disk(crate_root).context("loading fixtures for age data")?;

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

    let all_records: Vec<&run::QueryRankRecord> = output
        .query_records
        .iter()
        .chain(output.bad_case_records.iter())
        .collect();

    let query_result_records: Vec<metrics::QueryResultRecord> = all_records
        .iter()
        .map(|r| build_query_result_record(r, &note_ages))
        .collect();

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

    // Age-bucket recall — include both memory-ref queries AND bad-case
    // records so that over-decay fixture cases (e.g. bc-over-decay-001)
    // contribute to the age-bucket recall curves.
    let all_records_for_age: Vec<run::QueryRankRecord> = output
        .query_records
        .iter()
        .chain(output.bad_case_records.iter())
        .cloned()
        .collect();
    let age_bucket_recall = metrics::compute_age_bucket_recall(&all_records_for_age, &note_ages);
    let directional = metrics::directional_metrics(&output.query_records);

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

    let fixture_hashes = fixtures.manifest.as_ref().map(|m| m.file_hashes.clone());

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
/// Exits non-zero (via Err) if any gating threshold fails.
pub async fn cmd_compare(crate_root: &std::path::Path) -> Result<()> {
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

    let baseline = report::load_baseline(crate_root).context("loading committed baseline")?;

    info!("=== Phase 1 Compare ===");
    info!(
        baseline_commit = %baseline.metadata.refresh_commit,
        baseline_created = %baseline.metadata.created_at,
        "loaded baseline"
    );

    let current_aggregate = &current_report.aggregate_metrics;

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
            metric = %failure.metric, suite = %failure.suite,
            baseline = failure.baseline_value, current = failure.current_value,
            delta = failure.delta, "gating failure"
        );
    }

    current_report.compare_result = Some(compare_result.clone());
    report::write_reports(&current_report, &target_dir)?;
    info!(path = %target_dir.display(), "updated reports written");

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
pub async fn cmd_refresh_baseline(crate_root: &std::path::Path) -> Result<()> {
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

    let repo_root = crate_root
        .ancestors()
        .nth(3)
        .unwrap_or(crate_root)
        .to_path_buf();
    let refresh_commit = djinn_git::head_commit_sha(&repo_root).await.map_err(|e| {
        anyhow::anyhow!(
            "failed to resolve refresh commit from repository HEAD: {}",
            e
        )
    })?;

    info!("=== Refresh Baseline ===");
    info!(commit = %refresh_commit, "refresh commit");

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

    report::write_baseline(crate_root, &baseline)?;
    info!(path = %crate_root.join("baselines/phase1.json").display(), "baseline refreshed");
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
fn parse_iso8601_epoch(s: &str) -> Result<u64> {
    let s = s.trim_end_matches('Z');
    let s = if let Some(dot_pos) = s.find('.') {
        &s[..dot_pos]
    } else {
        s
    };
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
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 12 } else { month };
    let d = day as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (153 * (m - 3) + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe as i64 - 719468;
    Ok(days as u64 * 86400 + hour * 3600 + minute * 60 + second)
}

/// Returns a clear not-yet-implemented error.
pub fn not_yet_implemented(subcommand: &str, task_ref: &str) -> anyhow::Result<()> {
    anyhow::bail!(
        "Subcommand `{subcommand}` is not yet implemented. \
         Tracked by task {task_ref} in epic nih4 (Phase 1). \
         See the djinn-memory-eval README for the implementation roadmap."
    )
}

/// Validate that a `refresh_commit` value looks like a real commit SHA
/// and is not a known placeholder.
///
/// Called by `cmd_validate_fixtures` to reject test scaffolding metadata
/// in committed baselines.  Valid commit SHAs are lowercase hex and at
/// least 7 characters (abbreviated SHA) -- common git conventions.
pub fn validate_refresh_commit(refresh_commit: &str) -> anyhow::Result<()> {
    if refresh_commit.is_empty() {
        anyhow::bail!("baseline metadata.refresh_commit is empty");
    }

    const PLACEHOLDER_PATTERNS: &[&str] = &["local-test-refresh", "unknown", "placeholder", "none"];

    let lower = refresh_commit.to_lowercase();
    for &pattern in PLACEHOLDER_PATTERNS {
        if lower == pattern {
            anyhow::bail!(
                "baseline metadata.refresh_commit is a known placeholder \
                 (\"{}\").  Refresh the baseline with `cargo run -p \
                 djinn-memory-eval -- refresh-baseline` so it carries \
                 real repository HEAD commit provenance.",
                refresh_commit,
            );
        }
    }

    // A real commit SHA is hex and at least 7 characters (abbreviated SHA).
    if lower.len() < 7 || !lower.chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!(
            "baseline metadata.refresh_commit \"{}\" does not look like a \
             valid git commit SHA (expected 7+ hex characters). \
             Refresh the baseline with `cargo run -p djinn-memory-eval -- \
             refresh-baseline`.",
            refresh_commit,
        );
    }

    Ok(())
}

/// Validate committed fixtures and baseline without running the pipeline.
/// No LLM calls or external network are required.
///
/// Phase 1 requires at least one graph/entity and at least one task-affinity
/// rank-change proof case in the baseline's signal_comparisons.
pub fn cmd_validate_fixtures(crate_root: &std::path::Path) -> Result<()> {
    info!("=== Validate Fixtures ===");

    let fixtures =
        loader::load_fixtures_from_disk(crate_root).context("loading fixtures from disk")?;

    info!(
        corpus = fixtures.corpus_notes.len(),
        queries = fixtures.memory_ref_queries.len(),
        bad_cases = fixtures.bad_cases.len(),
        "fixtures loaded"
    );

    loader::validate_fixtures(&fixtures).context("fixture validation failed")?;
    info!("fixture validation passed");

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

    if total_queries < 25 {
        anyhow::bail!(
            "expected at least 25 total labeled queries, got {}",
            total_queries
        );
    }
    if fixtures.memory_ref_queries.len() < 15 {
        anyhow::bail!(
            "expected at least 15 mined memory_refs queries, got {}",
            fixtures.memory_ref_queries.len()
        );
    }
    if fixtures.bad_cases.len() < 10 {
        anyhow::bail!(
            "expected at least 10 bad-case rows, got {}",
            fixtures.bad_cases.len()
        );
    }

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

    let baseline_path = crate_root
        .join(FixturePaths::BASELINES_DIR)
        .join("phase1.json");
    if !baseline_path.exists() {
        anyhow::bail!(
            "baseline not found at {}. Run `djinn-memory-eval run` then `refresh-baseline`.",
            baseline_path.display()
        );
    }

    let baseline = report::load_baseline(crate_root).context("loading baseline")?;

    validate_refresh_commit(&baseline.metadata.refresh_commit)
        .context("baseline refresh_commit validation failed")?;
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
        suites = baseline.suite_metrics.len(), policy = %baseline.threshold_policy_version,
        signal_comparisons = baseline.signal_comparisons.len(), "baseline validated"
    );

    // Phase 1 requires at least one graph/entity and at least one task-affinity
    // comparison with rank_changed=true in the baseline.
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

    if graph_changed == 0 {
        anyhow::bail!(
            "baseline signal_comparisons has no graph/entity comparisons \
             with rank_changed=true ({} total graph comparisons found). \
             Phase 1 requires at least one graph/entity rank-change proof case. \
             Re-run `run` then `refresh-baseline`.",
            baseline
                .signal_comparisons
                .iter()
                .filter(|c| c.signal == "graph")
                .count()
        );
    }
    if ta_changed == 0 {
        anyhow::bail!(
            "baseline signal_comparisons has no task-affinity comparisons \
             with rank_changed=true ({} total task-affinity comparisons found). \
             Phase 1 requires at least one task-affinity rank-change proof case. \
             Re-run `run` then `refresh-baseline`.",
            baseline
                .signal_comparisons
                .iter()
                .filter(|c| c.signal == "task_affinity")
                .count()
        );
    }
    info!(
        graph_rank_changes = graph_changed,
        task_affinity_rank_changes = ta_changed,
        "signal comparison rank-change summary"
    );

    // Verify over-decay age-bucket recall is present in baseline.
    // If the committed fixtures include an over-decay-threshold bad case, the
    // committed baseline MUST include the `over_decay_threshold` age-bucket
    // recall entry. This catches stale baselines that predate the over-decay
    // fixture addition or were refreshed without including bad-case records
    // in the age-bucket computation.
    if has_over_decay
        && !baseline
            .age_bucket_recall
            .contains_key(&metrics::AgeBucket::OverDecayThreshold)
    {
        anyhow::bail!(
            "fixtures include an over-decay-threshold bad case but baseline \
             age_bucket_recall is missing the 'over_decay_threshold' bucket. \
             Re-run `run` then `refresh-baseline` to include over-decay recall data."
        );
    }

    let expected_total = fixtures.memory_ref_queries.len() + fixtures.bad_cases.len();
    let baseline_total = baseline.aggregate_metrics.query_count;
    if baseline_total != expected_total {
        anyhow::bail!(
            "baseline aggregate query_count {} != total fixture queries {} \
             (memory_ref {} + bad_cases {}). Re-run `run` then `refresh-baseline` to fix.",
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

    validate_baseline_not_all_miss(&baseline.aggregate_metrics, &baseline.suite_metrics)?;

    if let Some(ref manifest) = fixtures.manifest {
        if let Some(ref baseline_hashes) = baseline.metadata.fixture_hashes {
            if manifest.file_hashes != *baseline_hashes {
                anyhow::bail!(
                    "fixture hashes in baseline do not match manifest. Re-run `refresh-baseline` after updating fixtures."
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
/// Test override: set `DJINN_MEMORY_EVAL_TEST_OVERRIDE=allow_all_miss_baseline`.
pub fn validate_baseline_not_all_miss(
    aggregate: &metrics::AggregateMetrics,
    suite_metrics: &HashMap<String, metrics::SuiteMetrics>,
) -> Result<()> {
    if std::env::var("DJINN_MEMORY_EVAL_TEST_OVERRIDE").as_deref() == Ok("allow_all_miss_baseline")
    {
        tracing::warn!(
            "DJINN_MEMORY_EVAL_TEST_OVERRIDE=allow_all_miss_baseline is set — skipping all-miss baseline check"
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
                 Re-run against a working pipeline with meaningful fixtures. \
                 (Test override: DJINN_MEMORY_EVAL_TEST_OVERRIDE=allow_all_miss_baseline)",
                suite_name,
                sm.query_count,
            );
        }
    }
    Ok(())
}

/// Run the Phase 2 QA execution: extract QA pairs from pitfall/case notes,
/// run each question through real `NoteRepository::search` with top-k 10,
/// render results through `format_knowledge_notes(2000)`, and record
/// retrieval hit, gold rank, context recall, note type, and age bucket.
///
/// This path is deterministic/no-LLM: it prepares judge inputs but does not
/// call a provider. Phase 2 output is written adjacent to Phase 1 reports
/// and does not modify Phase 1 baselines or compare semantics.
pub async fn cmd_qa_run(crate_root: &std::path::Path) -> Result<()> {
    let output = crate::qa_run::execute_qa_run(crate_root).await?;

    info!("=== Phase 2 QA Run ===");
    info!(corpus = output.corpus_note_count, "corpus notes");
    info!(pairs = output.qa_count, "QA pairs processed");
    info!(
        extraction_pairs = output.extraction.pairs.len(),
        skipped = output.extraction.skipped.len(),
        eligible = output.extraction.eligible_count,
        "QA extraction"
    );
    info!(
        retrieval_hits = output.retrieval_hit_count,
        context_recalls = output.context_recall_count,
        "QA results"
    );

    // Write Phase 2 QA report to target/memory-eval alongside Phase 1 reports.
    let target_dir = std::path::PathBuf::from("target/memory-eval");
    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("creating output directory {}", target_dir.display()))?;

    let report_json =
        serde_json::to_string_pretty(&output).context("serializing Phase 2 QA run output")?;
    let report_path = target_dir.join("phase2-qa-report.json");
    std::fs::write(&report_path, report_json)
        .with_context(|| format!("writing {}", report_path.display()))?;
    info!(path = %report_path.display(), "Phase 2 QA report written");

    // Print per-QA summary to info log
    for record in &output.records {
        info!(
            qa_id = %record.qa_id,
            note_type = %record.note_type,
            retrieval_hit = record.retrieval_hit,
            gold_rank = ?record.gold_rank,
            context_recall = record.context_recall,
            age_bucket = %record.age_bucket,
            age_days = record.age_days,
            "QA result"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::*;
    use test_helpers::*;

    // ── AC: Low query count ──────────────────────────────────────────────

    #[test]
    fn validate_fixtures_rejects_low_total_query_count() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fixtures = Phase1Fixtures {
            corpus_notes: minimal_corpus_notes(),
            memory_ref_queries: make_n_queries(10),
            bad_cases: make_n_bad_cases(3),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
        write_baseline_to_disk(root, &make_baseline_with_counts(10, 3));
        let err = cmd_validate_fixtures(root).unwrap_err().to_string();
        assert!(
            err.contains("expected at least 25 total labeled queries"),
            "{}",
            err
        );
    }

    #[test]
    fn validate_fixtures_rejects_low_memory_ref_count() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fixtures = Phase1Fixtures {
            corpus_notes: minimal_corpus_notes(),
            memory_ref_queries: make_n_queries(12),
            bad_cases: make_n_bad_cases(13),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
        write_baseline_to_disk(root, &make_baseline_with_counts(12, 13));
        let err = cmd_validate_fixtures(root).unwrap_err().to_string();
        assert!(
            err.contains("expected at least 15 mined memory_refs queries"),
            "{}",
            err
        );
    }

    #[test]
    fn validate_fixtures_rejects_low_bad_case_count() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fixtures = Phase1Fixtures {
            corpus_notes: minimal_corpus_notes(),
            memory_ref_queries: make_n_queries(20),
            bad_cases: make_n_bad_cases(5),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
        write_baseline_to_disk(root, &make_baseline_with_counts(20, 5));
        let err = cmd_validate_fixtures(root).unwrap_err().to_string();
        assert!(
            err.contains("expected at least 10 bad-case rows"),
            "{}",
            err
        );
    }

    // ── AC: Dropped bad-case aggregation ─────────────────────────────────

    #[test]
    fn validate_fixtures_rejects_missing_bad_cases_in_suite_metrics() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fixtures = Phase1Fixtures {
            corpus_notes: minimal_corpus_notes(),
            memory_ref_queries: make_n_queries(20),
            bad_cases: make_n_bad_cases(10),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
        let mut baseline = make_baseline_with_counts(20, 10);
        baseline.suite_metrics.remove("bad_cases");
        write_baseline_to_disk(root, &baseline);
        let err = cmd_validate_fixtures(root).unwrap_err().to_string();
        assert!(err.contains("missing 'bad_cases' key"), "{}", err);
    }

    // ── AC: All-miss baseline ────────────────────────────────────────────

    #[test]
    fn validate_fixtures_rejects_all_miss_aggregate_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fixtures = Phase1Fixtures {
            corpus_notes: minimal_corpus_notes(),
            memory_ref_queries: make_n_queries(20),
            bad_cases: make_n_bad_cases(10),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
        let mut baseline = make_baseline_with_counts(20, 10);
        baseline.aggregate_metrics = metrics::AggregateMetrics {
            recall_at_1: 0.0,
            recall_at_5: 0.0,
            recall_at_10: 0.0,
            mrr: 0.0,
            zero_result_rate: 1.0,
            query_count: 30,
        };
        write_baseline_to_disk(root, &baseline);
        let err = cmd_validate_fixtures(root).unwrap_err().to_string();
        assert!(err.contains("all-miss"), "{}", err);
    }

    #[test]
    fn validate_fixtures_rejects_all_miss_suite_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fixtures = Phase1Fixtures {
            corpus_notes: minimal_corpus_notes(),
            memory_ref_queries: make_n_queries(20),
            bad_cases: make_n_bad_cases(10),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
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
        write_baseline_to_disk(root, &baseline);
        let err = cmd_validate_fixtures(root).unwrap_err().to_string();
        assert!(err.contains("all-miss"), "{}", err);
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
        assert!(validate_baseline_not_all_miss(&aggregate, &suite_metrics).is_err());
        // SAFETY: this test runs single-threaded and only touches the specific env var.
        unsafe {
            std::env::set_var("DJINN_MEMORY_EVAL_TEST_OVERRIDE", "allow_all_miss_baseline");
        }
        assert!(validate_baseline_not_all_miss(&aggregate, &suite_metrics).is_ok());
        unsafe {
            std::env::remove_var("DJINN_MEMORY_EVAL_TEST_OVERRIDE");
        }
    }

    // ── AC: Missing hard signal coverage ─────────────────────────────────

    #[test]
    fn validate_fixtures_hard_fails_on_missing_graph_signal_data() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut corpus = minimal_corpus_notes();
        corpus[0].expected_signals.graph = true;
        corpus[0].graph_edges = vec![];
        let fixtures = Phase1Fixtures {
            corpus_notes: corpus,
            memory_ref_queries: make_n_queries(1),
            bad_cases: make_n_bad_cases(1),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
        write_baseline_to_disk(root, &make_baseline_with_counts(1, 1));
        let err = format!("{:?}", cmd_validate_fixtures(root).unwrap_err());
        assert!(
            err.contains("graph signal claimed but no graph_edges"),
            "{}",
            err
        );
    }

    #[test]
    fn validate_fixtures_hard_fails_on_missing_entity_signal_data() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut corpus = minimal_corpus_notes();
        corpus[1].expected_signals.entity = true;
        corpus[1].labels = vec![];
        let fixtures = Phase1Fixtures {
            corpus_notes: corpus,
            memory_ref_queries: make_n_queries(1),
            bad_cases: make_n_bad_cases(1),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
        write_baseline_to_disk(root, &make_baseline_with_counts(1, 1));
        let err = format!("{:?}", cmd_validate_fixtures(root).unwrap_err());
        assert!(
            err.contains("entity signal claimed but no labels"),
            "{}",
            err
        );
    }

    #[test]
    fn validate_fixtures_hard_fails_on_missing_task_affinity_signal_data() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut queries = make_n_queries(1);
        queries[0].expected_signals.task_affinity = true;
        queries[0].task_id = None;
        let fixtures = Phase1Fixtures {
            corpus_notes: minimal_corpus_notes(),
            memory_ref_queries: queries,
            bad_cases: make_n_bad_cases(1),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
        write_baseline_to_disk(root, &make_baseline_with_counts(1, 1));
        let err = format!("{:?}", cmd_validate_fixtures(root).unwrap_err());
        assert!(
            err.contains("task_affinity signal claimed but no task_id"),
            "{}",
            err
        );
    }

    #[test]
    fn validate_fixtures_passes_with_minimum_valid_set() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fixtures = Phase1Fixtures {
            corpus_notes: minimal_corpus_notes(),
            memory_ref_queries: make_n_queries(20),
            bad_cases: make_n_bad_cases(10),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
        write_baseline_to_disk(root, &make_baseline_with_counts(20, 10));
        assert!(
            cmd_validate_fixtures(root).is_ok(),
            "should pass with valid set"
        );
    }

    // ── AC: Missing / no-change signal comparison families ──────────────

    #[test]
    fn validate_fixtures_rejects_baseline_missing_graph_signal_comparisons() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fixtures = Phase1Fixtures {
            corpus_notes: minimal_corpus_notes(),
            memory_ref_queries: make_n_queries(20),
            bad_cases: make_n_bad_cases(10),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
        let baseline = make_baseline_with_signal_comparisons(vec![run::SignalRankComparison {
            query_id: "q-000".into(),
            signal: "task_affinity".into(),
            rank_with_signal: Some(1),
            rank_without_signal: Some(5),
            rank_changed: true,
        }]);
        write_baseline_to_disk(root, &baseline);
        let err = format!("{:?}", cmd_validate_fixtures(root).unwrap_err());
        assert!(err.contains("no graph/entity comparisons"), "{}", err);
    }

    #[test]
    fn validate_fixtures_rejects_baseline_missing_task_affinity_signal_comparisons() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fixtures = Phase1Fixtures {
            corpus_notes: minimal_corpus_notes(),
            memory_ref_queries: make_n_queries(20),
            bad_cases: make_n_bad_cases(10),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
        let baseline = make_baseline_with_signal_comparisons(vec![run::SignalRankComparison {
            query_id: "q-000".into(),
            signal: "graph".into(),
            rank_with_signal: Some(1),
            rank_without_signal: Some(5),
            rank_changed: true,
        }]);
        write_baseline_to_disk(root, &baseline);
        let err = format!("{:?}", cmd_validate_fixtures(root).unwrap_err());
        assert!(err.contains("no task-affinity comparisons"), "{}", err);
    }

    #[test]
    fn validate_fixtures_rejects_baseline_graph_comparisons_no_rank_change() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fixtures = Phase1Fixtures {
            corpus_notes: minimal_corpus_notes(),
            memory_ref_queries: make_n_queries(20),
            bad_cases: make_n_bad_cases(10),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
        let baseline = make_baseline_with_signal_comparisons(vec![
            run::SignalRankComparison {
                query_id: "q-000".into(),
                signal: "graph".into(),
                rank_with_signal: Some(1),
                rank_without_signal: Some(1),
                rank_changed: false,
            },
            run::SignalRankComparison {
                query_id: "q-001".into(),
                signal: "task_affinity".into(),
                rank_with_signal: Some(1),
                rank_without_signal: Some(5),
                rank_changed: true,
            },
        ]);
        write_baseline_to_disk(root, &baseline);
        let err = format!("{:?}", cmd_validate_fixtures(root).unwrap_err());
        assert!(err.contains("no graph/entity comparisons"), "{}", err);
    }

    #[test]
    fn validate_fixtures_rejects_baseline_task_affinity_comparisons_no_rank_change() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fixtures = Phase1Fixtures {
            corpus_notes: minimal_corpus_notes(),
            memory_ref_queries: make_n_queries(20),
            bad_cases: make_n_bad_cases(10),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
        let baseline = make_baseline_with_signal_comparisons(vec![
            run::SignalRankComparison {
                query_id: "q-000".into(),
                signal: "graph".into(),
                rank_with_signal: Some(1),
                rank_without_signal: Some(5),
                rank_changed: true,
            },
            run::SignalRankComparison {
                query_id: "q-001".into(),
                signal: "task_affinity".into(),
                rank_with_signal: Some(1),
                rank_without_signal: Some(1),
                rank_changed: false,
            },
        ]);
        write_baseline_to_disk(root, &baseline);
        let err = format!("{:?}", cmd_validate_fixtures(root).unwrap_err());
        assert!(err.contains("no task-affinity comparisons"), "{}", err);
    }

    // -- AC: Placeholder refresh metadata rejection -----------------------

    #[test]
    fn validate_fixtures_rejects_empty_refresh_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fixtures = Phase1Fixtures {
            corpus_notes: minimal_corpus_notes(),
            memory_ref_queries: make_n_queries(20),
            bad_cases: make_n_bad_cases(10),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
        let mut baseline = make_baseline_with_counts(20, 10);
        baseline.metadata.refresh_commit = String::new();
        write_baseline_to_disk(root, &baseline);
        let err = format!("{:?}", cmd_validate_fixtures(root).unwrap_err());
        assert!(err.contains("refresh_commit"), "{}", err);
        assert!(err.contains("empty"), "{}", err);
    }

    #[test]
    fn validate_fixtures_rejects_placeholder_refresh_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fixtures = Phase1Fixtures {
            corpus_notes: minimal_corpus_notes(),
            memory_ref_queries: make_n_queries(20),
            bad_cases: make_n_bad_cases(10),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
        let mut baseline = make_baseline_with_counts(20, 10);
        baseline.metadata.refresh_commit = "local-test-refresh".to_string();
        write_baseline_to_disk(root, &baseline);
        let err = format!("{:?}", cmd_validate_fixtures(root).unwrap_err());
        assert!(err.contains("known placeholder"), "{}", err);
        assert!(err.contains("local-test-refresh"), "{}", err);
    }

    #[test]
    fn validate_fixtures_rejects_unknown_refresh_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fixtures = Phase1Fixtures {
            corpus_notes: minimal_corpus_notes(),
            memory_ref_queries: make_n_queries(20),
            bad_cases: make_n_bad_cases(10),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
        let mut baseline = make_baseline_with_counts(20, 10);
        baseline.metadata.refresh_commit = "unknown".to_string();
        write_baseline_to_disk(root, &baseline);
        let err = format!("{:?}", cmd_validate_fixtures(root).unwrap_err());
        assert!(err.contains("known placeholder"), "{}", err);
    }

    #[test]
    fn validate_fixtures_accepts_valid_commit_sha() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fixtures = Phase1Fixtures {
            corpus_notes: minimal_corpus_notes(),
            memory_ref_queries: make_n_queries(20),
            bad_cases: make_n_bad_cases(10),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
        let mut baseline = make_baseline_with_counts(20, 10);
        baseline.metadata.refresh_commit = "abcdef0123456789abcdef0123456789abcdef01".to_string();
        write_baseline_to_disk(root, &baseline);
        assert!(
            cmd_validate_fixtures(root).is_ok(),
            "should pass with a valid hex commit SHA"
        );
    }

    #[test]
    fn validate_fixtures_accepts_abbreviated_commit_sha() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fixtures = Phase1Fixtures {
            corpus_notes: minimal_corpus_notes(),
            memory_ref_queries: make_n_queries(20),
            bad_cases: make_n_bad_cases(10),
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);
        let mut baseline = make_baseline_with_counts(20, 10);
        baseline.metadata.refresh_commit = "abcdef0".to_string();
        write_baseline_to_disk(root, &baseline);
        assert!(
            cmd_validate_fixtures(root).is_ok(),
            "should pass with a 7-char abbreviated commit SHA"
        );
    }

    // ── AC: Over-decay age-bucket missing from baseline ─────────────────

    /// Validation fails when over-decay-threshold fixtures exist but the
    /// baseline age_bucket_recall omits the over_decay_threshold bucket.
    #[test]
    fn validate_fixtures_fails_when_over_decay_bucket_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let corpus = minimal_corpus_notes();
        let queries = make_n_queries(20);
        let bad_cases = make_n_bad_cases(10);

        let fixtures = Phase1Fixtures {
            corpus_notes: corpus,
            memory_ref_queries: queries,
            bad_cases,
            manifest: None,
        };
        write_fixtures_to_disk(root, &fixtures);

        // Build a valid baseline then remove the over_decay_threshold bucket
        let mut baseline = make_baseline_with_counts(20, 10);
        baseline
            .age_bucket_recall
            .remove(&metrics::AgeBucket::OverDecayThreshold);
        write_baseline_to_disk(root, &baseline);

        let result = cmd_validate_fixtures(root);
        assert!(
            result.is_err(),
            "should fail when over-decay fixtures exist but baseline omits over_decay_threshold bucket"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("over_decay_threshold"),
            "error should mention over_decay_threshold: {}",
            err
        );
    }
}
