//! `djinn-skills-manifest` — generate/check the project's `skills.json` manifest.
//!
//! Usage:
//!
//! ```text
//! djinn-skills-manifest [generate|check] [--out <path>] [--root <project-root>]
//! ```
//!
//! Defaults match the ihl1-roadmap design: `--root .`, `--out
//! .djinn/skills.json`. `generate` (the default) rewrites the checked artifact;
//! `check` regenerates in memory and exits non-zero if the bytes differ.
//!
//! The binary prints status to stderr and exits non-zero on failure. In check
//! mode, the drift error points contributors to `make skills-manifest-generate`.

use std::path::PathBuf;
use std::process::ExitCode;

use djinn_agent::skills_manifest::{
    DEFAULT_MANIFEST_PATH, check_manifest_drift, generate_manifest, to_pretty_json,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Generate,
    Check,
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut out: Option<PathBuf> = None;
    let mut root: Option<PathBuf> = None;
    let mut command = Command::Generate;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "generate" => {
                command = Command::Generate;
            }
            "check" => {
                command = Command::Check;
            }
            "--out" => {
                out = args.next().map(PathBuf::from);
            }
            "--root" => {
                root = args.next().map(PathBuf::from);
            }
            "--help" | "-h" => {
                eprintln!(
                    "djinn-skills-manifest — generate/check .djinn/skills.json\n\n\
                     USAGE:\n    \
                     djinn-skills-manifest [generate|check] [--root <project-root>] [--out <path>]\n\n\
                     LOCAL COMMANDS:\n    \
                     make skills-manifest-check      # fail if checked manifest is stale\n    \
                     make skills-manifest-generate   # update checked manifest\n\n\
                     Defaults: --root .  --out {DEFAULT_MANIFEST_PATH}\n"
                );
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
    }

    let root = root.unwrap_or_else(|| PathBuf::from("."));
    let out = out.unwrap_or_else(|| PathBuf::from(DEFAULT_MANIFEST_PATH));
    let manifest_path = if out.is_absolute() {
        out.clone()
    } else {
        root.join(&out)
    };

    if command == Command::Check {
        match check_manifest_drift(&root, &manifest_path) {
            Ok(()) => {
                eprintln!("skills manifest is up to date: {}", manifest_path.display());
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let manifest = match generate_manifest(&root, None) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "failed to generate manifest under `{}`: {e}",
                root.display()
            );
            return ExitCode::FAILURE;
        }
    };

    let json = match to_pretty_json(&manifest) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("failed to serialize manifest: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Some(parent) = manifest_path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("failed to create parent dir `{}`: {e}", parent.display());
        return ExitCode::FAILURE;
    }

    if let Err(e) = std::fs::write(&manifest_path, &json) {
        eprintln!(
            "failed to write manifest to `{}`: {e}",
            manifest_path.display()
        );
        return ExitCode::FAILURE;
    }

    eprintln!(
        "wrote {} skill(s) to {}",
        manifest.skills.len(),
        manifest_path.display()
    );
    ExitCode::SUCCESS
}
