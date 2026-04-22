//! P5 boot reseed hook.
//!
//! Walks `projects` once at server boot. For every row whose
//! `environment_config` is still the migration-10 default (`'{}'`) or
//! whose parsed config has `schema_version < 1`, builds a fresh
//! [`EnvironmentConfig`] from `projects.stack` via
//! [`EnvironmentConfig::from_stack`], folds the `verification_rules`
//! column into `environment_config.verification.rules` verbatim, and
//! writes it back — nulling `image_hash` so the image-controller
//! rebuilds on the next tick.
//!
//! Idempotence: the sentinel is "empty config". Once a project has a
//! non-empty config (from this reseed *or* a user edit via the MCP
//! tool), the hook skips it forever. No tracking table required.
//!
//! Run-once-at-boot is enough — between boots, new projects get added
//! via the MCP `project_add_from_github` flow, which can seed the
//! column itself (P6).

use std::sync::Arc;

use djinn_db::{Database, ProjectRepository};
use djinn_stack::environment::{EnvironmentConfig, VerificationRule};
use djinn_stack::schema::Stack;

/// Summary returned by [`reseed_empty_configs`], intended for log lines.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReseedStats {
    pub inspected: usize,
    pub reseeded: usize,
    pub skipped_already_seeded: usize,
    pub skipped_stack_missing: usize,
    pub errors: usize,
}

/// Run the reseed pass. Best-effort: per-row errors are counted + logged
/// but don't abort the pass. Returns the aggregate [`ReseedStats`].
pub async fn reseed_empty_configs(db: &Database) -> ReseedStats {
    let repo = ProjectRepository::new(
        db.clone(),
        djinn_core::events::EventBus::noop(),
    );
    let rows = match repo.list_for_reseed().await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(error = %err, "reseed_empty_configs: list_for_reseed failed");
            let mut s = ReseedStats::default();
            s.errors += 1;
            return s;
        }
    };

    let mut stats = ReseedStats::default();
    stats.inspected = rows.len();

    for row in rows {
        if !needs_reseed(&row.environment_config) {
            stats.skipped_already_seeded += 1;
            continue;
        }

        let stack = match parse_stack(&row.stack) {
            Some(s) => s,
            None => {
                tracing::info!(
                    project_id = %row.id,
                    "reseed_empty_configs: stack column empty or unparseable; deferring \
                     — the next mirror-fetch tick will populate stack, then the next \
                     boot's reseed will seed this row"
                );
                stats.skipped_stack_missing += 1;
                continue;
            }
        };

        let rules = parse_verification_rules(&row.verification_rules);

        let mut cfg = EnvironmentConfig::from_stack(&stack);
        cfg.verification.rules = rules;

        if let Err(err) = cfg.validate() {
            tracing::warn!(
                project_id = %row.id,
                error = %err,
                "reseed_empty_configs: generated config failed validation; skipping"
            );
            stats.errors += 1;
            continue;
        }

        let json = match serde_json::to_string(&cfg) {
            Ok(j) => j,
            Err(err) => {
                tracing::warn!(
                    project_id = %row.id,
                    error = %err,
                    "reseed_empty_configs: serialize config failed; skipping"
                );
                stats.errors += 1;
                continue;
            }
        };

        if let Err(err) = repo.set_environment_config(&row.id, &json).await {
            tracing::warn!(
                project_id = %row.id,
                error = %err,
                "reseed_empty_configs: write failed; skipping"
            );
            stats.errors += 1;
            continue;
        }

        tracing::info!(
            project_id = %row.id,
            workspace_count = cfg.workspaces.len(),
            rule_count = cfg.verification.rules.len(),
            "reseed_empty_configs: seeded environment_config from stack"
        );
        stats.reseeded += 1;
    }

    stats
}

/// Convenience — call from server boot with an `Arc<Database>` handle.
pub async fn reseed_empty_configs_arc(db: Arc<Database>) -> ReseedStats {
    reseed_empty_configs(&db).await
}

fn needs_reseed(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return true;
    }
    // Any config that parses with `schema_version = 0` is also "needs
    // reseed" — matches what the controller's enqueue path checks.
    match serde_json::from_str::<EnvironmentConfig>(trimmed) {
        Ok(cfg) => cfg.schema_version == 0,
        Err(_) => false, // unparseable → leave alone; user / MCP tool will fix
    }
}

fn parse_stack(raw: &str) -> Option<Stack> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return None;
    }
    serde_json::from_str::<Stack>(trimmed).ok()
}

fn parse_verification_rules(raw: &str) -> Vec<VerificationRule> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "[]" {
        return Vec::new();
    }
    serde_json::from_str::<Vec<VerificationRule>>(trimmed).unwrap_or_else(|err| {
        tracing::warn!(
            error = %err,
            "reseed_empty_configs: verification_rules JSON unparseable; using empty list"
        );
        Vec::new()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_reseed_when_empty() {
        assert!(needs_reseed(""));
        assert!(needs_reseed("{}"));
        assert!(needs_reseed("  {}  "));
    }

    #[test]
    fn needs_reseed_when_schema_zero() {
        assert!(needs_reseed(
            r#"{"schema_version": 0, "source": "auto-detected"}"#
        ));
    }

    #[test]
    fn does_not_reseed_when_schema_one() {
        assert!(!needs_reseed(
            r#"{"schema_version": 1, "source": "auto-detected"}"#
        ));
    }

    #[test]
    fn parse_stack_tolerates_empty() {
        assert!(parse_stack("{}").is_none());
        assert!(parse_stack("").is_none());
    }

    #[test]
    fn parse_verification_rules_tolerates_empty() {
        assert!(parse_verification_rules("[]").is_empty());
        assert!(parse_verification_rules("").is_empty());
    }

    #[test]
    fn parse_verification_rules_round_trips_existing_shape() {
        let raw = r#"[
            {"match_pattern": "src/**/*.rs", "commands": ["cargo test"]}
        ]"#;
        let rules = parse_verification_rules(raw);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].match_pattern, "src/**/*.rs");
    }
}
