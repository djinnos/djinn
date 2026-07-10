//! Immutable sample frame construction and sealing.
//!
//! A [`SealedFrame`] is an immutable snapshot of the eligible merged changes
//! within a time window, partitioned by stratum, with a content hash that
//! enables replay verification. Late corrections create a new revision
//! rather than mutating the existing one.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use djinn_db::{AuditSamplerRepository, MergedChangeRow};

// ── Error types ──────────────────────────────────────────────────────────────

/// Errors that can occur during frame construction.
#[derive(Debug, thiserror::Error)]
pub enum FrameBuilderError {
    /// No sample policy found for the project.
    #[error("no sample policy found for project {project_id}")]
    NoPolicy { project_id: String },

    /// The window is empty (no eligible changes).
    #[error(
        "window {window_start}..{window_end} contains no eligible changes for project {project_id}"
    )]
    EmptyWindow {
        project_id: String,
        window_start: String,
        window_end: String,
    },

    /// Database error during frame construction.
    #[error("database error: {0}")]
    Database(#[from] djinn_db::Error),
}

// ── Policy types ─────────────────────────────────────────────────────────────

/// Parsed sample policy with per-stratum sampling rates.
///
/// Rates are fractions in `[0.0, 1.0]` representing the probability that a
/// given merged change in that stratum is selected for audit.  The
/// autonomous-release stratum uses a higher rate than the unflagged baseline
/// to directly audit autonomous release authority.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SamplePolicy {
    /// Policy revision number (from `audit_sample_policies.revision`).
    pub revision: i32,
    /// Sampling rate for unflagged merged changes (stratum a).
    pub unflagged_rate: f64,
    /// Sampling rate for autonomous-release changes (stratum b).
    /// Typically higher than `unflagged_rate`.
    pub autonomous_rate: f64,
}

impl SamplePolicy {
    /// Parse a policy from a database row.
    pub fn from_row(row: &djinn_db::SamplePolicyRow) -> Result<Self, FrameBuilderError> {
        let pj = &row.policy_json;
        let unflagged_rate = pj
            .get("unflagged_rate")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.02);
        let autonomous_rate = pj
            .get("autonomous_rate")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.10);
        Ok(Self {
            revision: row.revision,
            unflagged_rate,
            autonomous_rate,
        })
    }

    /// Return the sampling rate for a given stratum name.
    pub fn rate_for_stratum(&self, stratum: &str) -> f64 {
        match stratum {
            "autonomous_release" => self.autonomous_rate,
            _ => self.unflagged_rate,
        }
    }
}

// ── Exclusion tracking ───────────────────────────────────────────────────────

/// Reason a merged change was excluded from sampling eligibility.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExclusionReason {
    /// The reason category (e.g. "outside_window", "previously_sampled").
    pub reason: String,
    /// Number of changes excluded for this reason.
    pub count: u64,
}

// ── Sealed frame types ───────────────────────────────────────────────────────

/// Per-stratum data within a sealed frame.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StratumFrame {
    /// Stratum name ("unflagged_merged" or "autonomous_release").
    pub name: String,
    /// Sampling rate for this stratum (from policy).
    pub rate: f64,
    /// Sorted list of eligible merged-change ids in this stratum.
    pub eligible_ids: Vec<String>,
}

/// An immutable sealed sample frame revision.
///
/// Contains all data needed for deterministic draw replay: the frame id,
/// revision, project/window bounds, policy revision, per-stratum eligible
/// ids (sorted for hash stability), exclusion accounting, and a content
/// hash computed over canonical sorted frame content.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SealedFrame {
    /// Unique frame identifier (uuid v7).
    pub frame_id: String,
    /// Revision number within (project, window). Starts at 1; increments
    /// when late corrections alter the frame's eligible set.
    pub revision: i32,
    /// Project id.
    pub project_id: String,
    /// Window start (ISO 8601, inclusive).
    pub window_start: String,
    /// Window end (ISO 8601, exclusive).
    pub window_end: String,
    /// Policy revision used for this frame.
    pub policy_revision: i32,
    /// Per-stratum frame data, keyed by stratum name.
    pub strata: HashMap<String, StratumFrame>,
    /// Exclusion counts keyed by reason category.
    pub exclusion_counts: HashMap<String, u64>,
    /// Deduplicated list of exclusion reasons seen.
    pub exclusion_reasons: Vec<ExclusionReason>,
    /// SHA-256 hex of canonical sorted frame content for integrity
    /// verification.
    pub content_hash: String,
    /// ISO 8601 timestamp when the frame was sealed.
    pub sealed_at: String,
    /// ISO 8601 timestamp when the frame row was created.
    pub created_at: String,
}

