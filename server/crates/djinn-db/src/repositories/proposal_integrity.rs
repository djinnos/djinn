//! Bounded proposal-head projection and guarded lint-result materialization.

use djinn_core::models::ProposalRevision;
use sqlx::Row;

use crate::database::Database;
use crate::{Error, Result};

/// Largest page the integrity doctor may request in one scan.
pub const MAX_PROPOSAL_INTEGRITY_PAGE_SIZE: i64 = 100;

/// A current material proposal revision suitable for deterministic linting.
#[derive(Clone, Debug)]
pub struct ProposalIntegrityHead {
    pub proposal_id: String,
    pub revision: ProposalRevision,
    pub body_sha256: String,
}

/// Ascending-id cursor page for the proposal integrity sweep.
#[derive(Clone, Debug)]
pub struct ProposalIntegrityHeadPage {
    pub after_proposal_id: Option<String>,
    pub limit: i64,
}

/// Outcome of a guarded cache materialization attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LintMaterializationOutcome {
    Materialized,
    AlreadyPresent,
    Stale,
}

pub struct ProposalIntegrityRepository {
    db: Database,
}

impl ProposalIntegrityRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Return current retained-body heads in a bounded ascending proposal-id page.
    /// Only statuses the doctor is permitted to inspect are included.
    pub async fn list_current_heads(
        &self,
        page: ProposalIntegrityHeadPage,
    ) -> Result<Vec<ProposalIntegrityHead>> {
        if !(1..=MAX_PROPOSAL_INTEGRITY_PAGE_SIZE).contains(&page.limit) {
            return Err(Error::InvalidData(format!(
                "proposal integrity page limit must be in 1..={MAX_PROPOSAL_INTEGRITY_PAGE_SIZE}"
            )));
        }
        self.db.ensure_initialized().await?;
        let rows = sqlx::query(
            r#"SELECT p.id AS proposal_id, pr.id, pr.proposal_id AS revision_proposal_id,
                      pr.seq, pr.title, pr.body, pr.body_format,
                      pr.acceptance_criteria::text AS acceptance_criteria,
                      pr.edited_by_user_id, pr.event_kind, pr.status_from, pr.status_to,
                      pr.event_metadata::text AS event_metadata, pr.created_at
                 FROM proposals p
                 JOIN proposal_revisions pr
                   ON pr.proposal_id = p.id
                  AND pr.seq = p.latest_revision_seq
                  AND pr.event_kind = 'spec_revision'
                WHERE p.status IN ('draft', 'in_review', 'approved', 'building')
                  AND ($1::varchar IS NULL OR p.id > $1)
                ORDER BY p.id ASC
                LIMIT $2"#,
        )
        .bind(page.after_proposal_id.as_deref())
        .bind(page.limit)
        .fetch_all(self.db.pool())
        .await?;
        rows.iter().map(head_from_row).collect()
    }

    /// Persist a lint result only if the snapshotted head is still current.
    /// The result is validated against the stored body while the proposal is
    /// locked, so callers cannot publish a result for supplied or stale text.
    pub async fn materialize_if_current(
        &self,
        snapshot: &ProposalIntegrityHead,
        result: &djinn_spec_lint::SpecLintResultV1,
    ) -> Result<LintMaterializationOutcome> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let current = sqlx::query(
            r#"SELECT p.id AS proposal_id, pr.id, pr.proposal_id AS revision_proposal_id,
                      pr.seq, pr.title, pr.body, pr.body_format,
                      pr.acceptance_criteria::text AS acceptance_criteria,
                      pr.edited_by_user_id, pr.event_kind, pr.status_from, pr.status_to,
                      pr.event_metadata::text AS event_metadata, pr.created_at
                 FROM proposals p
                 JOIN proposal_revisions pr
                   ON pr.proposal_id = p.id
                  AND pr.seq = p.latest_revision_seq
                  AND pr.event_kind = 'spec_revision'
                WHERE p.id = $1
                FOR UPDATE OF p"#,
        )
        .bind(&snapshot.proposal_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(current) = current else {
            tx.rollback().await?;
            return Ok(LintMaterializationOutcome::Stale);
        };
        let current = head_from_row(&current)?;
        if current.revision.id != snapshot.revision.id
            || current.revision.seq != snapshot.revision.seq
            || current.body_sha256 != snapshot.body_sha256
        {
            tx.rollback().await?;
            return Ok(LintMaterializationOutcome::Stale);
        }
        let format = match current.revision.body_format.as_str() {
            "markdown" => djinn_spec_lint::BodyFormat::Markdown,
            "mdx" => djinn_spec_lint::BodyFormat::Mdx,
            other => {
                return Err(Error::InvalidData(format!(
                    "invalid proposal body_format: {other}"
                )));
            }
        };
        if result.linter_version != djinn_spec_lint::SpecLintResultV1::LINTER_VERSION
            || result.body_sha256 != current.body_sha256
            || result.body_format != format
            || result.validate_for_body(&current.revision.body).is_err()
        {
            return Err(Error::InvalidData(
                "lint result does not match stored proposal revision".into(),
            ));
        }
        let inserted = sqlx::query(
            r#"INSERT INTO proposal_revision_lint_results
                 (proposal_id, revision_seq, linter_version, revision_id, body_sha256, result_json)
               VALUES ($1, $2, $3, $4, $5, $6)
               ON CONFLICT (proposal_id, revision_seq, linter_version) DO NOTHING"#,
        )
        .bind(&current.proposal_id)
        .bind(current.revision.seq)
        .bind(&result.linter_version)
        .bind(&current.revision.id)
        .bind(&current.body_sha256)
        .bind(serde_json::to_value(result)?)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        tx.commit().await?;
        Ok(if inserted {
            LintMaterializationOutcome::Materialized
        } else {
            LintMaterializationOutcome::AlreadyPresent
        })
    }
}

