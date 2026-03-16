use djinn_core::events::DjinnEventEnvelope;

use crate::Result;

#[derive(Debug, Clone, Copy, Default)]
pub struct EventsRepository;

impl EventsRepository {
    pub fn encode(event: &DjinnEventEnvelope) -> Result<String> {
        serde_json::to_string(event).map_err(Into::into)
    }
}
