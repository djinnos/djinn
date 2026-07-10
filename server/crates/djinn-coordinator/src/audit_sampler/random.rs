//! Deterministic replayable random draws for audit sampling.
//!
//! Implements the `hmac-sha256-counter-v1` algorithm for selecting merged
//! changes from a sealed frame. The algorithm is deterministic: given the
//! same frame content and seed, it always produces the same selections.
//!
//! ## Seed commitment / reveal protocol
//!
//! 1. **Commit**: Before the draw, the coordinator computes
//!    `commitment = SHA-256(seed)` and records it in the database alongside
//!    the frame. This proves the seed was fixed before any selections were
//!    visible.
//!
//! 2. **Reveal**: When performing the draw, the coordinator reveals the
//!    seed. Verifiers can check `SHA-256(seed) == commitment`.
//!
//! 3. **Draw**: For each stratum, the algorithm computes per-position
//!    HMAC scores and selects the top-k items (lowest score wins).
//!
//! ## No LLM or risk-scorer inputs
//!
//! The draw uses only:
//! - The sealed frame's eligible change ids (sorted per stratum)
//! - The revealed seed
//! - The stratum name (as HMAC message prefix)
//! - The position counter
//!
//! No external scoring, risk assessment, or model inference is involved.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::frame::SealedFrame;

/// Named algorithm identifier for the HMAC-SHA256 counter-based draw.
pub const HMAC_SHA256_COUNTER_V1: &str = "hmac-sha256-counter-v1";

type HmacSha256 = Hmac<Sha256>;

// ── Seed commitment ──────────────────────────────────────────────────────────

/// A seed commitment recorded before the draw is revealed.
///
/// Contains the SHA-256 hex of the seed, proving the seed was fixed
/// before any selections were visible.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SeedCommitment {
    /// SHA-256 hex of the seed bytes.
    pub commitment_hash: String,
}

/// Compute a seed commitment from raw seed bytes.
///
/// The commitment is `SHA-256(seed)` encoded as lowercase hex. The
/// coordinator records this before revealing the seed or performing
/// the draw.
pub fn commit_seed(seed: &[u8]) -> SeedCommitment {
    let hash = Sha256::digest(seed);
    SeedCommitment {
        commitment_hash: hex::encode(hash),
    }
}

/// Verify that a revealed seed matches a previously recorded commitment.
///
/// Returns `true` if `SHA-256(revealed_seed) == commitment.commitment_hash`.
pub fn verify_seed_commitment(revealed_seed: &[u8], commitment: &SeedCommitment) -> bool {
    let expected = commit_seed(revealed_seed);
    constant_time_eq(&expected.commitment_hash, &commitment.commitment_hash)
}

/// Constant-time string comparison to avoid timing side-channels.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ── Draw result types ────────────────────────────────────────────────────────

/// Result of a deterministic draw over a sealed frame.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DrawResult {
    /// Algorithm used (always `"hmac-sha256-counter-v1"`).
    pub algorithm: String,
    /// Seed commitment hash (SHA-256 of seed).
    pub seed_commitment: String,
    /// Revealed seed (hex-encoded).
    pub seed_reveal: String,
    /// Selected items across all strata.
    pub selections: Vec<DrawnItem>,
    /// Per-stratum replay data for verification.
    pub replay_data: serde_json::Value,
}

/// A single drawn item within a draw result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DrawnItem {
    /// The merged-change id that was selected.
    pub merged_change_id: String,
    /// Which stratum the change belongs to.
    pub stratum: String,
    /// Zero-based position in the draw order (across all strata).
    pub selected_position: i32,
}

// ── Replay verification ──────────────────────────────────────────────────────

/// Result of replaying a draw to verify selections.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayVerification {
    /// Whether the replay produced identical selections.
    pub valid: bool,
    /// Expected merged-change ids (from stored selection records).
    pub expected_ids: Vec<String>,
    /// Actual merged-change ids (from recomputed draw).
    pub actual_ids: Vec<String>,
    /// Mismatched ids (empty when valid=true).
    pub mismatches: Vec<String>,
}

// ── Core draw algorithm ──────────────────────────────────────────────────────

/// Compute the HMAC-SHA256 score for a given position in a stratum.
///
/// ```text
/// score = HMAC-SHA256(key=seed, message=stratum_name || ":" || counter)
/// ```
///
/// Returns the 32-byte HMAC output. Items are selected by sorting on
/// these scores (lexicographic/numeric ascending) and taking the
/// first `k`.
#[allow(clippy::expect_used)] // HMAC-SHA256 new_from_slice is infallible for any key length
pub fn compute_draw_score(seed: &[u8], stratum_name: &str, counter: u32) -> [u8; 32] {
    // HMAC-SHA256 accepts any key length: `new_from_slice` is infallible
    // in practice (it hashes keys > block_size and zero-pads shorter ones).
    // The `Result` is a type-level artifact from the generic HMAC interface;
    // the `expect` here is documented-safe.
    let mut mac = HmacSha256::new_from_slice(seed).expect("HMAC-SHA256 accepts any key length");
    mac.update(stratum_name.as_bytes());
    mac.update(b":");
    mac.update(&counter.to_be_bytes());
    let result = mac.finalize();
    let mut output = [0u8; 32];
    output.copy_from_slice(&result.into_bytes());
    output
}

