#![allow(clippy::print_stderr, clippy::print_stdout)]

//! Operator-run example for the Phase 1 GO/STOP telemetry workflow.
//!
//! Exports persisted session transcripts from a fixed 30-day window, builds a
//! matched baseline, evaluates the GO/STOP report, and emits a deterministic
//! 20-trace failed-edit audit frame. Production credentials are supplied via
//! `DATABASE_URL`; the example does not run in CI and does not assert that a
//! production collection or manual audit occurred.
//!
//! Usage (production):
//!
//! ```text
//! DATABASE_URL=postgres://... cargo run -p djinn-db --example run_telemetry_analysis -- \
//!     --window-start 2026-07-01 \
//!     --window-end 2026-07-30 \
//!     --output report.json \
//!     --audit-sampled 20 \
//!     --audit-qualifying 12 \
//!     --candidate-family codex \
//!     --baseline-families default,Responses/default
//! ```
//!
//! If `--audit-sampled` and `--audit-qualifying` are omitted, the report is
//! evaluated with the audit absent and will be `insufficient data`. The
//! deterministic 20-trace sample frame is still printed so the operator can
//! perform the manual audit and re-run with the counts.

use anyhow::{Context, Result};
use chrono::NaiveDate;
use djinn_core::models::SessionRecord;
use djinn_db::{
    Database, DatabaseConnectConfig, EvalInput, ExportDimensions, GateThresholds,
    ManualAuditResult, NormalizedToolCallRow, PostgresDatabaseConfig, SampleMinima,
    ToolCallExportRepository, WindowSpec, evaluate, matched_baseline_rows,
};

