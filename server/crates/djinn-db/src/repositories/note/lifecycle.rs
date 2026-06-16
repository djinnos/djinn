//! Note lifecycle decay actions for the housekeeping tick.
//!
//! Implements [`NoteRepository::decay_stale_extracted_notes`], a per-project
//! decay pass that lowers the confidence of stale *extracted* note types
//! (`case`, `pattern`, `pitfall`) toward the injection-ineligibility threshold
//! ([`super::scoring::STALE_CITATION`]) using the existing Bayesian confidence
//! update path ([`super::scoring::bayesian_update`] +
//! [`NoteRepository::set_confidence`]).
//!
//! # Safety boundary
//!
//! Only the three extracted note types are ever touched. The SQL `note_type`
//! predicate is the hard safety boundary: hand-written types (`adr`,
//! `reference`, `design`, `requirement`, `research`, `brief`, `roadmap`,
//! `catalog`, …) are excluded by the query and therefore can never be decayed.
//!
//! # Status filter
//!
//! Only `status = 'active'` notes are decayed. A note that a human has moved
//! to `archived` or `deprecated` is left alone — decay is a sweep over the live
//! corpus, not a force-archive.
//!
//! # Iteration cap
//!
//! Each candidate is decayed at most [`DECAY_ITERATION_CAP`] times per tick so
//! a single pathological note cannot starve the housekeeping tick. A note that
//! has not yet crossed below [`STALE_CITATION`] after the cap will be picked up
//! again on the next tick.

use sqlx::Row;

use crate::error::DbResult as Result;

use super::scoring::{CONFIDENCE_FLOOR, STALE_CITATION, STALE_DECAY_SIGNAL, bayesian_update};
use super::NoteRepository;

/// Env var overriding the global decay window (days). Default 30.
pub(crate) const DECAY_WINDOW_ENV: &str = "DJINN_LIFECYCLE_DECAY_WINDOW_DAYS";
/// Default decay window applied when the env override is absent or invalid.
pub(crate) const DEFAULT_DECAY_WINDOW_DAYS: u32 = 30;

/// Per-note-type decay window overrides (days). These are advisory defaults;
/// the global env override ([`DECAY_WINDOW_ENV`]) is used by the SQL predicate
/// today. The constants exist so future per-type windows can be wired without
/// hunting for the magic numbers, and so tests can assert on them.
#[allow(dead_code)]
pub(crate) const CASE_DECAY_WINDOW_DAYS: u32 = 30;
#[allow(dead_code)]
pub(crate) const PATTERN_DECAY_WINDOW_DAYS: u32 = 60;
#[allow(dead_code)]
pub(crate) const PITFALL_DECAY_WINDOW_DAYS: u32 = 45;

/// Maximum Bayesian decay iterations applied to a single note within one tick.
/// Bounds tick latency so a single stuck note cannot starve the housekeeping
/// pass. A note that needs more iterations is re-visited on the next tick.
pub(crate) const DECAY_ITERATION_CAP: u8 = 5;

/// Candidate row selected for decay.
struct DecayCandidate {
    id: String,
    confidence: f64,
}

impl NoteRepository {
    /// Decay stale extracted notes (`case`, `pattern`, `pitfall`) for a single
    /// project.
    ///
    /// Selects notes that are (a) one of the three extracted types, (b) still
    /// injection-eligible (`confidence > STALE_CITATION`), (c) older than the
    /// decay window by `last_accessed` (or never accessed), and (d) in the
    /// `active` lifecycle status. For each candidate, applies up to
    /// [`DECAY_ITERATION_CAP`] Bayesian decay steps via
    /// [`set_confidence`](Self::set_confidence) until the posterior drops to or
    /// below [`STALE_CITATION`] or the [`CONFIDENCE_FLOOR`] is reached.
    ///
    /// Returns the count of notes whose confidence crossed below
    /// [`STALE_CITATION`] during this tick. Hand-written note types are never
    /// selected and therefore never decayed — the SQL `note_type IN (...)`
    /// predicate is the safety boundary.
    pub async fn decay_stale_extracted_notes(
        &self,
        project_id: &str,
        decay_window_days: u32,
    ) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let window = parse_decay_window_days(decay_window_days);

