use std::{collections::BTreeSet, env, fs, path::PathBuf, process::ExitCode};

use djinn_qa::{
    CoverageContext, EvidenceSet, Profile, ScenarioInventory, Taxonomy, coverage_report,
    discovered_root, empty_evidence, empty_inventory, required_gap,
};

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("djinn-qa: {error}");
            ExitCode::from(2)
        }
    }
}
fn run(args: Vec<String>) -> Result<ExitCode, String> {
    if args.first().map(String::as_str) != Some("coverage") {
        return Err("usage: djinn-qa coverage --profile smoke-ci --format table|json [--output PATH] [--repo-root PATH]".into());
    }
    let (
        mut profile,
        mut format,
        mut output,
        mut root,
        mut taxonomy,
        mut scenarios,
        mut evidence,
        mut current_sha,
        mut baselines,
    ) = (
        None,
        "table",
        None,
        None,
        None,
        None,
        None,
        None,
        BTreeSet::new(),
    );
    let mut values = args.into_iter().skip(1);
    while let Some(flag) = values.next() {
        let value = values
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--profile" => profile = Some(value),
            "--format" => format = Box::leak(value.into_boxed_str()),
            "--output" => output = Some(PathBuf::from(value)),
            "--repo-root" => root = Some(PathBuf::from(value)),
            "--taxonomy" => taxonomy = Some(PathBuf::from(value)),
            "--scenarios" => scenarios = Some(PathBuf::from(value)),
            "--evidence" => evidence = Some(PathBuf::from(value)),
            "--current-sha" => current_sha = Some(value),
            "--accepted-baseline-sha" => {
                baselines.insert(value);
            }
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    let profile = match profile.as_deref() {
        Some("smoke-ci") => Profile::SmokeCi,
        _ => return Err("--profile smoke-ci is required".into()),
    };
    if !matches!(format, "table" | "json") {
        return Err("--format must be table or json".into());
    }
    let root = match root {
        Some(root) => root,
        None => discovered_root(&env::current_dir().map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?,
    };
    let taxonomy = Taxonomy::load(taxonomy.unwrap_or_else(|| root.join("qa/taxonomy.yaml")))
        .map_err(|error| error.to_string())?;
    let scenario_path = scenarios.unwrap_or_else(|| root.join("qa/scenarios.yaml"));
    let inventory = if scenario_path.is_file() {
        ScenarioInventory::load(scenario_path).map_err(|error| error.to_string())?
    } else {
        empty_inventory()
    };
    inventory
        .validate(&taxonomy, &root)
        .map_err(|error| error.to_string())?;
    let evidence_path = evidence.unwrap_or_else(|| root.join("qa/evidence.yaml"));
    let evidence_set = if evidence_path.is_file() {
        EvidenceSet::load(&evidence_path).map_err(|error| error.to_string())?
    } else {
        empty_evidence()
    };
    evidence_set
        .validate(&taxonomy, &inventory)
        .map_err(|error| error.to_string())?;
    let rows = coverage_report(
        &taxonomy,
        &inventory,
        &evidence_set,
        profile,
        &CoverageContext {
            current_sha: current_sha.unwrap_or_else(|| local_git_sha(&root).unwrap_or_default()),
            accepted_baseline_shas: baselines,
            ..Default::default()
        },
        evidence_path.is_file().then_some(evidence_path.as_path()),
    );
    let rendered = if format == "json" {
        serde_json::to_string_pretty(&rows).map_err(|error| error.to_string())? + "\n"
    } else {
        table(&rows)
    };
    if let Some(path) = output {
        if format != "json" {
            return Err("--output is supported only with --format json".into());
        }
        fs::write(path, rendered).map_err(|error| error.to_string())?;
    } else {
        print!("{rendered}");
    }
    Ok(if required_gap(&rows, profile) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}
fn local_git_sha(root: &std::path::Path) -> Option<String> {
    djinn_git::head_sha(root).ok()
}
fn table(rows: &[djinn_qa::CoverageReportRow]) -> String {
    let mut text = String::from(
        "coverage_id\tsubsystem\trequired_profiles\tstate\tscenario_ids\tevidence_path\tlast_passed_at\tlast_evidence_sha\tstale_reasons\tmemory_sources\n",
    );
    for row in rows {
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            row.coverage_id,
            row.subsystem,
            row.required_profiles.join(","),
            state_name(row.state),
            row.scenario_ids.join(","),
            row.evidence_path.as_deref().unwrap_or(""),
            row.last_passed_at.as_deref().unwrap_or(""),
            row.last_evidence_sha.as_deref().unwrap_or(""),
            row.stale_reasons.join(","),
            row.memory_sources.join(",")
        ));
    }
    text
}

fn state_name(state: djinn_qa::CoverageState) -> &'static str {
    match state {
        djinn_qa::CoverageState::Unproven => "unproven",
        djinn_qa::CoverageState::Proven => "proven",
        djinn_qa::CoverageState::Stale => "stale",
        djinn_qa::CoverageState::Failing => "failing",
    }
}