struct Args {
    window_start: String,
    window_end: String,
    output: String,
    source_description: String,
    candidate_family: String,
    baseline_families: Vec<String>,
    audit_sampled: Option<usize>,
    audit_qualifying: Option<usize>,
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1).peekable();
    let mut window_start = None;
    let mut window_end = None;
    let mut output = None;
    let mut source_description = None;
    let mut candidate_family = None;
    let mut baseline_families = None;
    let mut audit_sampled = None;
    let mut audit_qualifying = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--window-start" => window_start = args.next(),
            "--window-end" => window_end = args.next(),
            "--output" => output = args.next(),
            "--source-description" => source_description = args.next(),
            "--candidate-family" => candidate_family = args.next(),
            "--baseline-families" => {
                baseline_families = args.next().map(|s| {
                    s.split(',')
                        .map(|x| x.trim().to_owned())
                        .collect::<Vec<_>>()
                });
            }
            "--audit-sampled" => {
                audit_sampled = args.next().map(|s| s.parse::<usize>().unwrap_or(0));
            }
            "--audit-qualifying" => {
                audit_qualifying = args.next().map(|s| s.parse::<usize>().unwrap_or(0));
            }
            other if other.starts_with('-') => {
                eprintln!("warning: unknown flag {other}");
            }
            _ => {}
        }
    }

    fn require(name: &str, value: Option<String>) -> String {
        value.unwrap_or_else(|| {
            eprintln!("Missing required argument: {name}");
            eprintln!("Usage: DATABASE_URL=... cargo run -p djinn-db --example run_telemetry_analysis -- --window-start YYYY-MM-DD --window-end YYYY-MM-DD --output report.json");
            std::process::exit(1);
        })
    }

    let start = require("--window-start", window_start);
    let end = require("--window-end", window_end);

    // Validate that the window is an inclusive 30-day window (29 days between).
    let start_date = NaiveDate::parse_from_str(&start, "%Y-%m-%d").unwrap_or_else(|e| {
        eprintln!("Invalid --window-start {start}: {e}");
        std::process::exit(1);
    });
    let end_date = NaiveDate::parse_from_str(&end, "%Y-%m-%d").unwrap_or_else(|e| {
        eprintln!("Invalid --window-end {end}: {e}");
        std::process::exit(1);
    });
    if (end_date - start_date).num_days() != 29 {
        eprintln!(
            "Expected an inclusive 30-day window (start to end = 29 days), got {} days",
            (end_date - start_date).num_days()
        );
        std::process::exit(1);
    }

    Args {
        window_start: start,
        window_end: end,
        output: require("--output", output),
        source_description: source_description
            .unwrap_or_else(|| "persisted session transcripts".to_owned()),
        candidate_family: candidate_family.unwrap_or_else(|| "codex".to_owned()),
        baseline_families: baseline_families
            .unwrap_or_else(|| vec!["default".to_owned(), "Responses/default".to_owned()]),
        audit_sampled,
        audit_qualifying,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args();

    let db_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL environment variable is required for the production export")?;

    let db = Database::open_with_config(DatabaseConnectConfig::Postgres(PostgresDatabaseConfig {
        url: db_url,
    }))?;
    db.ensure_initialized().await?;

    // The query uses a half-open interval [start, end_next_day) so the
    // inclusive end date is fully covered regardless of the `started_at`
    // timestamp precision.
    let end_next_day = NaiveDate::parse_from_str(&args.window_end, "%Y-%m-%d")?
        .succ_opt()
        .context("could not compute day after window end")?
        .to_string();

    let sessions: Vec<SessionRecord> = sqlx::query_as::<_, SessionRecord>(
        "SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at, \
         status, tokens_in, tokens_out, cache_read_tokens, cache_write_tokens, \
         task_run_id, title, parked_reason, cost_usd, input_price_per_million_snapshot, \
         output_price_per_million_snapshot, cache_read_price_per_million_snapshot, \
         cache_write_price_per_million_snapshot, cost_basis, billing_source \
         FROM sessions \
         WHERE started_at >= $1 AND started_at < $2 \
           AND task_id IS NOT NULL \
           AND agent_type <> 'chat' \
         ORDER BY started_at ASC, id ASC",
    )
    .bind(&args.window_start)
    .bind(&end_next_day)
    .fetch_all(db.pool())
    .await
    .context("failed to query sessions for the analysis window")?;

    eprintln!("Found {} sessions in the window", sessions.len());

    let repo = ToolCallExportRepository::new(db.clone());

    let candidate_dimensions = ExportDimensions {
        provider_id: Some("openai".into()),
        format_family: Some("OpenAIResponses".into()),
        tool_surface_family: Some(args.candidate_family.clone()),
    };

    let mut candidate_rows: Vec<NormalizedToolCallRow> = Vec::new();
    let mut baseline_pool: Vec<NormalizedToolCallRow> = Vec::new();

    for session in &sessions {
        let mut rows = repo
            .export_session(session.clone(), candidate_dimensions.clone())
            .await
            .with_context(|| format!("failed to export session {}", session.id))?;
        candidate_rows.append(&mut rows);

        for family in &args.baseline_families {
            let baseline_dimensions = ExportDimensions {
                provider_id: Some("openai".into()),
                format_family: Some("OpenAIResponses".into()),
                tool_surface_family: Some(family.clone()),
            };
            let mut rows = repo
                .export_session(session.clone(), baseline_dimensions)
                .await
                .with_context(|| {
                    format!(
                        "failed to export session {} as baseline {}",
                        session.id, family
                    )
                })?;
            baseline_pool.append(&mut rows);
        }
    }

    // Optional Langfuse/OTel enrichment would join here on session/task IDs and
    // copy only bounded fields (latency, token counts, resolved model_id, trace
    // ID). It must not replace transcript rows or copy free-text prompt/source
    // content.

    let candidate_rows: Vec<_> = candidate_rows
        .into_iter()
        .filter(|r| r.tool_surface_family.as_deref() == Some(&args.candidate_family))
        .collect();

    let baseline_rows =
        matched_baseline_rows(&candidate_rows, &baseline_pool, &args.baseline_families);

    let audit = match (args.audit_sampled, args.audit_qualifying) {
        (Some(sampled), Some(qualifying)) => Some(ManualAuditResult::new(sampled, qualifying)),
        (Some(_), None) | (None, Some(_)) => {
            eprintln!("--audit-sampled and --audit-qualifying must be supplied together");
            std::process::exit(1);
        }
        (None, None) => None,
    };

    let input = EvalInput {
        window: WindowSpec {
            start_day: args.window_start,
            end_day: args.window_end,
            source_description: args.source_description,
        },
        candidate_rows,
        baseline_rows,
        audit,
    };

    let report = evaluate(
        &input,
        Some(SampleMinima::default()),
        Some(GateThresholds::default()),
    );

    let report_json = serde_json::to_string_pretty(&report)?;
    std::fs::write(&args.output, report_json)
        .with_context(|| format!("failed to write report to {}", args.output))?;
    eprintln!("Wrote GO/STOP report to {}", args.output);

    // Deterministic 20-trace failed-edit audit frame.
    let mut frame: Vec<&NormalizedToolCallRow> = input
        .candidate_rows
        .iter()
        .filter(|r| r.tool_name == "edit" && r.result_status != "success")
        .collect();
    frame.sort_by(|a, b| {
        a.session_id
            .cmp(&b.session_id)
            .then(a.task_id.cmp(&b.task_id))
            .then(a.turn_index.cmp(&b.turn_index))
            .then(a.tool_call_id.cmp(&b.tool_call_id))
    });
    let frame = &frame[..frame.len().min(20)];

    let sample_json: Vec<serde_json::Value> = frame
        .iter()
        .map(|r| {
            serde_json::json!({
                "session_id": r.session_id,
                "task_id": r.task_id,
                "tool_call_id": r.tool_call_id,
                "turn_index": r.turn_index,
                "tool_name": r.tool_name,
                "result_status": r.result_status,
                "error_class": r.error_class,
                "error_text": r.error_text,
                "path": r.path,
            })
        })
        .collect();

    eprintln!(
        "\nDeterministic 20-trace failed-edit audit frame ({} of {} qualifying rows):",
        sample_json.len(),
        input
            .candidate_rows
            .iter()
            .filter(|r| r.tool_name == "edit" && r.result_status != "success")
            .count()
    );
    eprintln!("{}", serde_json::to_string_pretty(&sample_json)?);
    if sample_json.len() < 20 {
        eprintln!(
            "\nWARNING: fewer than 20 failed edit traces exist; the audit is incomplete and the report must be 'insufficient data'."
        );
    }
    eprintln!("\nOperator next steps:");
    eprintln!(
        "1. Classify each sampled trace as genuine surface-confusion/context failure or not."
    );
    eprintln!(
        "2. Re-run with --audit-sampled 20 --audit-qualifying <count> to emit the final decision."
    );

    Ok(())
}