impl SealedFrame {
    /// Return all eligible change ids across all strata, in canonical
    /// sorted order (stratum name ascending, then id ascending).
    pub fn all_eligible_ids_sorted(&self) -> Vec<(&str, &str)> {
        let mut keys: Vec<&str> = self.strata.keys().map(|s| s.as_str()).collect();
        keys.sort();
        let mut result = Vec::new();
        for stratum_name in keys {
            if let Some(sf) = self.strata.get(stratum_name) {
                let mut ids: Vec<&str> = sf.eligible_ids.iter().map(|s| s.as_str()).collect();
                ids.sort();
                for id in ids {
                    result.push((stratum_name, id));
                }
            }
        }
        result
    }

    /// Total number of eligible changes across all strata.
    pub fn total_eligible_count(&self) -> usize {
        self.strata.values().map(|s| s.eligible_ids.len()).sum()
    }
}

// ── ADR-020-style event payloads ─────────────────────────────────────────────

/// Event type constant for a frame being sealed.
pub const EVENT_FRAME_SEALED: &str = "audit.frame.sealed";
/// Event type constant for a frame being revised (superseded).
pub const EVENT_FRAME_REVISED: &str = "audit.frame.revised";

/// ADR-020-style payload emitted when a frame is sealed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SealedFrameEventPayload {
    /// Event type (always `"audit.frame.sealed"`).
    pub event_type: String,
    /// Frame id.
    pub frame_id: String,
    /// Revision number.
    pub revision: i32,
    /// Project id.
    pub project_id: String,
    /// Window start.
    pub window_start: String,
    /// Window end.
    pub window_end: String,
    /// Policy revision.
    pub policy_revision: i32,
    /// Content hash (SHA-256 hex).
    pub content_hash: String,
    /// Eligible change counts per stratum.
    pub eligible_counts: HashMap<String, usize>,
    /// Exclusion counts per reason category.
    pub exclusion_counts: HashMap<String, u64>,
    /// ISO 8601 sealed timestamp.
    pub sealed_at: String,
}

impl SealedFrameEventPayload {
    /// Build the event payload from a sealed frame.
    pub fn from_frame(frame: &SealedFrame) -> Self {
        let eligible_counts = frame
            .strata
            .iter()
            .map(|(k, v)| (k.clone(), v.eligible_ids.len()))
            .collect();
        Self {
            event_type: EVENT_FRAME_SEALED.to_string(),
            frame_id: frame.frame_id.clone(),
            revision: frame.revision,
            project_id: frame.project_id.clone(),
            window_start: frame.window_start.clone(),
            window_end: frame.window_end.clone(),
            policy_revision: frame.policy_revision,
            content_hash: frame.content_hash.clone(),
            eligible_counts,
            exclusion_counts: frame.exclusion_counts.clone(),
            sealed_at: frame.sealed_at.clone(),
        }
    }
}

/// ADR-020-style payload emitted when a frame is revised due to late
/// corrections.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RevisedFrameEventPayload {
    /// Event type (always `"audit.frame.revised"`).
    pub event_type: String,
    /// New frame revision id.
    pub new_frame_id: String,
    /// Superseded frame revision id.
    pub superseded_frame_id: String,
    /// New revision number.
    pub new_revision: i32,
    /// Project id.
    pub project_id: String,
    /// Window start.
    pub window_start: String,
    /// Window end.
    pub window_end: String,
    /// Reason for the revision.
    pub reason: String,
    /// ISO 8601 timestamp.
    pub revised_at: String,
}

