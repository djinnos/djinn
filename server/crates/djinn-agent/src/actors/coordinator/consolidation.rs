use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use djinn_core::models::ConsolidationCluster;
use djinn_db::{
    CreateCanonicalConsolidatedNote, CreateConsolidationRunMetric, Database, DbNoteGroup,
    NoteConsolidationRepository,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const CONSOLIDATION_MIN_CLUSTER_SIZE: usize = 3;
const CONSOLIDATION_TAGS: &str = r#"["canonical","consolidated"]"#;

pub(super) trait ConsolidationRunner: Send + Sync {
    /// Run consolidation for a note group across all notes (unscoped).
    /// Retained for backward compatibility and direct invocations outside the
    /// periodic session-scoped consolidation loop.
    #[allow(dead_code)]
    fn run_for_group<'a>(
        &'a self,
        group: DbNoteGroup,
    ) -> Pin<Box<dyn Future<Output = djinn_db::Result<()>> + Send + 'a>>;

    /// Run consolidation for a note group scoped to a single session.
    /// Only notes linked to `session_id` via `consolidated_note_provenance`
    /// are considered as duplicate candidates.
    fn run_for_group_in_session<'a>(
        &'a self,
        group: DbNoteGroup,
        session_id: String,
    ) -> Pin<Box<dyn Future<Output = djinn_db::Result<()>> + Send + 'a>>;
}

pub(super) struct DbConsolidationRunner {
    db: Database,
}

impl DbConsolidationRunner {
    pub(super) fn new(db: Database) -> Self {
        Self { db }
    }
}

impl ConsolidationRunner for DbConsolidationRunner {
    fn run_for_group<'a>(
        &'a self,
        group: DbNoteGroup,
    ) -> Pin<Box<dyn Future<Output = djinn_db::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let repo = NoteConsolidationRepository::new(self.db.clone());
            let started_at = now_rfc3339();
            let clusters = repo
                .likely_duplicate_clusters(&group.project_id, &group.note_type)
                .await?;
            consolidate_clusters(&repo, &group, &clusters, &started_at).await
        })
    }

    fn run_for_group_in_session<'a>(
        &'a self,
        group: DbNoteGroup,
        session_id: String,
    ) -> Pin<Box<dyn Future<Output = djinn_db::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let repo = NoteConsolidationRepository::new(self.db.clone());
            let started_at = now_rfc3339();
            let clusters = repo
                .likely_duplicate_clusters_for_session(
                    &group.project_id,
                    &group.note_type,
                    &session_id,
                )
                .await?;
            consolidate_clusters(&repo, &group, &clusters, &started_at).await
        })
    }
}

/// Shared consolidation logic: filter qualifying clusters, create canonical
/// notes, and record run metrics.
async fn consolidate_clusters(
    repo: &NoteConsolidationRepository,
    group: &DbNoteGroup,
    clusters: &[ConsolidationCluster],
    started_at: &str,
) -> djinn_db::Result<()> {
    let qualifying_clusters = clusters
        .iter()
        .filter(|cluster| cluster.note_ids.len() >= CONSOLIDATION_MIN_CLUSTER_SIZE)
        .collect::<Vec<_>>();

    if qualifying_clusters.is_empty() {
        return Ok(());
    }

    let mut consolidated_note_count = 0_i64;
    let mut source_note_count = 0_i64;

    for cluster in qualifying_clusters.iter().copied() {
        let source_session_ids = repo
            .resolve_source_session_ids(&group.project_id, &cluster.note_ids)
            .await?;
        let source_session_refs = source_session_ids
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let synthesized = synthesize_cluster(cluster);

        repo.create_canonical_consolidated_note(CreateCanonicalConsolidatedNote {
            project_id: &group.project_id,
            note_type: &group.note_type,
            title: &synthesized.title,
            content: &synthesized.content,
            tags: CONSOLIDATION_TAGS,
            abstract_: synthesized.abstract_.as_deref(),
            overview: synthesized.overview.as_deref(),
            confidence: synthesized.confidence,
            source_session_ids: &source_session_refs,
        })
        .await?;

        consolidated_note_count += 1;
        source_note_count += cluster.note_ids.len() as i64;
    }

    let completed_at = now_rfc3339();
    repo.create_run_metric(CreateConsolidationRunMetric {
        project_id: &group.project_id,
        note_type: &group.note_type,
        status: "completed",
        scanned_note_count: group.note_count,
        candidate_cluster_count: clusters.len() as i64,
        consolidated_cluster_count: qualifying_clusters.len() as i64,
        consolidated_note_count,
        source_note_count,
        started_at,
        completed_at: Some(&completed_at),
        error_message: None,
    })
    .await?;

    Ok(())
}

