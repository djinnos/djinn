//! Note lifecycle event-envelope constructors.
//!
//! These previously lived on `djinn_core::events::DjinnEventEnvelope` as
//! associated functions (`DjinnEventEnvelope::note_created`, etc.). They were
//! moved here when the memory types were extracted out of `djinn-core` so that
//! the core crate does not need to depend on the memory crate.

use djinn_core::events::DjinnEventEnvelope;

use crate::note::{ContradictionCandidate, Note};

/// Envelope emitted when a note is created.
pub fn note_created(note: &Note) -> DjinnEventEnvelope {
    DjinnEventEnvelope {
        entity_type: "note",
        action: "created",
        payload: serde_json::to_value(note).unwrap(),
        id: None,
        project_id: None,
        from_sync: false,
    }
}

/// Envelope emitted when a note is updated.
pub fn note_updated(note: &Note) -> DjinnEventEnvelope {
    DjinnEventEnvelope {
        entity_type: "note",
        action: "updated",
        payload: serde_json::to_value(note).unwrap(),
        id: None,
        project_id: None,
        from_sync: false,
    }
}

/// Envelope emitted when a note is deleted.
pub fn note_deleted(id: &str) -> DjinnEventEnvelope {
    DjinnEventEnvelope {
        entity_type: "note",
        action: "deleted",
        payload: serde_json::to_value(serde_json::json!({ "id": id })).unwrap(),
        id: Some(id.to_string()),
        project_id: None,
        from_sync: false,
    }
}

/// Envelope listing structural-contradiction candidates detected for a note.
pub fn contradiction_candidates(
    note: &Note,
    candidates: &[ContradictionCandidate],
) -> DjinnEventEnvelope {
    DjinnEventEnvelope {
        entity_type: "note",
        action: "contradiction_candidates",
        payload: serde_json::to_value(serde_json::json!({
            "note_id": note.id,
            "project_id": note.project_id,
            "permalink": note.permalink,
            "candidates": candidates,
        }))
        .unwrap(),
        id: Some(note.id.clone()),
        project_id: Some(note.project_id.clone()),
        from_sync: false,
    }
}

/// Envelope emitted when a note lacks an L0 abstract or L1 overview summary.
pub fn note_missing_summary(note: &Note) -> DjinnEventEnvelope {
    DjinnEventEnvelope {
        entity_type: "note",
        action: "missing_summary",
        payload: serde_json::to_value(serde_json::json!({
            "id": note.id,
            "project_id": note.project_id,
            "permalink": note.permalink,
            "title": note.title,
            "note_type": note.note_type,
            "missing_abstract": note.abstract_.is_none(),
            "missing_overview": note.overview.is_none(),
        }))
        .unwrap(),
        id: Some(note.id.clone()),
        project_id: Some(note.project_id.clone()),
        from_sync: false,
    }
}
