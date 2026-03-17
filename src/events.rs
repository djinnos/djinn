pub use djinn_core::events::DjinnEventEnvelope;
pub use djinn_core::events::EventBus;

/// Create an `EventBus` that forwards events into a tokio broadcast channel.
/// The broadcast sender is kept for subscribing (SSE, sync listeners); use
/// this helper when constructing repositories that need an `EventBus`.
pub fn event_bus_for(tx: &tokio::sync::broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
    let tx = tx.clone();
    EventBus::new(move |event| { let _ = tx.send(event); })
}

#[cfg(test)]
mod tests {
    use super::DjinnEventEnvelope;
    use djinn_agent::verification::StepEvent;
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
