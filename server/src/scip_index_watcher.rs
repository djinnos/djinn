//! Periodic driver for the standalone SCIP-index Job.
//!
//! Leader-only, and modelled on [`crate::mirror_fetcher`]: iterate the projects
//! table on a tick, skip anything without indexable code, and hand each project
//! to [`ScipIndexScheduler::tick_project`], which decides whether to create a
//! Job and creates at most one.
//!
//! # Why this is a separate loop rather than a hook on the warm path
//!
//! `mirror_fetcher` calls `graph_warmer().trigger()` unconditionally every 60s,
//! which is what leaves the warm debounce permanently pinned at its 900s
//! anti-starvation cap. Hanging a 3-hour cadence off that path would inherit a
//! timer that always fires. This loop pulls its own state instead.
//!
//! # Armed vs disarmed
//!
//! The real scheduler is wired unconditionally. `scip_index_enabled` (default
//! **false**, `DJINN_K8S_SCIP_INDEX_ENABLED`) is checked *inside* the tick, so:
//!
//!   * arming the feature is a config flip, not a redeploy; and
//!   * the production code path is never unreachable — a disarmed deployment
//!     still resolves the real scheduler, and a wiring regression surfaces as
//!     an ERROR log rather than as silence.
//!
//! That second property is deliberate. A composition that silently resolves to
//! no implementation, and therefore does nothing forever while every test stays
//! green, is a failure mode this codebase has already paid for.

use std::sync::Arc;
use std::time::Duration;

use djinn_db::ProjectRepository;
use djinn_k8s::scip_schedule::{
    GitMirrorHeadSource, MirrorHeadSource, ScipIndexDecision, ScipIndexScheduler,
    reconcile_warm_attempts_for_heads,
};
use time::format_description::well_known::Rfc3339;
use tokio::time::{Interval, MissedTickBehavior};

use crate::server::AppState;

/// How often the loop wakes. Deliberately far shorter than the 3-hour cadence
/// floor: the tick is cheap (one apiserver list plus two git calls per project)
/// and the scheduler owns the real rate limiting, so a short tick just means
/// the window is entered promptly once head quiescence and the floor are both
/// satisfied.
const DEFAULT_TICK_SECS: u64 = 300;
const TICK_ENV: &str = "DJINN_SCIP_INDEX_TICK_SECS";

pub fn spawn(state: AppState) {
    let tick = parse_interval(std::env::var(TICK_ENV).ok().as_deref());
    let cancel = state.cancel().clone();

    tokio::spawn(async move {
        tracing::info!(?tick, "scip_index_watcher starting");
        let mut ticker = make_ticker(tick);
        let heads: Arc<dyn MirrorHeadSource> = Arc::new(GitMirrorHeadSource);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::debug!("scip_index_watcher cancelled");
                    break;
                }
                _ = ticker.tick() => {
                    if let Err(err) = run_tick(&state, heads.as_ref()).await {
                        tracing::warn!(error = %err, "scip_index_watcher tick failed");
                    }
                }
            }
        }
    });
}

/// Resolve the real, cluster-backed scheduler from the live warmer.
///
/// Shares the warmer's `kube::Client` rather than building a second one. `None`
/// means this deployment is not running the Kubernetes warmer at all (local /
/// non-K8s wiring), which is a legitimate no-op — but it is reported at ERROR
/// when the feature is armed, because in that case it is a wiring bug and the
/// symptom would otherwise be indistinguishable from "nothing needed doing".
async fn resolve_scheduler(state: &AppState) -> Option<ScipIndexScheduler> {
    state
        .graph_warmer()
        .await
        .as_any()
        .downcast_ref::<djinn_k8s::K8sGraphWarmer>()
        .and_then(djinn_k8s::K8sGraphWarmer::scip_index_scheduler)
}