        // Select decay candidates. Runtime `sqlx::query` (not the
        // compile-checked `query!` macro) is used because the
        // `($3 || ' days')::interval` construction is dynamic.
        //
        // Safety boundary: `note_type IN ('case','pattern','pitfall')` is the
        // hard predicate that guarantees hand-written types are never touched.
        // The `status = 'active'` predicate guarantees a human-archived or
        // deprecated note is never decayed.
        let rows = sqlx::query(
            r#"SELECT id, confidence
               FROM notes
               WHERE project_id = $1
                 AND note_type IN ('case', 'pattern', 'pitfall')
                 AND confidence > $2
                 AND status = 'active'
                 AND (
                     last_accessed IS NULL
                     OR last_accessed = ''
                     OR last_accessed < to_char(
                         (now() at time zone 'utc') - ($3 || ' days')::interval,
                         'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'
                     )
                 )
               ORDER BY last_accessed ASC NULLS FIRST"#,
        )
        .bind(project_id)
        .bind(STALE_CITATION)
        .bind(window.to_string())
        .fetch_all(self.db.pool())
        .await?;

        let mut candidates: Vec<DecayCandidate> = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.try_get("id")?;
            let confidence: f64 = row.try_get("confidence")?;
            candidates.push(DecayCandidate { id, confidence });
        }

        let mut decayed_count: u64 = 0;
        for candidate in &candidates {
            let crossed = self
                .decay_note_confidence(&candidate.id, candidate.confidence)
                .await?;
            if crossed {
                decayed_count += 1;
            }
        }

        tracing::debug!(
            project_id,
            candidate_count = candidates.len(),
            decayed_count,
            decay_window_days = window,
            "decay_stale_extracted_notes tick"
        );

        Ok(decayed_count)
    }

    /// Apply up to [`DECAY_ITERATION_CAP`] Bayesian decay steps to a single
    /// note. The posterior is recomputed in-memory from the prior passed in;
    /// each step writes the new confidence via
    /// [`set_confidence`](Self::set_confidence) so the row is durable if the
    /// tick is interrupted. Returns `true` if the note's confidence crossed
    /// below [`STALE_CITATION`] during this tick.
    ///
    /// The decay signal is [`STALE_DECAY_SIGNAL`] (0.15), chosen between
    /// `CONTRADICTION` (0.1) and `STALE_CITATION` (0.3) so a single step moves
    /// the posterior toward the floor without collapsing it in one shot.
    async fn decay_note_confidence(&self, note_id: &str, starting_confidence: f64) -> Result<bool> {
        let mut confidence = starting_confidence;

        for _ in 0..DECAY_ITERATION_CAP {
            // Already at or below the injection-ineligibility threshold — no
            // further decay is useful this tick.
            if confidence <= STALE_CITATION {
                return Ok(true);
            }
            // Already floored — `bayesian_update` clamps to the floor, so
            // further iterations are a no-op.
            if (confidence - CONFIDENCE_FLOOR).abs() < f64::EPSILON {
                return Ok(false);
            }

            confidence = bayesian_update(confidence, STALE_DECAY_SIGNAL);
            self.set_confidence(note_id, confidence).await?;

            if confidence <= STALE_CITATION {
                return Ok(true);
            }
        }

        Ok(confidence <= STALE_CITATION)
    }
}

/// Resolve the effective decay window. The env override
/// ([`DECAY_WINDOW_ENV`]) wins when set to a positive value; otherwise the
/// caller-provided `decay_window_days` (itself usually the env-derived default
/// read by the housekeeping tick) is used.
pub(crate) fn parse_decay_window_days(caller_default: u32) -> u32 {
    if let Ok(raw) = std::env::var(DECAY_WINDOW_ENV) {
        if let Ok(parsed) = raw.parse::<u32>() {
            if parsed > 0 {
                return parsed;
            }
        }
    }
    if caller_default > 0 {
        caller_default
    } else {
        DEFAULT_DECAY_WINDOW_DAYS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decay_constants_are_in_expected_ranges() {
        // STALE_DECAY_SIGNAL must sit between CONTRADICTION and STALE_CITATION.
        const { assert!(STALE_DECAY_SIGNAL > super::super::scoring::CONTRADICTION) };
        const { assert!(STALE_DECAY_SIGNAL < STALE_CITATION) };

        // Per-type windows are distinct and positive.
        const { assert!(CASE_DECAY_WINDOW_DAYS > 0) };
        const { assert!(PATTERN_DECAY_WINDOW_DAYS > 0) };
        const { assert!(PITFALL_DECAY_WINDOW_DAYS > 0) };

        // Iteration cap is small enough to bound tick latency.
        const { assert!(DECAY_ITERATION_CAP <= 8) };
    }
}
