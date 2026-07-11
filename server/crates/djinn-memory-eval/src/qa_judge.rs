//! Phase 2 QA judge orchestration and artifacts.
//!
//! The scheduled/manual workflow calls this command after the deterministic
//! `qa-run` capture. The command intentionally enforces the Phase 2 credential
//! boundary before any model call: anonymous/default-owner model spend is never
//! allowed. When the required credentialed model slot is absent, the command
//! still writes the Phase 2 report and summary with a visible `credential_error`
//! status, then returns an error for the non-gating workflow step to surface.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use djinn_core::models::{Model, Pricing};
use djinn_provider::catalog::{CatalogService, builtin};
use djinn_provider::provider::{
    ProviderConfig, TelemetryMeta, create_provider, default_reasoning_effort_for_model,
};
use djinn_provider::{CompletionRequest, CompletionResponse, complete};

use crate::qa::QaPair;
use crate::qa_run::{QaRunOutput, QaRunRecord};

const AGREEMENT_TARGET: f64 = 0.95;
const JUDGE_MAX_TOKENS: u32 = 512;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct QaJudgeReport {
    pub schema_version: String,
    pub model_slot: Option<String>,
    pub judge_status: String,
    pub message: String,
    pub qa_run: QaRunOutput,
    pub agreement_target: f64,
    pub inter_judge_agreement_rate: Option<f64>,
    pub total_cost_usd: Option<f64>,
    pub judge_results: Vec<JudgePassResult>,
    pub judge_pass_costs: Vec<JudgePassCost>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct JudgePassResult {
    pub qa_id: String,
    pub pass_id: String,
    pub verdict: String,
    pub correct: bool,
    pub rationale: String,
    pub retrieval_hit: bool,
    pub context_recall: bool,
}

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

struct JudgeProvider {
    model_slot: String,
    model: Model,
    cost_basis: String,
    provider: Box<dyn djinn_provider::provider::LlmProvider>,
}

#[derive(Debug, serde::Deserialize)]
struct RawJudgeVerdict {
    verdict: String,
    #[serde(default)]
    rationale: String,
}

pub async fn execute_qa_judge(crate_root: &Path) -> Result<()> {
    let qa_run = crate::qa_run::execute_qa_run(crate_root)
        .await
        .context("running Phase 2 QA retrieval/injection capture for judge input")?;
    let target_dir = PathBuf::from("target/memory-eval");
    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("creating output directory {}", target_dir.display()))?;

    let Some(model_slot) = normalized_env("DJINN_MEMORY_QA_JUDGE_MODEL") else {
        let message = "Phase 2 QA judge requires DJINN_MEMORY_QA_JUDGE_MODEL to name an explicit credentialed model slot; anonymous/default-owner model spend fallback is forbidden";
        let report = build_report(
            None,
            "credential_error",
            message,
            qa_run,
            Vec::new(),
            Vec::new(),
            None,
        );
        write_artifacts(&target_dir, &report)
            .context("writing Phase 2 credential-error artifacts")?;
        bail!(message);
    };

    let judge = match resolve_env_model_slot(&model_slot) {
        Ok(judge) => judge,
        Err(error) => {
            let message = format!(
                "Phase 2 QA judge credential/model-slot error for '{model_slot}': {error}; anonymous/default-owner model spend fallback is forbidden"
            );
            let report = build_report(
                Some(model_slot),
                "credential_error",
                &message,
                qa_run,
                Vec::new(),
                Vec::new(),
                None,
            );
            write_artifacts(&target_dir, &report)
                .context("writing Phase 2 credential-error artifacts")?;
            bail!(message);
        }
    };

    let qa_by_id = qa_run
        .extraction
        .pairs
        .iter()
        .map(|pair| (pair.qa_id.as_str(), pair))
        .collect::<HashMap<_, _>>();
    let mut results = Vec::new();
    let mut costs = Vec::new();

    for record in &qa_run.records {
        let qa_pair = qa_by_id
            .get(record.qa_id.as_str())
            .ok_or_else(|| anyhow!("QA pair '{}' missing from extraction report", record.qa_id))?;
        for pass_id in ["judge-a", "judge-b"] {
            match run_judge_pass(&judge, record, qa_pair, pass_id).await {
                Ok((result, cost)) => {
                    results.push(result);
                    costs.push(cost);
                }
                Err(error) => {
                    let message = format!(
                        "Phase 2 QA judge provider error on {} {pass_id}: {error}",
                        record.qa_id
                    );
                    let report = build_report(
                        Some(judge.model_slot),
                        "provider_error",
                        &message,
                        qa_run,
                        results,
                        costs,
                        None,
                    );
                    write_artifacts(&target_dir, &report)
                        .context("writing Phase 2 provider-error artifacts")?;
                    bail!(message);
                }
            }
        }
    }

    let agreement = inter_judge_agreement(&results);
    let message = format!(
        "Phase 2 QA judge completed {} dual-pass QA pairs with credentialed model slot '{}'",
        qa_run.records.len(),
        judge.model_slot
    );
    let report = build_report(
        Some(judge.model_slot),
        "completed",
        &message,
        qa_run,
        results,
        costs,
        agreement,
    );
    write_artifacts(&target_dir, &report).context("writing Phase 2 completed artifacts")?;
    Ok(())
}

