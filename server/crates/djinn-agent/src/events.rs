use djinn_core::events::{DjinnEventEnvelope, EventBus};

/// Create an `EventBus` that forwards events into a tokio broadcast channel.
#[allow(dead_code)]
pub fn event_bus_for(tx: &tokio::sync::broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
    let tx = tx.clone();
    EventBus::new(move |event| {
        let _ = tx.send(event);
    })
}