async fn run_tick(state: &AppState, heads: &dyn MirrorHeadSource) -> anyhow::Result<()> {
    let repo = ProjectRepository::new(state.db().clone(), state.event_bus());
    // Durable lifecycle recovery is independent of the standalone SCIP switch
    // and intentionally does not use Kubernetes Job retention as evidence.
    let mut project_heads = Vec::new();
    for project in repo.list().await? {
        let project_id = project.id.as_str().to_string();
        if let Some(head) = heads.head(&project_id).await {
            project_heads.push((project_id, head));
        }
    }
    let recovery_heads = project_heads
        .iter()
        .map(|(project_id, head)| (project_id.clone(), head.revision.clone()))
        .collect::<Vec<_>>();
    let now = time::OffsetDateTime::now_utc().format(&Rfc3339)?;
    let timed_out = reconcile_warm_attempts_for_heads(
        state.db().clone(),
        &recovery_heads,
        &now,
        Duration::from_secs(DEFAULT_TICK_SECS),
    )
    .await?;
    if !timed_out.is_empty() {
        tracing::info!(
            count = timed_out.len(),
            "scip_index_watcher: timed out stale warm attempts"
        );
    }

    let Some(scheduler) = resolve_scheduler(state).await else {
        // Read the flag straight from the environment here: without a
        // scheduler there is no config to read it from, and staying silent
        // when an operator has armed the feature is the exact failure this
        // branch exists to make loud.
        if std::env::var("DJINN_K8S_SCIP_INDEX_ENABLED").is_ok_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        }) {
            tracing::error!(
                "scip_index_watcher: SCIP indexing is ARMED but no Kubernetes graph warmer \
                 is wired, so no SCIP Job can ever be created. This is a composition bug, \
                 not a quiet no-op."
            );
        } else {
            tracing::debug!("scip_index_watcher: no Kubernetes warmer; nothing to do");
        }
        return Ok(());
    };

    if !scheduler.enabled() {
        tracing::debug!(
            "scip_index_watcher: disarmed (DJINN_K8S_SCIP_INDEX_ENABLED); \
             standalone dispatch disabled; durable warm attempts reconciled"
        );
        return Ok(());
    }

    for (project_id, head) in project_heads {
        let project_id = project_id.as_str();

        // Same filter mirror_fetcher applies before triggering a warm: the
        // canonical graph is a CODE graph, so a docs-only repo indexes to zero
        // nodes and a SCIP Job for it is pure cost.
        if !djinn_agent::environment::project_has_indexable_code(state.db(), project_id).await {
            continue;
        }

        // The image the Job runs in, resolved with the same catalog-image
        // precedence the warm path uses. Not ready yet means the devcontainer
        // has not been built; skip and retry next tick.
        let Some(image_tag) = repo
            .resolve_dispatch_image(project_id)
            .await
            .ok()
            .flatten()
            .and_then(|image| image.pull_ref())
        else {
            tracing::debug!(project_id, "scip_index_watcher: no ready project image");
            continue;
        };

        // The declared JS workspace roots the SCIP Pod must install before
        // indexing. scip-typescript resolves `tsconfig` `extends` through
        // `node_modules`, so a monorepo whose JS lives below the repo root
        // indexes to nothing without this: the Pod used to probe for a lockfile
        // at the root only, which silently skipped those workspaces entirely.
        let js_workspace_roots = match repo.get_environment_config(project_id).await {
            Ok(Some(raw)) => {
                serde_json::from_str::<djinn_stack::environment::EnvironmentConfig>(&raw)
                    .ok()
                    .map(|cfg| djinn_k8s::js_install::js_workspace_roots(Some(&cfg)))
                    .unwrap_or_default()
            }
            _ => Vec::new(),
        };

        // `policy` is `None` deliberately: `warm_cache_env_vars` ignores the
        // cargo-cache policy (incremental-on is an invariant across all djinn
        // build pods), so passing it would imply an influence it does not have.
        let decision = scheduler
            .tick_project(
                project_id,
                Some(&head),
                &image_tag,
                None,
                &js_workspace_roots,
            )
            .await;
        if let ScipIndexDecision::Dispatch { revision } = &decision {
            tracing::info!(
                project_id,
                revision,
                "scip_index_watcher: dispatched standalone semantic index"
            );
        } else {
            tracing::debug!(
                project_id,
                reason = decision.reason(),
                "scip_index_watcher: no Job this tick"
            );
        }
    }
    Ok(())
}

fn parse_interval(raw: Option<&str>) -> Duration {
    let secs = raw
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_TICK_SECS);
    Duration::from_secs(secs)
}