struct SynthesizedClusterNote {
    title: String,
    content: String,
    abstract_: Option<String>,
    overview: Option<String>,
    confidence: f64,
}

fn synthesize_cluster(cluster: &ConsolidationCluster) -> SynthesizedClusterNote {
    let mut notes = cluster.notes.iter().collect::<Vec<_>>();
    notes.sort_by(|left, right| {
        left.permalink
            .cmp(&right.permalink)
            .then_with(|| left.id.cmp(&right.id))
    });

    let primary = notes[0];
    let title = format!("Canonical {}: {}", primary.note_type, primary.title.trim());
    let abstracts =
        collect_unique_fragments(notes.iter().filter_map(|note| note.abstract_.as_deref()));
    let overviews =
        collect_unique_fragments(notes.iter().filter_map(|note| note.overview.as_deref()));
    let note_titles = notes
        .iter()
        .map(|note| format!("- {} ({})", note.title.trim(), note.permalink))
        .collect::<Vec<_>>()
        .join("\n");
    let note_bodies = notes
        .iter()
        .map(|note| {
            let summary = preferred_summary(note);
            format!(
                "### {}\n{}\n\nSource permalink: {}",
                note.title.trim(),
                summary,
                note.permalink
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let abstract_ = abstracts.first().cloned().or_else(|| {
        overviews
            .first()
            .map(|overview| truncate_for_summary(overview, 280))
    });
    let overview = if abstracts.is_empty() && overviews.is_empty() {
        None
    } else {
        Some(
            abstracts
                .iter()
                .chain(overviews.iter())
                .cloned()
                .collect::<Vec<_>>()
                .join(" "),
        )
    };

    let content = format!(
        "# {}\n\n## Consolidated summary\n{}\n\n## Source notes\n{}\n\n## Synthesized details\n{}",
        title,
        abstract_
            .clone()
            .or_else(|| overview.clone())
            .unwrap_or_else(|| truncate_for_summary(&preferred_summary(primary), 280)),
        note_titles,
        note_bodies
    );

    SynthesizedClusterNote {
        title,
        content,
        abstract_,
        overview,
        confidence: bounded_confidence(notes.len()),
    }
}

fn preferred_summary(note: &djinn_core::models::ConsolidationNote) -> String {
    note.abstract_
        .as_deref()
        .or(note.overview.as_deref())
        .unwrap_or(note.content.as_str())
        .trim()
        .to_string()
}

fn collect_unique_fragments<'a>(fragments: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    fragments
        .map(str::trim)
        .filter(|fragment| !fragment.is_empty())
        .filter(|fragment| seen.insert((*fragment).to_string()))
        .map(ToString::to_string)
        .collect()
}

fn truncate_for_summary(input: &str, max_chars: usize) -> String {
    let trimmed = input.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }

    let truncated = trimmed
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    format!("{}…", truncated.trim_end())
}

fn bounded_confidence(cluster_size: usize) -> f64 {
    (0.5 + 0.05 * cluster_size as f64).min(0.8)
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("rfc3339 timestamp formatting should succeed")
}

pub(super) async fn run_note_consolidation(
    db: &Database,
    consolidation_runner: &Arc<dyn ConsolidationRunner>,
) {
    let repo = NoteConsolidationRepository::new(db.clone());

    // Session-scoped consolidation: discover sessions that have provenance
    // entries and consolidate per-session to avoid merging unrelated
    // cross-session notes (ADR-045 §5).
    let session_ids = match repo.list_sessions_with_provenance().await {
        Ok(ids) => ids,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "CoordinatorActor: failed to list sessions with provenance for consolidation"
            );
            return;
        }
    };

    for session_id in session_ids {
        let groups = match repo.list_db_note_groups_for_session(&session_id).await {
            Ok(groups) => groups,
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    error = %error,
                    "CoordinatorActor: failed to list session note groups for consolidation"
                );
                continue;
            }
        };

        for group in groups {
            if let Err(error) = consolidation_runner
                .run_for_group_in_session(group.clone(), session_id.clone())
                .await
            {
                tracing::warn!(
                    session_id = %session_id,
                    project_id = %group.project_id,
                    note_type = %group.note_type,
                    error = %error,
                    "CoordinatorActor: failed to run session-scoped DB note consolidation"
                );
            }
        }
    }
}