/// Perform a deterministic draw over a sealed frame.
///
/// For each stratum in the frame:
/// 1. Compute `k = ceil(eligible_count * rate)` (minimum 1 if rate > 0 and
///    eligible_count > 0).
/// 2. Score each eligible id at its position using
///    [`compute_draw_score`].
/// 3. Sort by score ascending (lowest score wins) and take the first `k`.
///
/// Returns the [`DrawResult`] with all selections and replay data.
pub fn draw_selections(frame: &SealedFrame, seed: &[u8]) -> DrawResult {
    let commitment = commit_seed(seed);
    let mut all_selections: Vec<DrawnItem> = Vec::new();
    let mut replay_entries: Vec<ReplayEntry> = Vec::new();
    let mut global_position: i32 = 0;

    // Process strata in canonical order for deterministic output.
    let mut stratum_names: Vec<&String> = frame.strata.keys().collect();
    stratum_names.sort();

    for stratum_name in &stratum_names {
        if let Some(sf) = frame.strata.get(*stratum_name) {
            if sf.eligible_ids.is_empty() || sf.rate <= 0.0 {
                continue;
            }

            let k = compute_k(sf.eligible_ids.len(), sf.rate);
            if k == 0 {
                continue;
            }

            // Score each position.
            let mut scored: Vec<(u32, [u8; 32], &str)> = sf
                .eligible_ids
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    let counter = i as u32;
                    let score = compute_draw_score(seed, &sf.name, counter);
                    (counter, score, id.as_str())
                })
                .collect();

            // Sort by score ascending (lowest wins).
            scored.sort_by_key(|a| a.1);

            // Take top-k.
            let selected: Vec<_> = scored.into_iter().take(k).collect();

            for (counter, score, id) in &selected {
                all_selections.push(DrawnItem {
                    merged_change_id: id.to_string(),
                    stratum: sf.name.clone(),
                    selected_position: global_position,
                });
                replay_entries.push(ReplayEntry {
                    stratum: sf.name.clone(),
                    counter: *counter,
                    score_hex: hex::encode(score),
                    selected_id: id.to_string(),
                });
                global_position += 1;
            }
        }
    }

    let replay_data = serde_json::to_value(&replay_entries).unwrap_or(serde_json::Value::Null);

    DrawResult {
        algorithm: HMAC_SHA256_COUNTER_V1.to_string(),
        seed_commitment: commitment.commitment_hash,
        seed_reveal: hex::encode(seed),
        selections: all_selections,
        replay_data,
    }
}

/// Compute k (number to select) from eligible count and rate.
///
/// Uses `ceil(count * rate)` but at least 1 when count > 0 and rate > 0.
pub(crate) fn compute_k(count: usize, rate: f64) -> usize {
    if count == 0 || rate <= 0.0 {
        return 0;
    }
    let raw = (count as f64 * rate).ceil() as usize;
    raw.max(1).min(count)
}

/// Verify that a draw is replayable from stored frame content and seed.
///
/// Recomputes the draw from the frame's eligible ids and the revealed
/// seed, then compares the selected ids against the expected selections.
pub fn verify_replay(
    frame: &SealedFrame,
    seed: &[u8],
    expected_selection_ids: &[String],
) -> ReplayVerification {
    let recomputed = draw_selections(frame, seed);
    let actual_ids: Vec<String> = recomputed
        .selections
        .iter()
        .map(|s| s.merged_change_id.clone())
        .collect();

    let expected_sorted = {
        let mut v = expected_selection_ids.to_vec();
        v.sort();
        v
    };
    let actual_sorted = {
        let mut v = actual_ids.clone();
        v.sort();
        v
    };

    let mismatches: Vec<String> = expected_sorted
        .iter()
        .zip(actual_sorted.iter())
        .filter(|(e, a)| e != a)
        .flat_map(|(e, a)| vec![format!("expected: {e}"), format!("actual: {a}")])
        .collect();

    // Also check length mismatch.
    let length_match = expected_sorted.len() == actual_sorted.len();
    let content_match = mismatches.is_empty();

    ReplayVerification {
        valid: length_match && content_match,
        expected_ids: expected_sorted,
        actual_ids: actual_sorted,
        mismatches,
    }
}

// ── Internal replay entry ────────────────────────────────────────────────────

/// Per-position replay data stored alongside selections for audit
/// verification.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReplayEntry {
    stratum: String,
    counter: u32,
    score_hex: String,
    selected_id: String,
}