// ── Content hash ─────────────────────────────────────────────────────────────

/// Compute the canonical content hash for a set of stratum frames.
///
/// The hash is computed over the canonical (sorted) representation of all
/// eligible change ids, grouped by stratum (sorted by name), with each id
/// prefixed by its stratum. This ensures the hash is stable regardless of
/// map iteration order or JSON serialization differences.
pub fn compute_content_hash(strata: &HashMap<String, StratumFrame>) -> String {
    let mut hasher = Sha256::new();

    // Sort stratum names for canonical ordering.
    let mut stratum_names: Vec<&String> = strata.keys().collect();
    stratum_names.sort();

    for stratum_name in &stratum_names {
        if let Some(sf) = strata.get(*stratum_name) {
            // Stratum name as prefix (already sorted by outer loop).
            hasher.update(sf.name.as_bytes());
            hasher.update(b":");
            // Eligible ids must be sorted for canonical hash.
            for id in &sf.eligible_ids {
                hasher.update(id.as_bytes());
                hasher.update(b",");
            }
            // Separator between strata.
            hasher.update(b"|");
        }
    }

    hex::encode(hasher.finalize())
}

// ── Frame builder ────────────────────────────────────────────────────────────

/// Constructs sealed sample frames from persisted facts.
///
/// The builder reads eligible merged changes from the
/// [`AuditSamplerRepository`], partitions them into strata, computes
/// the content hash, and produces an immutable [`SealedFrame`].
pub struct FrameBuilder<'a> {
    repo: &'a AuditSamplerRepository,
}

impl<'a> FrameBuilder<'a> {
    /// Create a new frame builder with the given repository.
    pub fn new(repo: &'a AuditSamplerRepository) -> Self {
        Self { repo }
    }

    /// Build and seal a sample frame for the given project, window, and
    /// policy.
    ///
    /// Returns the sealed frame and the ADR-020 event payload. If a
    /// previous frame exists for this (project, window), the new frame's
    /// revision is incremented and the previous frame is superseded.
    pub async fn build_and_seal(
        &self,
        project_id: &str,
        window_start: &str,
        window_end: &str,
        policy: &SamplePolicy,
        sealed_at: &str,
    ) -> Result<(SealedFrame, SealedFrameEventPayload), FrameBuilderError> {
        // 1. Read eligible merged changes in the window.
        let eligible = self
            .repo
            .list_eligible_changes_in_window(project_id, window_start, window_end)
            .await?;

        if eligible.is_empty() {
            return Err(FrameBuilderError::EmptyWindow {
                project_id: project_id.to_string(),
                window_start: window_start.to_string(),
                window_end: window_end.to_string(),
            });
        }

        // 2. Partition into strata.
        let (strata, exclusion_counts, exclusion_reasons) =
            partition_into_strata(&eligible, policy);

        // 3. Compute content hash.
        let content_hash = compute_content_hash(&strata);

        // 4. Determine revision number.
        let existing_frames = self
            .repo
            .list_sample_frames_in_window(project_id, window_start, window_end)
            .await?;
        let revision = existing_frames
            .iter()
            .map(|f| f.revision)
            .max()
            .unwrap_or(0)
            + 1;

        // 5. Get or create the policy in the DB.
        let policy_row = self
            .repo
            .get_latest_sample_policy(project_id)
            .await?
            .ok_or_else(|| FrameBuilderError::NoPolicy {
                project_id: project_id.to_string(),
            })?;

        // 6. Build eligible_change_ids JSON (sorted flat list across strata).
        let all_eligible: Vec<&str> = strata
            .values()
            .flat_map(|sf| sf.eligible_ids.iter().map(|s| s.as_str()))
            .collect();
        // Sort for canonical representation.
        let mut all_eligible_sorted = all_eligible;
        all_eligible_sorted.sort();
        let eligible_json = serde_json::to_value(&all_eligible_sorted)
            .map_err(|e| FrameBuilderError::Database(djinn_db::Error::from(e)))?;

        // 7. Build exclusion JSON.
        let exc_counts_json = serde_json::to_value(&exclusion_counts)
            .map_err(|e| FrameBuilderError::Database(djinn_db::Error::from(e)))?;
        let exc_reasons_json = serde_json::to_value(
            exclusion_reasons
                .iter()
                .map(|r| &r.reason)
                .collect::<Vec<_>>(),
        )
        .map_err(|e| FrameBuilderError::Database(djinn_db::Error::from(e)))?;

        // 8. Persist the frame.
        let frame_row = self
            .repo
            .create_sample_frame(djinn_db::CreateSampleFrameParams {
                project_id,
                policy_id: &policy_row.id,
                window_start,
                window_end,
                revision,
                eligible_change_ids: &eligible_json,
                content_hash: Some(&content_hash),
                exclusion_counts: &exc_counts_json,
                exclusion_reasons: &exc_reasons_json,
                sealed_at,
            })
            .await?;

        // 9. If there's a previous frame, mark it superseded.
        if let Some(prev) = existing_frames.last()
            && prev.superseded_by_id.is_none()
        {
            let _ = self
                .repo
                .mark_frame_superseded(&prev.id, &frame_row.id)
                .await;
        }

        // 10. Build the sealed frame value object.
        let frame_id = frame_row.id.clone();
        let created_at = frame_row.created_at.clone();

        let sealed = SealedFrame {
            frame_id: frame_id.clone(),
            revision,
            project_id: project_id.to_string(),
            window_start: window_start.to_string(),
            window_end: window_end.to_string(),
            policy_revision: policy.revision,
            strata,
            exclusion_counts,
            exclusion_reasons,
            content_hash,
            sealed_at: sealed_at.to_string(),
            created_at,
        };

        let event = SealedFrameEventPayload::from_frame(&sealed);

        Ok((sealed, event))
    }
}

