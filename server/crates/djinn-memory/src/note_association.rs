use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;

/// A note association representing implicit co-access relationship between two notes.
/// Used by Hebbian learning to strengthen connections between notes that are
/// frequently accessed together (ADR-023 cognitive memory architecture).
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct NoteAssociation {
    pub note_a_id: String,
    pub note_b_id: String,
    /// Association weight [0.0, 1.0]. Starts at 0.01, grows with co-accesses.
    pub weight: f64,
    /// Number of times these two notes have been co-accessed.
    pub co_access_count: i64,
    /// ISO 8601 timestamp of the most recent co-access.
    pub last_co_access: String,
}

impl NoteAssociation {
    /// Create a new association with default weight and count=1.
    /// Assumes caller has already canonicalized note_a_id < note_b_id.
    pub fn new(note_a_id: String, note_b_id: String) -> Self {
        let now = time::OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| String::from(""));
        Self {
            note_a_id,
            note_b_id,
            weight: 0.01,
            co_access_count: 1,
            last_co_access: now,
        }
    }

    /// Update weight per Hebbian rule: w_new = min(1.0, w_old * (1 + 0.01)^n)
    /// where n is the co-access count for this update session.
    pub fn update_hebbian(&mut self, n_co_accesses: u32) {
        // Increment total co-access count
        self.co_access_count += n_co_accesses as i64;

        // Apply Hebbian weight update: w * (1.01)^n
        let growth_factor = 1.01_f64.powi(n_co_accesses as i32);
        self.weight = (self.weight * growth_factor).min(1.0);

        // Update last co-access timestamp
        self.last_co_access = time::OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| String::from(""));
    }
}

/// Canonical ordering helper: returns (min, max) of two note IDs.
/// Guarantees note_a_id < note_b_id for the association table constraint.
pub fn canonical_pair<'a>(note_id_1: &'a str, note_id_2: &'a str) -> (&'a str, &'a str) {
    if note_id_1 < note_id_2 {
        (note_id_1, note_id_2)
    } else {
        (note_id_2, note_id_1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_pair_orders_correctly() {
        assert_eq!(canonical_pair("a", "b"), ("a", "b"));
        assert_eq!(canonical_pair("b", "a"), ("a", "b"));
        assert_eq!(canonical_pair("same", "same"), ("same", "same"));
    }

    #[test]
    fn new_association_has_defaults() {
        let assoc = NoteAssociation::new("note_1".to_string(), "note_2".to_string());
        assert_eq!(assoc.note_a_id, "note_1");
        assert_eq!(assoc.note_b_id, "note_2");
        assert_eq!(assoc.weight, 0.01);
        assert_eq!(assoc.co_access_count, 1);
        // Timestamp should be parseable RFC3339
        assert!(!assoc.last_co_access.is_empty());
        // Should parse successfully
        let _ = time::OffsetDateTime::parse(&assoc.last_co_access, &Rfc3339).unwrap();
    }

    #[test]
    fn hebbian_update_increments_count() {
        let mut assoc = NoteAssociation::new("a".to_string(), "b".to_string());
        assoc.update_hebbian(5);
        assert_eq!(assoc.co_access_count, 6); // 1 initial + 5
    }

    #[test]
    fn hebbian_update_increases_weight() {
        let mut assoc = NoteAssociation::new("a".to_string(), "b".to_string());
        let initial_weight = assoc.weight;
        assoc.update_hebbian(1);
        assert!(assoc.weight > initial_weight);
        // 0.01 * 1.01 = 0.0101
        assert!((assoc.weight - 0.0101).abs() < 0.0001);
    }

    #[test]
    fn hebbian_weight_caps_at_one() {
        let mut assoc = NoteAssociation::new("a".to_string(), "b".to_string());
        // Set weight very high
        assoc.weight = 0.99;
        assoc.update_hebbian(100);
        assert_eq!(assoc.weight, 1.0);
    }
}