fn head_from_row(row: &sqlx::postgres::PgRow) -> Result<ProposalIntegrityHead> {
    let revision = ProposalRevision {
        id: row.try_get("id")?,
        proposal_id: row.try_get("revision_proposal_id")?,
        seq: row.try_get("seq")?,
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        body_format: row.try_get("body_format")?,
        acceptance_criteria: row.try_get("acceptance_criteria")?,
        edited_by_user_id: row.try_get("edited_by_user_id")?,
        event_kind: row.try_get("event_kind")?,
        status_from: row.try_get("status_from")?,
        status_to: row.try_get("status_to")?,
        event_metadata: row.try_get("event_metadata")?,
        created_at: row.try_get("created_at")?,
    };
    Ok(ProposalIntegrityHead {
        proposal_id: row.try_get("proposal_id")?,
        body_sha256: djinn_spec_lint::body_sha256(&revision.body),
        revision,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use crate::repositories::proposal::{
        ProposalCreateInput, ProposalRepository, ProposalUpdateInput,
    };
    use djinn_core::events::EventBus;

    async fn create(proposals: &ProposalRepository, title: &str, status: &str) -> String {
        proposals
            .create(ProposalCreateInput {
                title,
                body: "# Goal\n\nA retained body.",
                acceptance_criteria: Some("[\"works\"]"),
                status: Some(status),
                body_format: None,
            })
            .await
            .unwrap()
            .id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pages_exactly_the_active_statuses_in_ascending_id_order() {
        let db = Database::open_in_memory().unwrap();
        let proposals = ProposalRepository::new(db.clone(), EventBus::noop());
        for (title, status) in [
            ("draft", "draft"),
            ("review", "in_review"),
            ("approved", "approved"),
            ("building", "building"),
            ("triage", "triage"),
            ("done", "done"),
            ("rejected", "rejected"),
            ("archived", "archived"),
            ("superseded", "superseded"),
        ] {
            create(&proposals, title, status).await;
        }
        let repo = ProposalIntegrityRepository::new(db);
        let first = repo
            .list_current_heads(ProposalIntegrityHeadPage {
                after_proposal_id: None,
                limit: 2,
            })
            .await
            .unwrap();
        assert_eq!(first.len(), 2);
        assert!(
            first
                .windows(2)
                .all(|pair| pair[0].proposal_id < pair[1].proposal_id)
        );
        let second = repo
            .list_current_heads(ProposalIntegrityHeadPage {
                after_proposal_id: Some(first[1].proposal_id.clone()),
                limit: 100,
            })
            .await
            .unwrap();
        let ids: Vec<_> = first
            .iter()
            .chain(second.iter())
            .map(|head| head.proposal_id.as_str())
            .collect();
        assert_eq!(ids.len(), 4);
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            repo.list_current_heads(ProposalIntegrityHeadPage {
                after_proposal_id: None,
                limit: 0,
            })
            .await
            .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn materialization_accepts_matching_head_and_rejects_changed_head() {
        let db = Database::open_in_memory().unwrap();
        let proposals = ProposalRepository::new(db.clone(), EventBus::noop());
        let id = create(&proposals, "materialize", "draft").await;
        let repo = ProposalIntegrityRepository::new(db.clone());
        let head = repo
            .list_current_heads(ProposalIntegrityHeadPage {
                after_proposal_id: None,
                limit: 1,
            })
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(head.proposal_id, id);
        sqlx::query("DELETE FROM proposal_revision_lint_results WHERE proposal_id = $1")
            .bind(&id)
            .execute(db.pool())
            .await
            .unwrap();
        let result = proposals.lint_for_revision(&head.revision).await.unwrap();
        assert_eq!(
            repo.materialize_if_current(&head, &result).await.unwrap(),
            LintMaterializationOutcome::Materialized
        );

        let stale = head.clone();
        proposals
            .update(
                &id,
                ProposalUpdateInput {
                    title: "materialize",
                    body: "# Goal\n\nA changed retained body.",
                    acceptance_criteria: "[\"works\"]",
                    status: "draft",
                    superseded_by: None,
                    body_format: None,
                    event_metadata: None,
                },
            )
            .await
            .unwrap();
        let before: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM proposal_revision_lint_results WHERE proposal_id = $1",
        )
        .bind(&id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(
            repo.materialize_if_current(&stale, &result).await.unwrap(),
            LintMaterializationOutcome::Stale
        );
        let after: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM proposal_revision_lint_results WHERE proposal_id = $1",
        )
        .bind(&id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(after, before);
    }
}