// ── Stratum partitioning ─────────────────────────────────────────────────────

/// Partition eligible merged changes into strata, applying the policy's
/// per-stratum rates. Returns (strata, exclusion_counts, exclusion_reasons).
fn partition_into_strata(
    eligible: &[MergedChangeRow],
    policy: &SamplePolicy,
) -> (
    HashMap<String, StratumFrame>,
    HashMap<String, u64>,
    Vec<ExclusionReason>,
) {
    let mut strata: HashMap<String, Vec<String>> = HashMap::new();
    let mut exclusion_counts: HashMap<String, u64> = HashMap::new();
    let mut exclusion_reason_set: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for change in eligible {
        if change.excluded {
            let reason = change
                .exclusion_reason
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            *exclusion_counts.entry(reason.clone()).or_insert(0) += 1;
            exclusion_reason_set.insert(reason);
            continue;
        }

        let stratum_name = change
            .stratum_enum()
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|| "unflagged_merged".to_string());

        strata
            .entry(stratum_name)
            .or_default()
            .push(change.id.clone());
    }

    // Sort each stratum's ids for canonical ordering.
    for ids in strata.values_mut() {
        ids.sort();
    }

    // Build StratumFrame objects.
    let stratum_frames: HashMap<String, StratumFrame> = strata
        .into_iter()
        .map(|(name, ids)| {
            let rate = policy.rate_for_stratum(&name);
            let frame = StratumFrame {
                name: name.clone(),
                rate,
                eligible_ids: ids,
            };
            (name, frame)
        })
        .collect();

    let mut exclusion_reasons: Vec<ExclusionReason> = exclusion_reason_set
        .into_iter()
        .map(|reason| {
            let count = exclusion_counts.get(&reason).copied().unwrap_or(0);
            ExclusionReason { reason, count }
        })
        .collect();
    exclusion_reasons.sort_by(|a, b| a.reason.cmp(&b.reason));

    (stratum_frames, exclusion_counts, exclusion_reasons)
}

#[cfg(test)]
mod frame_partition_tests {
    use super::*;
    use djinn_db::MergedChangeRow;

