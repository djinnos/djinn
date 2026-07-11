//! Phase 2 QA judge orchestration and artifacts.
//!
//! The scheduled/manual workflow calls this command after the deterministic
//! `qa-run` capture. The command intentionally enforces the Phase 2 credential
//! boundary before any model call: anonymous/default-owner model spend is never
//! allowed. When the required credentialed model slot is absent, the command
//! still writes the Phase 2 report and summary with a visible `credential_error`
//! status, then returns an error for the non-gating workflow step to surface.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::qa_run::QaRunOutput;

const AGREEMENT_TARGET: f64 = 0.95;

/// Machine-readable Phase 2 QA judge report.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct QaJudgeReport {
    /// Report schema version for downstream nightly trend ingestion.
    pub schema_version: String,
    /// The credentialed model slot requested for the judge passes.
    pub model_slot: Option<String>,
    /// High-level judge status: `credential_error`, `provider_error`, or
    /// `completed` once provider-backed judging is wired.
    pub judge_status: String,
    /// Human-readable status/error message.
    pub message: String,
    /// The deterministic retrieval/injection capture that judge passes consume.
    pub qa_run: QaRunOutput,
    /// Inter-judge agreement threshold documented for Phase 2 trending.
    pub agreement_target: f64,
    /// Inter-judge agreement rate when both passes complete for at least one QA
    /// pair. Missing on credential/provider failures.
    pub inter_judge_agreement_rate: Option<f64>,
    /// Total attributed cost across both judge passes. Missing for failures or
    /// unpriced provider sessions; never backfilled as zero/free.
    pub total_cost_usd: Option<f64>,
    /// Cost attribution rows for individual judge passes.
    pub judge_pass_costs: Vec<JudgePassCost>,
}

/// Cost attribution fields expected from each Phase 2 judge pass.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct JudgePassCost {
    pub qa_id: String,
    pub pass_id: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub cost_basis: String,
    pub cost_usd: Option<f64>,
    pub unpriced_reason: Option<String>,
}

/// Execute Phase 2 judge orchestration and write artifacts.
pub async fn execute_qa_judge(crate_root: &Path) -> Result<()> {
    let qa_run = crate::qa_run::execute_qa_run(crate_root)
        .await
        .context("running Phase 2 QA retrieval/injection capture for judge input")?;

    let target_dir = PathBuf::from("target/memory-eval");
    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("creating output directory {}", target_dir.display()))?;

    let model_slot = normalized_env("DJINN_MEMORY_QA_JUDGE_MODEL");
    if model_slot.is_none() {
        let message = "Phase 2 QA judge requires DJINN_MEMORY_QA_JUDGE_MODEL to name an explicit credentialed model slot; anonymous/default-owner model spend fallback is forbidden";
        let report = QaJudgeReport {
            schema_version: "phase2-qa-v1".to_string(),
            model_slot: None,
            judge_status: "credential_error".to_string(),
            message: message.to_string(),
            qa_run,
            agreement_target: AGREEMENT_TARGET,
            inter_judge_agreement_rate: None,
            total_cost_usd: None,
            judge_pass_costs: Vec::new(),
        };
        write_artifacts(&target_dir, &report)
            .context("writing Phase 2 credential-error artifacts")?;
        bail!(message);
    }

    // The CLI subcommand exists so the nightly workflow can invoke the Phase 2
    // judge path and distinguish real credential/provider failures from an
    // unknown-command wiring error. Provider-backed dual-pass grading must land
    // behind this explicit model-slot boundary; until that integration is
    // available, fail visibly as a non-gating provider result rather than
    // silently substituting a deterministic or anonymous judge.
    let message = "Phase 2 QA judge provider integration is unavailable in this build; no anonymous/default-owner model spend fallback was attempted";
    let report = QaJudgeReport {
        schema_version: "phase2-qa-v1".to_string(),
        model_slot,
        judge_status: "provider_error".to_string(),
        message: message.to_string(),
        qa_run,
        agreement_target: AGREEMENT_TARGET,
        inter_judge_agreement_rate: None,
        total_cost_usd: None,
        judge_pass_costs: Vec::new(),
    };
    write_artifacts(&target_dir, &report).context("writing Phase 2 provider-error artifacts")?;
    bail!(message)
}

fn normalized_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn write_artifacts(target_dir: &Path, report: &QaJudgeReport) -> Result<()> {
    let report_path = target_dir.join("phase2-qa-report.json");
    let summary_path = target_dir.join("phase2-qa-summary.md");

    let report_json =
        serde_json::to_string_pretty(report).context("serializing Phase 2 QA judge report")?;
    std::fs::write(&report_path, report_json)
        .with_context(|| format!("writing {}", report_path.display()))?;

    std::fs::write(&summary_path, render_summary(report))
        .with_context(|| format!("writing {}", summary_path.display()))?;
    Ok(())
}

fn render_summary(report: &QaJudgeReport) -> String {
    let mut summary = String::new();
    summary.push_str("# Phase 2 Memory QA Judge\n\n");
    summary.push_str(
        "This nightly/manual job is non-gating and is not a PR or merge-queue check.\n\n",
    );
    summary.push_str(&format!("- Status: `{}`\n", report.judge_status));
    summary.push_str(&format!("- Message: {}\n", report.message));
    summary.push_str(&format!(
        "- QA pairs: {}\n- Retrieval hits: {}\n- Context recalls: {}\n",
        report.qa_run.qa_count,
        report.qa_run.retrieval_hit_count,
        report.qa_run.context_recall_count
    ));
    summary.push_str(&format!(
        "- Inter-judge agreement target: {:.0}%\n",
        report.agreement_target * 100.0
    ));
    match report.inter_judge_agreement_rate {
        Some(rate) => summary.push_str(&format!("- Inter-judge agreement: {:.2}%\n", rate * 100.0)),
        None => summary
            .push_str("- Inter-judge agreement: unavailable (judge passes did not complete)\n"),
    }
    match report.total_cost_usd {
        Some(cost) => summary.push_str(&format!("- Total attributed judge cost: ${cost:.6}\n")),
        None => summary
            .push_str("- Total attributed judge cost: unavailable/unpriced; not treated as free\n"),
    }
    summary.push_str("\nCredential/provider failures are expected to be visible here and in the JSON artifact without blocking merges.\n");
    summary
}