fn make_ticker(interval: Duration) -> Interval {
    let mut t = tokio::time::interval(interval);
    t.set_missed_tick_behavior(MissedTickBehavior::Skip);
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_interval_parses_and_falls_back() {
        assert_eq!(parse_interval(Some("42")), Duration::from_secs(42));
        assert_eq!(
            parse_interval(Some("0")),
            Duration::from_secs(DEFAULT_TICK_SECS)
        );
        assert_eq!(
            parse_interval(Some("nonsense")),
            Duration::from_secs(DEFAULT_TICK_SECS)
        );
        assert_eq!(parse_interval(None), Duration::from_secs(DEFAULT_TICK_SECS));
    }

    /// **The warm path's claim wait has to fit inside the warm Job's own
    /// deadline.**
    ///
    /// When a SCIP Job is already indexing this exact tree, the warm path waits
    /// for it rather than starting a duplicate ~10 GB index. That wait can be a
    /// total loss — the holder may run past the budget — in which case the warm
    /// still has to do every phase itself. So the budget plus one whole
    /// measured warm must fit in `activeDeadlineSeconds`, or the mitigation for
    /// a wasteful warm becomes a warm that publishes no graph at all.
    ///
    /// This lives here because it is the only crate that can see both numbers:
    /// the budget is `djinn-graph`'s, the deadline is `djinn-k8s`'s.
    #[test]
    fn the_claim_wait_budget_fits_inside_the_warm_job_deadline() {
        let cfg = djinn_k8s::KubernetesConfig::for_testing();
        let wait = djinn_graph::semantic_index_claim::DEFAULT_WAIT_SECONDS as i64;
        let worst_case = wait + djinn_k8s::config::MEASURED_FULL_WARM_SECONDS;
        assert!(
            worst_case <= cfg.warm_job_timeout_seconds,
            "a warm that waits its full {wait}s claim budget, gains nothing and \
             then runs the whole {}s warm itself needs {worst_case}s, but \
             activeDeadlineSeconds is {}s — the Pod would be SIGKILLed before \
             publishing",
            djinn_k8s::config::MEASURED_FULL_WARM_SECONDS,
            cfg.warm_job_timeout_seconds,
        );
        // …and the budget is not zero, which would silently disable the wait
        // and leave the duplicate index in place.
        assert!(wait > 0);
    }

    /// **The rendered key must be the key the reader reads, and the two
    /// defaults must agree.**
    ///
    /// The warm Pod's environment is exactly what `build_warm_job` puts in it,
    /// so a claim-wait rendered under any other name is an operator lever that
    /// silently does nothing — the failure mode this repository has already
    /// paid for with `DJINN_SCIP_JOB_DEADLINE_SECONDS`. Only this crate can see
    /// both the writer (`djinn-k8s`) and the reader (`djinn-graph`), so the
    /// contract is asserted here instead of being duplicated as a literal.
    #[test]
    fn the_warm_job_renders_the_claim_wait_under_the_key_the_worker_reads() {
        assert_eq!(
            djinn_k8s::warm_job::WARM_SCIP_CLAIM_WAIT_ENV,
            djinn_graph::semantic_index_claim::WAIT_ENV,
            "the warm Job renders one key and the in-Pod claim reads another; \
             the wait would be unconfigurable and the default silently permanent"
        );
        assert_eq!(
            djinn_k8s::KubernetesConfig::for_testing().scip_claim_wait_seconds,
            djinn_graph::semantic_index_claim::DEFAULT_WAIT_SECONDS,
            "the rendered default and the compiled-in default must agree, or a \
             warm Pod and an in-process warm wait for different lengths of time"
        );
    }

    /// The loop tick must be shorter than the cadence floor, or the effective
    /// cadence becomes the tick rather than the configured interval.
    #[test]
    fn tick_is_shorter_than_the_cadence_floor() {
        let cfg = djinn_k8s::KubernetesConfig::for_testing();
        assert!(
            DEFAULT_TICK_SECS < cfg.scip_index_interval_seconds,
            "a {DEFAULT_TICK_SECS}s tick must be shorter than the \
             {}s cadence floor, or the floor is not what governs the rate",
            cfg.scip_index_interval_seconds,
        );
    }
}
