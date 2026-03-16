use djinn_core::events::DjinnEventEnvelope;

#[derive(Debug, Clone, Copy, Default)]
pub struct EventsRepository;

impl EventsRepository {
    pub fn encode(event: &DjinnEventEnvelope) -> crate::error::DbResult<String> {
        serde_json::to_string(event).map_err(Into::into)
    }
}
