//! Repository-supported offline extraction replay gate.

use std::path::PathBuf;

use djinn_slot::extraction_replay_eval::{
    OfflineReplayThresholds, render_extraction_replay_markdown, run_offline_fixture_replay,
};

fn usage() -> &'static str {
    "usage: extraction-replay [--fixtures DIR] [--output-dir DIR] [--minimum-rubric N] [--minimum-dedup-precision N]"
}

fn argument_value(
    arguments: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<String, String> {
    arguments
        .next()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_arguments() -> Result<(PathBuf, PathBuf, OfflineReplayThresholds), String> {
    let mut fixtures = PathBuf::from("crates/djinn-slot/tests/fixtures/extraction_replay");
    let mut output_dir = PathBuf::from("target/extraction-replay");
    let mut thresholds = OfflineReplayThresholds::default();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--fixtures" => fixtures = PathBuf::from(argument_value(&mut arguments, "--fixtures")?),
            "--output-dir" => {
                output_dir = PathBuf::from(argument_value(&mut arguments, "--output-dir")?)
            }
            "--minimum-rubric" => {
                thresholds.minimum_rubric_satisfaction =
                    argument_value(&mut arguments, "--minimum-rubric")?
                        .parse()
                        .map_err(|_| "--minimum-rubric must be a number".to_string())?
            }
            "--minimum-dedup-precision" => {
                thresholds.minimum_dedup_precision =
                    argument_value(&mut arguments, "--minimum-dedup-precision")?
                        .parse()
                        .map_err(|_| "--minimum-dedup-precision must be a number".to_string())?
            }
            "--help" | "-h" => return Err(usage().to_string()),
            _ => return Err(format!("unknown argument {argument}\n{}", usage())),
        }
    }
    thresholds.validate()?;
    Ok((fixtures, output_dir, thresholds))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (fixtures, output_dir, thresholds) = parse_arguments().map_err(std::io::Error::other)?;
    let report = run_offline_fixture_replay(fixtures)
        .await
        .map_err(std::io::Error::other)?;
    std::fs::create_dir_all(&output_dir)?;
    std::fs::write(
        output_dir.join("report.json"),
        format!("{}\n", serde_json::to_string_pretty(&report)?),
    )?;
    std::fs::write(
        output_dir.join("report.md"),
        render_extraction_replay_markdown(&report, thresholds),
    )?;
    let unmet = thresholds.unmet_dimensions(&report);
    if unmet.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "offline extraction replay thresholds failed: {}",
            unmet.join(", ")
        )
        .into())
    }
}
