//! Periodic git-mirror refresh driver.
//!
//! Iterates the projects table every N seconds, mints a fresh per-installation
//! token for each GitHub-linked project, and asks [`MirrorManager`] to
//! ensure + fetch its bare mirror. Single-flight per project is enforced
//! inside `MirrorManager`, so concurrent callers (this watcher + the
//! imperative `POST /api/projects/{id}/mirror/refresh` endpoint + future
//! webhook ingestion) serialize correctly without extra coordination here.
//!
//! Environment:
//!   * `DJINN_MIRROR_FETCH_INTERVAL_SECS` — tick cadence (default 60).
//!
//! Skipped projects: rows without GitHub coords OR without a cached
//! installation id are ignored (legacy / host-path projects). Transient
//! fetch failures are logged and retried on the next tick.
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use djinn_db::ProjectRepository;
use djinn_provider::github_app::installations::get_installation_token;
use serde::Serialize;
use tokio::time::{Interval, MissedTickBehavior};

use crate::server::{AppState, authenticate};
use axum::http::HeaderMap;

const DEFAULT_INTERVAL_SECS: u64 = 60;
const INTERVAL_ENV: &str = "DJINN_MIRROR_FETCH_INTERVAL_SECS";

pub fn spawn(state: AppState) {
    let interval = parse_interval(std::env::var(INTERVAL_ENV).ok().as_deref());
    let cancel = state.cancel().clone();

    tokio::spawn(async move {
        tracing::info!(?interval, "mirror_fetcher watcher starting");
        let mut ticker = make_ticker(interval);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::debug!("mirror_fetcher watcher cancelled");
                    break;
                }
                _ = ticker.tick() => {
                    if let Err(err) = run_tick(&state).await {
                        tracing::warn!(error = %err, "mirror_fetcher tick failed");
                    }
                }
            }
        }
    });
}

async fn run_tick(state: &AppState) -> anyhow::Result<()> {
    let repo = ProjectRepository::new(state.db().clone(), state.event_bus());
    let projects = repo.list().await?;

    for project in projects {
        let Some((owner, repo_name)) = repo.get_github_coords(&project.id).await? else {
            continue;
        };
        let Some(installation_id) = repo.get_installation_id(&project.id).await? else {
            continue;
        };

        match fetch_one(state, &project.id, &owner, &repo_name, installation_id).await {
            Ok(()) => tracing::debug!(project_id = %project.id, "mirror fetched"),
            Err(err) => {
                tracing::warn!(project_id = %project.id, error = %err, "mirror fetch failed")
            }
        }
    }

    Ok(())
}

/// Mint a fresh installation token and refresh the mirror for one project.
/// Separated so the imperative refresh endpoint can call it directly.
pub(crate) async fn fetch_one(
    state: &AppState,
    project_id: &str,
    owner: &str,
    repo: &str,
    installation_id: u64,
) -> anyhow::Result<()> {
    let token = get_installation_token(installation_id).await?;
    let origin_url = format!(
        "https://x-access-token:{}@github.com/{}/{}.git",
        token.token, owner, repo
    );
    let mirror = state.mirror();
    mirror.ensure_mirror(project_id, &origin_url).await?;
    mirror.fetch_mirror(project_id, &origin_url).await?;

    // Stack detection — best-effort, must never break the mirror fetch.
    // Graph-warmer trigger lands in PR 8.
    let mirror_path = mirror.mirror_path(project_id);
    let detected_stack =
        match tokio::task::spawn_blocking(move || djinn_stack::detect_blocking(&mirror_path)).await
        {
            Ok(Ok(stack)) => {
                match serde_json::to_string(&stack) {
                    Ok(json) => {
                        let repo_db =
                            ProjectRepository::new(state.db().clone(), state.event_bus());
                        if let Err(err) = repo_db.set_stack(project_id, &json).await {
                            tracing::warn!(project_id, error = %err, "persist detected stack failed");
                        }
                    }
                    Err(err) => {
                        tracing::warn!(project_id, error = %err, "serialize detected stack failed")
                    }
                }
                Some(stack)
            }
            Ok(Err(err)) => {
                tracing::warn!(project_id, error = %err, "stack detection failed");
                None
            }
            Err(err) => {
                tracing::warn!(project_id, error = %err, "stack detection panicked");
                None
            }
        };

    // Phase 3 PR 5: image-controller enqueue — fire-and-forget, log on error.
    // Absent controller (local dev without a kube::Client) is expected.
    if let (Some(stack), Some(controller)) = (detected_stack, state.image_controller().await) {
        if let Err(err) = controller.enqueue(project_id, &stack).await {
            tracing::warn!(project_id, error = %err, "image-controller enqueue failed");
        }
    }

    // Phase 3 PR 8: fire the canonical-graph warmer. The warmer's own
    // freshness + single-flight guards make a duplicate trigger on an
    // already-hot cache cheap. Log + swallow — warmer failure must never
    // break the mirror fetch tick.
    state.graph_warmer().await.trigger(project_id).await;

    Ok(())
}

fn parse_interval(raw: Option<&str>) -> Duration {
    let secs = raw
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_INTERVAL_SECS);
    Duration::from_secs(secs)
}

fn make_ticker(interval: Duration) -> Interval {
    let mut t = tokio::time::interval(interval);
    t.set_missed_tick_behavior(MissedTickBehavior::Skip);
    t
}

// ─── HTTP: POST /api/projects/:id/mirror/refresh ──────────────────────────
//
// Imperative refresh for a single project. Authenticated like `/admin/*` —
// any signed-in user can trigger, since the deployment is one-org-scoped.

#[derive(Serialize)]
struct RefreshResponse {
    project_id: String,
    status: &'static str,
}

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/api/projects/{id}/mirror/refresh",
        post(refresh_handler),
    )
}

async fn refresh_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(project_id): Path<String>,
) -> Response {
    match authenticate(&state, &headers).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(err) => {
            tracing::error!(error = %err, "mirror refresh: auth lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    let repo = ProjectRepository::new(state.db().clone(), state.event_bus());
    let coords = match repo.get_github_coords(&project_id).await {
        Ok(Some(c)) => c,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::error!(error = %err, project_id = %project_id, "mirror refresh: coord lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let installation_id = match repo.get_installation_id(&project_id).await {
        Ok(Some(id)) => id,
        Ok(None) => return StatusCode::CONFLICT.into_response(),
        Err(err) => {
            tracing::error!(error = %err, project_id = %project_id, "mirror refresh: installation lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    match fetch_one(&state, &project_id, &coords.0, &coords.1, installation_id).await {
        Ok(()) => Json(RefreshResponse {
            project_id,
            status: "refreshed",
        })
        .into_response(),
        Err(err) => {
            tracing::warn!(error = %err, project_id = %project_id, "mirror refresh failed");
            (StatusCode::BAD_GATEWAY, format!("fetch failed: {err}")).into_response()
        }
    }
}
