//! The installer-script bundle that rides alongside the generated
//! Dockerfile in the builder Pod.
//!
//! Scripts are authored under `scripts/` and embedded here via
//! `include_str!`. Each script reads its inputs from env vars the
//! Dockerfile `RUN` line sets (e.g. `TOOLCHAINS="stable 1.85.0"`),
//! writes to a well-known path (`/opt/djinn/bin`, `/etc/profile.d/`),
//! and must be idempotent — rebuilds replay the same layer content.
//!
//! Load order is alphabetical via `/etc/profile.d/*.sh`. The worker's
//! PATH fragment is named `00-djinn.sh` to guarantee it sorts before
//! every language's fragment (`10-rust.sh`, `20-node.sh`, ...).

/// One script in the bundle — filename (relative to `scripts/`) + its
/// body. Order is stable and alphabetical by name so [`SCRIPT_BUNDLE_SHA`]
/// is reproducible.
#[derive(Debug, Clone, Copy)]
pub struct ScriptFile {
    pub name: &'static str,
    pub body: &'static str,
}

pub const SCRIPTS: &[ScriptFile] = &[
    ScriptFile {
        name: "base-alpine.sh",
        body: include_str!("../scripts/base-alpine.sh"),
    },
    ScriptFile {
        name: "base-debian.sh",
        body: include_str!("../scripts/base-debian.sh"),
    },
    ScriptFile {
        name: "install-agent-worker.sh",
        body: include_str!("../scripts/install-agent-worker.sh"),
    },
    ScriptFile {
        name: "install-clang.sh",
        body: include_str!("../scripts/install-clang.sh"),
    },
    ScriptFile {
        name: "install-dotnet.sh",
        body: include_str!("../scripts/install-dotnet.sh"),
    },
    ScriptFile {
        name: "install-go.sh",
        body: include_str!("../scripts/install-go.sh"),
    },
    ScriptFile {
        name: "install-java.sh",
        body: include_str!("../scripts/install-java.sh"),
    },
    ScriptFile {
        name: "install-node.sh",
        body: include_str!("../scripts/install-node.sh"),
    },
    ScriptFile {
        name: "install-python.sh",
        body: include_str!("../scripts/install-python.sh"),
    },
    ScriptFile {
        name: "install-ruby.sh",
        body: include_str!("../scripts/install-ruby.sh"),
    },
    ScriptFile {
        name: "install-rust.sh",
        body: include_str!("../scripts/install-rust.sh"),
    },
    ScriptFile {
        name: "install-system.sh",
        body: include_str!("../scripts/install-system.sh"),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_bundle_is_sorted_by_name() {
        let names: Vec<&str> = SCRIPTS.iter().map(|s| s.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "SCRIPTS must be alphabetical by name");
    }

    #[test]
    fn every_script_has_bash_shebang() {
        for s in SCRIPTS {
            assert!(
                s.body.starts_with("#!/usr/bin/env bash")
                    || s.body.starts_with("#!/bin/sh"),
                "{} must declare a POSIX-shell shebang",
                s.name
            );
        }
    }
}
