//! Surgical normalization of a cloned project's `.cargo/config.toml` so djinn's
//! warm/verify/worker build pods always get incremental-on + sccache-off.
//!
//! The platform sets `CARGO_INCREMENTAL=1` and `RUSTC_WRAPPER=""` in the pod
//! env, but a project can defeat that from its own `.cargo/config.toml`:
//!
//! * `rustc-wrapper = "sccache"` under `[build]` re-introduces sccache even when
//!   `RUSTC_WRAPPER=""` is set (cargo reads the config wrapper directly).
//! * `CARGO_INCREMENTAL = { value = "0", force = true }` under `[env]` CLAMPS
//!   incremental to 0 regardless of the environment variable — env can't beat
//!   `force = true`.
//! * `[profile.*] incremental = false` disables incremental in the profile.
//!
//! sccache forbids incremental compilation, and toggling incremental on/off
//! between warm and verify/worker invalidates the workspace-crate fingerprints,
//! so the warm seed is wasted. This module rewrites a CLONE's cargo config (we
//! only ever mutate djinn-created ephemeral clones, never the canonical repo) to
//! strip those clamps while preserving every other key (rustflags, target,
//! registries, openssl/system-lib link env, etc.).
//!
//! The rewrite is line-oriented so it preserves the file byte-for-byte except
//! for the specific lines it neutralizes — far safer for "keep all other keys"
//! than a parse/reserialize round-trip, and it adds no new dependencies.

use std::path::Path;

use tracing::{debug, warn};

/// Normalize any `.cargo/config.toml` found at `root` or one level below it
/// (mirroring djinn's cargo-workspace resolution: a repo's cargo root may be the
/// clone root or a subdir like `server/`). Best-effort: every failure is logged
/// and swallowed — this is a cache optimization and must NEVER fail a run.
pub async fn normalize_cargo_config_at(root: &Path) {
    let root = root.to_path_buf();
    let result =
        tokio::task::spawn_blocking(move || normalize_cargo_config_blocking(&root)).await;
    match result {
        Ok(count) => {
            if count > 0 {
                debug!(
                    files_rewritten = count,
                    "normalize_cargo_config: stripped sccache/incremental clamps from clone config"
                );
            }
        }
        Err(e) => warn!(error = %e, "normalize_cargo_config: blocking task panicked (non-fatal)"),
    }
}

/// Synchronous core. Returns the number of config files actually rewritten.
fn normalize_cargo_config_blocking(root: &Path) -> usize {
    let mut candidates = vec![root.join(".cargo").join("config.toml")];
    // One level down (e.g. `<clone>/server/.cargo/config.toml`).
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if dir.is_dir() {
                candidates.push(dir.join(".cargo").join("config.toml"));
            }
        }
    }

    let mut rewritten = 0;
    for path in candidates {
        if !path.is_file() {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(original) => {
                let normalized = normalize_cargo_config_text(&original);
                if normalized != original {
                    if let Err(e) = std::fs::write(&path, &normalized) {
                        warn!(path = %path.display(), error = %e, "normalize_cargo_config: write failed (non-fatal)");
                    } else {
                        rewritten += 1;
                    }
                }
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "normalize_cargo_config: read failed (non-fatal)");
            }
        }
    }
    rewritten
}