fn build_report(
    model_slot: Option<String>,
    judge_status: &str,
    message: &str,
    qa_run: QaRunOutput,
    judge_results: Vec<JudgePassResult>,
    judge_pass_costs: Vec<JudgePassCost>,
    inter_judge_agreement_rate: Option<f64>,
) -> QaJudgeReport {
    let total_cost_usd = total_cost(&judge_pass_costs);
    QaJudgeReport {
        schema_version: "phase2-qa-v1".to_string(),
        model_slot,
        judge_status: judge_status.to_string(),
        message: message.to_string(),
        qa_run,
        agreement_target: AGREEMENT_TARGET,
        inter_judge_agreement_rate,
        total_cost_usd,
        judge_results,
        judge_pass_costs,
    }
}

fn resolve_env_model_slot(model_slot: &str) -> Result<JudgeProvider> {
    let catalog = CatalogService::new();
    catalog.inject_builtin_providers(builtin::BUILTIN_PROVIDERS);
    let model = catalog.find_model(model_slot).with_context(|| {
        format!("model slot '{model_slot}' is not available in the provider catalog")
    })?;
    let provider_id = model.provider_id.clone();
    let builtin_provider = builtin::find_builtin_provider(&provider_id)
        .with_context(|| format!("provider '{provider_id}' is not supported by djinn-provider"))?;
    let key_name = builtin_provider.required_env_vars.first().ok_or_else(|| {
        anyhow!("provider '{provider_id}' for model slot '{model_slot}' has no API-key env credential path for nightly QA")
    })?;
    let api_key = normalized_env(key_name).ok_or_else(|| {
        anyhow!("provider '{provider_id}' for model slot '{model_slot}' is missing credential env '{key_name}'")
    })?;
    let provider_row = catalog
        .list_providers()
        .into_iter()
        .find(|provider| provider.id == provider_id)
        .with_context(|| format!("provider '{provider_id}' missing from catalog"))?;
    let format_family = builtin_provider.format_family(&model.id);
    let config = ProviderConfig {
        base_url: provider_row.base_url,
        auth: builtin_provider.auth_method(api_key),
        format_family,
        model_id: model.id.clone(),
        context_window: model.context_window.max(0) as u32,
        telemetry: Some(TelemetryMeta {
            operation: Some("memory_qa_phase2_judge".to_string()),
            ..TelemetryMeta::default()
        }),
        session_affinity_key: Some("memory-qa-phase2-nightly".to_string()),
        provider_headers: Default::default(),
        capabilities: builtin_provider.capabilities(),
        reasoning_effort: default_reasoning_effort_for_model(
            model.reasoning,
            format_family,
            &model.id,
        ),
        tool_schema_compat: builtin::tool_schema_compat_for(&provider_id, &model.id),
    };
    let cost_basis = if !has_pricing(&model.pricing) {
        "unpriced"
    } else if builtin_provider.is_subscription() {
        "projected"
    } else {
        "actual"
    }
    .to_string();
    Ok(JudgeProvider {
        model_slot: model_slot.to_string(),
        model,
        cost_basis,
        provider: create_provider(config),
    })
}

async fn run_judge_pass(
    judge: &JudgeProvider,
    record: &QaRunRecord,
    qa_pair: &QaPair,
    pass_id: &str,
) -> Result<(JudgePassResult, JudgePassCost)> {
    let response = complete(
        judge.provider.as_ref(),
        CompletionRequest {
            system: judge_system_prompt(pass_id),
            prompt: judge_prompt(record, qa_pair, pass_id),
            max_tokens: JUDGE_MAX_TOKENS,
        },
    )
    .await
    .with_context(|| format!("calling credentialed model slot '{}'", judge.model_slot))?;
    let verdict = parse_verdict(&response.text)
        .with_context(|| format!("parsing judge JSON for {} {pass_id}", record.qa_id))?;
    let correct = verdict.verdict.eq_ignore_ascii_case("CORRECT");
    let result = JudgePassResult {
        qa_id: record.qa_id.clone(),
        pass_id: pass_id.to_string(),
        verdict: if correct { "CORRECT" } else { "WRONG" }.to_string(),
        correct,
        rationale: verdict.rationale,
        retrieval_hit: record.retrieval_hit,
        context_recall: record.context_recall,
    };
    let cost = cost_row(judge, record, pass_id, &response);
    Ok((result, cost))
}

