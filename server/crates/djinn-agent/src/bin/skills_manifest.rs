//! `djinn-skills-manifest` — generate the project's `skills.json` manifest.
//!
//! Usage:
//!
//! ```text
//! djinn-skills-manifest [--out <path>] [--root <project-root>]
//! ```
//!
//! Defaults match the ihl1-roadmap design: `--root .`, `--out
//! .djinn/skills.json`. The binary is intentionally tiny — it is the
//! canonical entry point for both the local developer flow (`cargo run -p
//! djinn-agent --bin djinn-skills-manifest`) and the T2 CI drift check
//! (`verify` subcommand in a follow-up).
//!
//! The binary prints the generated path to stderr and exits non-zero on
//! failure. Stdout is reserved for future piped use (e.g. `--check`).

use std::path::PathBuf;
use std::process::ExitCode;

use djinn_agent::skills_manifest::{generate_manifest, to_pretty_json, DEFAULT_MANIFEST_PATH};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut out: Option<PathBuf> = None;
    let mut root: Option<PathBuf> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => {
                out = args.next().map(PathBuf::from);
            }
            "--root" => {
                root = args.next().map(PathBuf::from);
            }
            "--help" | "-h" => {
                eprintln!(
                    "djinn-skills-manifest — generate .djinn/skills.json\n\n\
                     USAGE:\n    \
                     djinn-skills-manifest [--root <project-root>] [--out <path>]\n\n\
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

    let manifest = match generate_manifest(&root, None) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("failed to generate manifest under `{}`: {e}", root.display());
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

    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("failed to create parent dir `{}`: {e}", parent.display());
            return ExitCode::FAILURE;
        }
    }

    if let Err(e) = std::fs::write(&out, &json) {
        eprintln!("failed to write manifest to `{}`: {e}", out.display());
        return ExitCode::FAILURE;
    }

    eprintln!(
        "wrote {} skill(s) to {}",
        manifest.skills.len(),
        out.display()
    );
    ExitCode::SUCCESS
}
