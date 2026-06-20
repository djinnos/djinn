pub use djinn_core::events::DjinnEventEnvelope;
pub use djinn_core::events::EventBus;

/// Create an `EventBus` that forwards events into a tokio broadcast channel.
/// The broadcast sender is kept for subscribing (SSE, sync listeners); use
/// this helper when constructing repositories that need an `EventBus`.
pub fn event_bus_for(tx: &tokio::sync::broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
    let tx = tx.clone();
    EventBus::new(move |event| {
        let _ = tx.send(event);
    })
}