    fn make_change(
        id: &str,
        stratum: &str,
        excluded: bool,
        reason: Option<&str>,
    ) -> MergedChangeRow {
        MergedChangeRow {
            id: id.to_string(),
            project_id: "proj1".to_string(),
            task_id: None,
            pr_number: None,
            head_sha: None,
            merge_commit_sha: format!("sha-{id}"),
            merged_at: "2026-07-01T00:00:00Z".to_string(),
            gate_outcome: "pass".to_string(),
            gate_provenance: None,
            release_provenance: None,
            stratum: stratum.to_string(),
            excluded,
            exclusion_reason: reason.map(|s| s.to_string()),
            created_at: "2026-07-01T00:00:00Z".to_string(),
            updated_at: "2026-07-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn partition_separates_strata_and_sorts_ids() {
        let policy = SamplePolicy {
            revision: 1,
            unflagged_rate: 0.02,
            autonomous_rate: 0.10,
        };
        let changes = vec![
            make_change("c3", "unflagged_merged", false, None),
            make_change("c1", "unflagged_merged", false, None),
            make_change("c2", "autonomous_release", false, None),
        ];

        let (strata, exc_counts, exc_reasons) = partition_into_strata(&changes, &policy);

        let unflagged = strata.get("unflagged_merged").expect("must have unflagged");
        assert_eq!(unflagged.eligible_ids, vec!["c1", "c3"]);
        assert!((unflagged.rate - 0.02).abs() < f64::EPSILON);

        let auto = strata.get("autonomous_release").expect("must have auto");
        assert_eq!(auto.eligible_ids, vec!["c2"]);
        assert!((auto.rate - 0.10).abs() < f64::EPSILON);

        assert!(exc_counts.is_empty());
        assert!(exc_reasons.is_empty());
    }

    #[test]
    fn partition_counts_exclusions() {
        let policy = SamplePolicy {
            revision: 1,
            unflagged_rate: 0.02,
            autonomous_rate: 0.10,
        };
        let changes = vec![
            make_change("c1", "unflagged_merged", false, None),
            make_change("c2", "unflagged_merged", true, Some("outside_window")),
            make_change("c3", "unflagged_merged", true, Some("outside_window")),
            make_change("c4", "unflagged_merged", true, Some("previously_sampled")),
        ];

        let (strata, exc_counts, exc_reasons) = partition_into_strata(&changes, &policy);

        assert_eq!(
            strata.get("unflagged_merged").unwrap().eligible_ids.len(),
            1
        );
        assert_eq!(exc_counts.get("outside_window"), Some(&2));
        assert_eq!(exc_counts.get("previously_sampled"), Some(&1));
        assert_eq!(exc_reasons.len(), 2);
    }

    #[test]
    fn content_hash_is_deterministic() {
        let mut strata = HashMap::new();
        strata.insert(
            "unflagged_merged".to_string(),
            StratumFrame {
                name: "unflagged_merged".to_string(),
                rate: 0.02,
                eligible_ids: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            },
        );
        strata.insert(
            "autonomous_release".to_string(),
            StratumFrame {
                name: "autonomous_release".to_string(),
                rate: 0.10,
                eligible_ids: vec!["d".to_string()],
            },
        );

        let h1 = compute_content_hash(&strata);
        let h2 = compute_content_hash(&strata);
        assert_eq!(h1, h2, "hash must be deterministic");
        assert_eq!(h1.len(), 64, "SHA-256 hex is 64 chars");
    }

    #[test]
    fn content_hash_differs_with_different_ids() {
        let mut strata1 = HashMap::new();
        strata1.insert(
            "unflagged_merged".to_string(),
            StratumFrame {
                name: "unflagged_merged".to_string(),
                rate: 0.02,
                eligible_ids: vec!["a".to_string()],
            },
        );
        let mut strata2 = HashMap::new();
        strata2.insert(
            "unflagged_merged".to_string(),
            StratumFrame {
                name: "unflagged_merged".to_string(),
                rate: 0.02,
                eligible_ids: vec!["b".to_string()],
            },
        );

        assert_ne!(
            compute_content_hash(&strata1),
            compute_content_hash(&strata2)
        );
    }
}