fn judge_system_prompt(pass_id: &str) -> String {
    format!(
        "You are Phase 2 memory QA judge pass {pass_id}. Grade only whether the injected memory context contains the gold answer needed to answer the question. Return strict JSON only: {{\"verdict\":\"CORRECT\"|\"WRONG\",\"rationale\":\"short reason\"}}. Do not use outside knowledge."
    )
}

fn judge_prompt(record: &QaRunRecord, qa_pair: &QaPair, pass_id: &str) -> String {
    format!(
        "Pass: {pass_id}\nQA id: {qa_id}\nSource note: {source}\nQuestion:\n{question}\n\nGold answer that must be present in the injected context:\n{gold}\n\nInjected memory context (exact 2000-character session-start payload):\n{payload}\n\nTop-k retrieval permalinks: {permalinks:?}\nRetrieval hit: {retrieval_hit}\nDeterministic context recall substring check: {context_recall}\n\nReturn JSON only.",
        qa_id = record.qa_id,
        source = record.source_permalink,
        question = record.question,
        gold = qa_pair.gold_answer,
        payload = record.injected_payload,
        permalinks = record.result_permalinks,
        retrieval_hit = record.retrieval_hit,
        context_recall = record.context_recall,
    )
}

fn parse_verdict(text: &str) -> Result<RawJudgeVerdict> {
    let trimmed = text.trim();
    let json_text = if trimmed.starts_with('{') {
        trimmed
    } else {
        let start = trimmed
            .find('{')
            .ok_or_else(|| anyhow!("judge response did not contain a JSON object: {trimmed}"))?;
        let end = trimmed.rfind('}').ok_or_else(|| {
            anyhow!("judge response did not contain a complete JSON object: {trimmed}")
        })?;
        &trimmed[start..=end]
    };
    let verdict: RawJudgeVerdict = serde_json::from_str(json_text)?;
    if !matches!(verdict.verdict.as_str(), "CORRECT" | "WRONG") {
        bail!(
            "judge verdict must be CORRECT or WRONG, got '{}'; raw response: {text}",
            verdict.verdict
        );
    }
    Ok(verdict)
}

fn cost_row(
    judge: &JudgeProvider,
    record: &QaRunRecord,
    pass_id: &str,
    response: &CompletionResponse,
) -> JudgePassCost {
    let input_tokens = u64::from(response.input_tokens);
    let output_tokens = u64::from(response.output_tokens);
    let cost_usd = priced_cost(&judge.model.pricing, input_tokens, output_tokens);
    let unpriced_reason = cost_usd.is_none().then(|| {
        format!(
            "model slot '{}' has no non-zero catalog pricing; cost is unknown, not free",
            judge.model_slot
        )
    });
    JudgePassCost {
        qa_id: record.qa_id.clone(),
        pass_id: pass_id.to_string(),
        input_tokens: Some(input_tokens),
        output_tokens: Some(output_tokens),
        cache_read_tokens: None,
        cache_write_tokens: None,
        cost_basis: judge.cost_basis.clone(),
        cost_usd,
        unpriced_reason,
    }
}

fn has_pricing(pricing: &Pricing) -> bool {
    pricing.input_per_million != 0.0
        || pricing.output_per_million != 0.0
        || pricing.cache_read_per_million != 0.0
        || pricing.cache_write_per_million != 0.0
}

fn priced_cost(pricing: &Pricing, input_tokens: u64, output_tokens: u64) -> Option<f64> {
    has_pricing(pricing).then_some(
        (input_tokens as f64 * pricing.input_per_million
            + output_tokens as f64 * pricing.output_per_million)
            / 1_000_000.0,
    )
}

fn inter_judge_agreement(results: &[JudgePassResult]) -> Option<f64> {
    let mut by_qa: HashMap<&str, Vec<&JudgePassResult>> = HashMap::new();
    for result in results {
        by_qa.entry(result.qa_id.as_str()).or_default().push(result);
    }
    let mut compared = 0usize;
    let mut agreed = 0usize;
    for passes in by_qa.values() {
        if passes.len() == 2 {
            compared += 1;
            if passes[0].correct == passes[1].correct {
                agreed += 1;
            }
        }
    }
    (compared > 0).then_some(agreed as f64 / compared as f64)
}

fn total_cost(costs: &[JudgePassCost]) -> Option<f64> {
    let mut total = 0.0;
    for cost in costs {
        total += cost.cost_usd?;
    }
    Some(total)
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
        "- QA pairs: {}\n- Retrieval hits: {}\n- Context recalls: {}\n- Judge pass results: {}\n",
        report.qa_run.qa_count,
        report.qa_run.retrieval_hit_count,
        report.qa_run.context_recall_count,
        report.judge_results.len()
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
