//! Capture raw MiniMax Anthropic-compatible streaming `data:` frames safely.
//!
//! Example:
//!
//! ```text
//! MINIMAX_API_KEY=... cargo run -p djinn-provider --example capture_minimax_sse -- \
//!   --base-url https://api.minimax.io/anthropic/v1 \
//!   --model MiniMax-M3 \
//!   --reasoning-effort low \
//!   --output /var/tmp/minimax-sse-capture.json
//! ```
//!
//! Use `--dry-run` to write sanitized request metadata without network access.

use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;

use anyhow::{Context, anyhow};
use djinn_provider::provider::capture::{
    AnthropicSseCaptureConfig, capture_anthropic_sse_to_file, dry_run_anthropic_sse_capture,
    parse_header_pair,
};
use djinn_provider::provider::{AuthMethod, ReasoningEffort};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;
    let config = AnthropicSseCaptureConfig {
        base_url: args.base_url,
        model: args.model,
        auth: args.auth,
        prompt: args.prompt,
        reasoning_effort: args.reasoning_effort,
        max_tokens: args.max_tokens,
        provider_headers: args.headers,
    };

    if args.dry_run {
        let artifact = dry_run_anthropic_sse_capture(&config)?;
        let json = serde_json::to_string_pretty(&artifact)?;
        tokio::fs::write(&args.output, json)
            .await
            .with_context(|| format!("write dry-run artifact to {}", args.output.display()))?;
    } else {
        capture_anthropic_sse_to_file(config, &args.output).await?;
    }

    eprintln!(
        "wrote sanitized MiniMax/Anthropic SSE capture artifact to {}",
        args.output.display()
    );
    Ok(())
}

struct Args {
    base_url: String,
    model: String,
    auth: AuthMethod,
    prompt: String,
    reasoning_effort: Option<ReasoningEffort>,
    max_tokens: u32,
    output: PathBuf,
    headers: BTreeMap<String, String>,
    dry_run: bool,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut base_url = env::var("MINIMAX_BASE_URL")
            .unwrap_or_else(|_| "https://api.minimax.io/anthropic/v1".to_string());
        let mut model = env::var("MINIMAX_MODEL").unwrap_or_else(|_| "MiniMax-M3".to_string());
        let mut api_key = env::var("MINIMAX_API_KEY").ok();
        let mut auth_header = env::var("MINIMAX_AUTH_HEADER").ok();
        let mut prompt = env::var("MINIMAX_CAPTURE_PROMPT").unwrap_or_else(|_| {
            "Reply with one short sentence. If thinking is available, use the provider's structured thinking stream rather than inline <think> tags.".to_string()
        });
        let mut reasoning_effort = env::var("MINIMAX_REASONING_EFFORT")
            .ok()
            .map(|value| parse_reasoning_effort(&value))
            .transpose()?;
        let mut max_tokens = env::var("MINIMAX_MAX_TOKENS")
            .ok()
            .map(|value| value.parse::<u32>().context("parse MINIMAX_MAX_TOKENS"))
            .transpose()?
            .unwrap_or(4097);
        let mut output = env::var("MINIMAX_CAPTURE_OUTPUT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("minimax-anthropic-sse-capture.json"));
        let mut headers = BTreeMap::new();
        let mut dry_run = false;

        let mut iter = env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--base-url" => base_url = next_value(&mut iter, "--base-url")?,
                "--model" => model = next_value(&mut iter, "--model")?,
                "--api-key" => api_key = Some(next_value(&mut iter, "--api-key")?),
                "--auth-header" => auth_header = Some(next_value(&mut iter, "--auth-header")?),
                "--prompt" => prompt = next_value(&mut iter, "--prompt")?,
                "--reasoning-effort" => {
                    reasoning_effort = Some(parse_reasoning_effort(&next_value(
                        &mut iter,
                        "--reasoning-effort",
                    )?)?);
                }
                "--max-tokens" => {
                    max_tokens = next_value(&mut iter, "--max-tokens")?
                        .parse()
                        .context("parse --max-tokens")?;
                }
                "--output" => output = PathBuf::from(next_value(&mut iter, "--output")?),
                "--header" => {
                    let (name, value) = parse_header_pair(&next_value(&mut iter, "--header")?)?;
                    headers.insert(name, value);
                }
                "--dry-run" => dry_run = true,
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                other => return Err(anyhow!("unknown argument: {other}; use --help")),
            }
        }

        let auth = if let Some(key) = api_key {
            match auth_header {
                Some(header) => AuthMethod::ApiKeyHeader { header, key },
                None => AuthMethod::BearerToken(key),
            }
        } else if dry_run {
            AuthMethod::NoAuth
        } else {
            return Err(anyhow!(
                "MINIMAX_API_KEY or --api-key is required unless --dry-run is set"
            ));
        };

        Ok(Self {
            base_url,
            model,
            auth,
            prompt,
            reasoning_effort,
            max_tokens,
            output,
            headers,
            dry_run,
        })
    }
}

fn next_value(iter: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    iter.next()
        .ok_or_else(|| anyhow!("{flag} requires a value"))
}

fn parse_reasoning_effort(value: &str) -> anyhow::Result<ReasoningEffort> {
    match value.to_ascii_lowercase().as_str() {
        "minimal" => Ok(ReasoningEffort::Minimal),
        "low" => Ok(ReasoningEffort::Low),
        "medium" => Ok(ReasoningEffort::Medium),
        "high" => Ok(ReasoningEffort::High),
        other => Err(anyhow!(
            "unsupported reasoning effort '{other}' (expected minimal, low, medium, or high)"
        )),
    }
}

fn print_help() {
    println!(
        "Capture raw MiniMax Anthropic-compatible SSE data frames safely\n\n\
         Options (env fallback in parentheses):\n\
           --base-url URL             provider base URL (MINIMAX_BASE_URL)\n\
           --model MODEL              model id (MINIMAX_MODEL)\n\
           --api-key KEY              API key; never written to output (MINIMAX_API_KEY)\n\
           --auth-header NAME         use NAME: KEY instead of Authorization: Bearer (MINIMAX_AUTH_HEADER)\n\
           --prompt TEXT              prompt (MINIMAX_CAPTURE_PROMPT)\n\
           --reasoning-effort TIER    minimal|low|medium|high; enables Anthropic thinking (MINIMAX_REASONING_EFFORT)\n\
           --max-tokens N             request max_tokens (MINIMAX_MAX_TOKENS; default 4097)\n\
           --header 'Name: value'     extra provider header; secret-like headers are redacted in output\n\
           --output PATH              artifact path (MINIMAX_CAPTURE_OUTPUT)\n\
           --dry-run                  write sanitized request metadata only, no network/API key required\n"
    );
}
