pub use djinn_core::events::DjinnEventEnvelope;
pub use djinn_core::events::EventBus;

#[cfg(test)]
mod tests {
    use super::DjinnEventEnvelope;
    use crate::verification::StepEvent;
    use serde_json::json;

    #[test]
    fn envelope_verification_step_with_real_step_event() {
        let step = StepEvent::Started {
            index: 1,
            total: 3,
            name: "clippy".into(),
            command: "cargo clippy".into(),
        };
        let envelope = DjinnEventEnvelope::verification_step("p1", Some("t1"), "verification", &step);

        assert_eq!(envelope.entity_type(), "verification");
        assert_eq!(envelope.action(), "step");
        assert_eq!(envelope.project_id.as_deref(), Some("p1"));
        assert_eq!(
            envelope.payload(),
            &json!({
                "project_id": "p1",
                "task_id": "t1",
                "phase": "verification",
                "step": {
                    "Started": {
                        "index": 1,
                        "total": 3,
                        "name": "clippy",
                        "command": "cargo clippy"
                    }
                }
            })
        );
    }
}