/// Pure, line-oriented rewrite of a `.cargo/config.toml` body.
///
/// Tracks the current `[section]` so it only neutralizes keys in the contexts
/// that matter, and preserves every other line verbatim:
///
/// * `[build] rustc-wrapper = ...` → dropped (clears any sccache wrapper).
/// * `[env] CARGO_INCREMENTAL = ...` → dropped (clears any force-clamp; the pod
///   env sets `CARGO_INCREMENTAL=1`).
/// * `[profile.*] incremental = false` → rewritten to `incremental = true`.
///
/// Exposed for unit testing.
pub fn normalize_cargo_config_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut section = String::new();

    for line in input.lines() {
        let trimmed = line.trim();

        // Track the current table header. Only top-level `[..]` / `[[..]]`
        // headers matter for our keys.
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim()
                .to_string();
            out.push_str(line);
            out.push('\n');
            continue;
        }

        let key = trimmed
            .split_once('=')
            .map(|(k, _)| k.trim())
            .unwrap_or("");

        // [build] rustc-wrapper = "sccache" → drop the line entirely so cargo
        // falls back to RUSTC_WRAPPER ("" from the pod env) = no wrapper.
        if section == "build" && key == "rustc-wrapper" {
            continue;
        }

        // [env] CARGO_INCREMENTAL = ... (possibly `{ value = "0", force = true }`)
        // → drop so the pod env's CARGO_INCREMENTAL=1 wins (no force-clamp).
        if section == "env" && key == "CARGO_INCREMENTAL" {
            continue;
        }

        // [profile.<name>] incremental = false → flip to true.
        if section.starts_with("profile.") && key == "incremental" {
            // Preserve leading whitespace + any trailing comment.
            let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
            let comment = line
                .split_once('#')
                .map(|(_, c)| format!(" #{c}"))
                .unwrap_or_default();
            out.push_str(&format!("{indent}incremental = true{comment}"));
            out.push('\n');
            continue;
        }

        out.push_str(line);
        out.push('\n');
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Input config with sccache wrapper + CARGO_INCREMENTAL force-clamp +
    /// profile incremental=false → output has no wrapper, incremental on,
    /// and every other key (rustflags, openssl link env, target, registries)
    /// preserved verbatim.
    #[test]
    fn strips_sccache_and_force_clamp_preserves_other_keys() {
        let input = r#"[build]
rustc-wrapper = "sccache"
rustflags = ["-C", "link-arg=-fuse-ld=lld"]

[env]
OPENSSL_NO_VENDOR = "1"
CARGO_INCREMENTAL = { value = "0", force = true }
DATABASE_URL = "postgres://localhost/db"

[profile.dev]
incremental = false          # required for sccache effectiveness

[profile.test]
incremental = false

[profile.release]
incremental = false

[net]
git-fetch-with-cli = true

[target.x86_64-unknown-linux-gnu]
linker = "clang"
"#;

        let out = normalize_cargo_config_text(input);

        // sccache wrapper gone.
        assert!(
            !out.contains("rustc-wrapper"),
            "rustc-wrapper must be stripped:\n{out}"
        );
        // CARGO_INCREMENTAL force-clamp gone.
        assert!(
            !out.contains("CARGO_INCREMENTAL"),
            "CARGO_INCREMENTAL clamp must be stripped:\n{out}"
        );
        // No remaining `incremental = false` in any profile.
        assert!(
            !out.contains("incremental = false"),
            "all incremental=false must be flipped:\n{out}"
        );
        // incremental is now on for the dev/test/release profiles (3×).
        assert_eq!(
            out.matches("incremental = true").count(),
            3,
            "all three profiles must be incremental = true:\n{out}"
        );

        // Every other key preserved verbatim.
        assert!(out.contains(r#"rustflags = ["-C", "link-arg=-fuse-ld=lld"]"#));
        assert!(out.contains(r#"OPENSSL_NO_VENDOR = "1""#));
        assert!(out.contains(r#"DATABASE_URL = "postgres://localhost/db""#));
        assert!(out.contains("git-fetch-with-cli = true"));
        assert!(out.contains(r#"linker = "clang""#));
        // The dev profile's trailing comment is preserved.
        assert!(
            out.contains("# required for sccache effectiveness"),
            "trailing comment must be preserved:\n{out}"
        );
    }

    /// A config with no sccache and no clamp is left byte-for-byte unchanged
    /// (so the file-rewrite path is a no-op and won't dirty the clone).
    #[test]
    fn clean_config_is_unchanged() {
        let input = r#"[build]
rustflags = ["-D", "warnings"]

[env]
OPENSSL_NO_VENDOR = "1"

[net]
git-fetch-with-cli = true
"#;
        assert_eq!(normalize_cargo_config_text(input), input);
    }

    /// A `CARGO_INCREMENTAL` key OUTSIDE `[env]` (e.g. inadvertently under
    /// `[build]`) is left alone — we only neutralize the env force-clamp.
    #[test]
    fn only_env_cargo_incremental_is_stripped() {
        let input = r#"[build]
CARGO_INCREMENTAL = "weird"

[env]
CARGO_INCREMENTAL = { value = "0", force = true }
"#;
        let out = normalize_cargo_config_text(input);
        // The [build] one survives; the [env] one is gone.
        assert!(out.contains(r#"CARGO_INCREMENTAL = "weird""#));
        assert_eq!(out.matches("CARGO_INCREMENTAL").count(), 1);
    }
}
