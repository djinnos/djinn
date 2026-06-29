use std::collections::HashMap;
use std::convert::Infallible;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::events::DjinnEventEnvelope;
use crate::server::AppState;

const FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const SESSION_MESSAGE_MIN_INTERVAL: Duration = Duration::from_millis(50);
const SESSION_TOKEN_UPDATE_MIN_INTERVAL: Duration = Duration::from_millis(500);
// Browser-visible liveness ping interval. The axum KeepAlive comment frames
// emitted by Sse::keep_alive are invisible to EventSource listeners, so we
// also emit a named ping event on this cadence. Pings never carry an id,
// so Last-Event-ID replay continues to track meaningful domain events.
const PING_INTERVAL: Duration = Duration::from_secs(25);

enum EventTier {
    Immediate,
    Coalesced {
        key: String,
    },
    Throttled {
        key: &'static str,
        min_interval: Duration,
    },
}

struct BatchAccumulator {
    coalesced: HashMap<String, DjinnEventEnvelope>,
    throttled_pending: HashMap<&'static str, DjinnEventEnvelope>,
    throttled_last_sent: HashMap<&'static str, Instant>,
}

impl BatchAccumulator {
    fn new() -> Self {
        Self {
            coalesced: HashMap::new(),
            throttled_pending: HashMap::new(),
            throttled_last_sent: HashMap::new(),
        }
    }

    fn classify(envelope: &DjinnEventEnvelope) -> EventTier {
        match (envelope.entity_type(), envelope.action()) {
            ("task", "created" | "deleted")
            | ("epic", "created" | "deleted")
            | ("proposal", "created" | "deleted")
            | ("proposal_feedback", "created")
            | ("session", "dispatched" | "ended")
            | ("lifecycle", "step")
            | ("dispatch_pause", "changed") => EventTier::Immediate,
            ("task", "updated")
            | ("epic", "updated")
            | ("proposal", "updated")
            | ("agent", "updated")
            | ("project", "updated") => EventTier::Coalesced {
                key: coalesce_key(envelope),
            },
            ("session", "message") => EventTier::Throttled {
                key: "session.message",
                min_interval: SESSION_MESSAGE_MIN_INTERVAL,
            },
            ("session", "token_update") => EventTier::Throttled {
                key: "session.token_update",
                min_interval: SESSION_TOKEN_UPDATE_MIN_INTERVAL,
            },
            _ => EventTier::Immediate,
        }
    }

    #[allow(clippy::disallowed_methods)] // scoped: direct wall-clock read; migration tracked by lint-ratchet task 70y0 (Clock abstraction already lands in 8bcj/m5g4)
    fn push(&mut self, envelope: DjinnEventEnvelope) -> Vec<DjinnEventEnvelope> {
        match Self::classify(&envelope) {
            EventTier::Immediate => vec![envelope],
            EventTier::Coalesced { key } => {
                self.coalesced.insert(key, envelope);
                Vec::new()
            }
            EventTier::Throttled { key, min_interval } => {
                let should_send_now = self
                    .throttled_last_sent
                    .get(key)
                    .map(|last| last.elapsed() >= min_interval)
                    .unwrap_or(true);
                if should_send_now {
                    self.throttled_last_sent.insert(key, Instant::now());
                    vec![envelope]
                } else {
                    self.throttled_pending.insert(key, envelope);
                    Vec::new()
                }
            }
        }
    }

    #[allow(clippy::disallowed_methods)] // scoped: direct wall-clock read; migration tracked by lint-ratchet task 70y0 (Clock abstraction already lands in 8bcj/m5g4)
    fn flush(&mut self) -> Vec<DjinnEventEnvelope> {
        let mut out = Vec::new();

        let mut coalesced = self.coalesced.drain().collect::<Vec<_>>();
        coalesced.sort_by(|a, b| a.0.cmp(&b.0));
        out.extend(coalesced.into_iter().map(|(_, envelope)| envelope));

        let pending_keys = self.throttled_pending.keys().copied().collect::<Vec<_>>();
        for key in pending_keys {
            let Some(envelope) = self.throttled_pending.get(key) else {
                continue;
            };
            let min_interval = match key {
                "session.message" => SESSION_MESSAGE_MIN_INTERVAL,
                "session.token_update" => SESSION_TOKEN_UPDATE_MIN_INTERVAL,
                _ => continue,
            };
            let allowed = self
                .throttled_last_sent
                .get(key)
                .map(|last| last.elapsed() >= min_interval)
                .unwrap_or(true);
            if allowed {
                let envelope = self
                    .throttled_pending
                    .remove(key)
                    .expect("pending throttled event exists");
                self.throttled_last_sent.insert(key, Instant::now());
                out.push(envelope);
            } else {
                let _ = envelope;
            }
        }

        out
    }
}

fn coalesce_key(envelope: &DjinnEventEnvelope) -> String {
    let event_name = format!("{}.{}", envelope.entity_type(), envelope.action());
    let payload = envelope.payload();
    match (envelope.entity_type(), envelope.action()) {
        ("task", "updated") => payload
            .get("task")
            .and_then(|task| task.get("id"))
            .and_then(|id| id.as_str())
            .map(|id| format!("{event_name}:{id}"))
            .unwrap_or(event_name),
        ("epic", "updated") => payload
            .get("id")
            .and_then(|id| id.as_str())
            .map(|id| format!("{event_name}:{id}"))
            .unwrap_or(event_name),
        ("proposal", "updated") => payload
            .get("id")
            .and_then(|id| id.as_str())
            .map(|id| format!("{event_name}:{id}"))
            .unwrap_or(event_name),
        ("agent", "updated") => payload
            .get("id")
            .and_then(|id| id.as_str())
            .map(|id| format!("{event_name}:{id}"))
            .unwrap_or(event_name),
        ("project", "updated") => payload
            .get("id")
            .and_then(|id| id.as_str())
            .or_else(|| payload.get("project_id").and_then(|id| id.as_str()))
            .map(|id| format!("{event_name}:{id}"))
            .unwrap_or(event_name),
        _ => event_name,
    }
}

fn event_name(envelope: &DjinnEventEnvelope) -> String {
    format!("{}.{}", envelope.entity_type, envelope.action)
}

fn serialize_event(envelope: &DjinnEventEnvelope) -> Result<Event, Infallible> {
    let event_name = event_name(envelope);
    if envelope.entity_type == "session" {
        tracing::debug!(event_name = %event_name, "SSE: sending session event to client");
    }
    let data = serde_json::to_string(envelope).unwrap_or_else(|_| "{}".to_string());
    Ok(Event::default().event(event_name).data(data))
}

fn lagged_event() -> Event {
    Event::default().event("lagged").data("{}")
}

/// Build the browser-visible liveness event emitted roughly every `PING_INTERVAL`.
///
/// The payload is intentionally a fixed `"{}"` so consumers do not have to defend
/// against a moving schema, while still being a valid SSE data field. The event
/// has no `id`, so it does not advance the browser's `Last-Event-ID` cursor —
/// Last-Event-ID replay continues to represent meaningful domain event ids.
fn ping_event() -> Event {
    Event::default().event("ping").data("{}")
}

async fn next_batched_event(
    rx: &mut broadcast::Receiver<DjinnEventEnvelope>,
    accumulator: &mut BatchAccumulator,
    interval: &mut tokio::time::Interval,
    ping_interval: &mut tokio::time::Interval,
) -> Option<Result<Event, Infallible>> {
    loop {
        let flushed = accumulator.flush();
        if let Some(first) = flushed.into_iter().next() {
            return Some(serialize_event(&first));
        }

        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(envelope) => {
                        let ready = accumulator.push(envelope);
                        if let Some(first) = ready.into_iter().next() {
                            return Some(serialize_event(&first));
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "SSE subscriber lagged");
                        return Some(Ok(lagged_event()));
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        let flushed = accumulator.flush();
                        if let Some(first) = flushed.into_iter().next() {
                            return Some(serialize_event(&first));
                        }
                        return None;
                    }
                }
            }
            _ = interval.tick() => {
                let flushed = accumulator.flush();
                if let Some(first) = flushed.into_iter().next() {
                    return Some(serialize_event(&first));
                }
            }
            _ = ping_interval.tick() => {
                // On a ping tick, drain any ready domain events first so pings
                // never delay a meaningful update. Only emit the named ping
                // event when there is nothing pending to send.
                let flushed = accumulator.flush();
                if let Some(first) = flushed.into_iter().next() {
                    return Some(serialize_event(&first));
                }
                return Some(Ok(ping_event()));
            }
        }
    }
}

pub async fn events_handler(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events().subscribe();
    let cancel = state.cancel().clone();
    tracing::info!("SSE: new client connected");

    let stream = stream::unfold(
        (
            rx,
            BatchAccumulator::new(),
            tokio::time::interval(FLUSH_INTERVAL),
            tokio::time::interval(PING_INTERVAL),
        ),
        |(mut rx, mut accumulator, mut interval, mut ping_interval)| async move {
            let next =
                next_batched_event(&mut rx, &mut accumulator, &mut interval, &mut ping_interval)
                    .await;
            next.map(|event| (event, (rx, accumulator, interval, ping_interval)))
        },
    );

    let stream = futures::StreamExt::take_until(stream, cancel.cancelled_owned());
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Serialize)]
pub struct DbInfo {
    path: String,
    backend: String,
    wsl: bool,
    direct_access_likely: bool,
}

pub async fn db_info_handler(State(state): State<AppState>) -> axum::Json<DbInfo> {
    let health = state.database_health();
    let path = std::path::PathBuf::from(&health.target);
    let wsl = is_wsl();
    let direct_access_likely = !path
        .components()
        .take(3)
        .any(|c| c.as_os_str().len() == 1 && path.starts_with("/mnt/"));

    axum::Json(DbInfo {
        path: path.to_string_lossy().into_owned(),
        backend: health.backend_label,
        wsl,
        direct_access_likely,
    })
}

fn is_wsl() -> bool {
    std::env::var("WSL_DISTRO_NAME").is_ok()
        || std::env::var("WSL_INTEROP").is_ok()
        || std::path::Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists()
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test: real time for timing assertions
mod tests {
    use super::*;
    use serde_json::json;

    fn task_updated_envelope(id: &str, title: &str) -> DjinnEventEnvelope {
        DjinnEventEnvelope {
            entity_type: "task",
            action: "updated",
            payload: json!({
                "task": {
                    "id": id,
                    "title": title,
                },
                "from_sync": false,
            }),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }

    fn epic_updated_envelope(id: &str, title: &str) -> DjinnEventEnvelope {
        DjinnEventEnvelope {
            entity_type: "epic",
            action: "updated",
            payload: json!({
                "id": id,
                "title": title,
            }),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }

    fn project_updated_envelope(id: &str, name: &str) -> DjinnEventEnvelope {
        DjinnEventEnvelope {
            entity_type: "project",
            action: "updated",
            payload: json!({
                "id": id,
                "name": name,
            }),
            id: None,
            project_id: Some(id.to_string()),
            from_sync: false,
        }
    }

    fn agent_updated_envelope(id: &str, status: &str) -> DjinnEventEnvelope {
        DjinnEventEnvelope {
            entity_type: "agent",
            action: "updated",
            payload: json!({
                "id": id,
                "status": status,
            }),
            id: Some(id.to_string()),
            project_id: None,
            from_sync: false,
        }
    }

    #[test]
    fn sse_event_name_uses_entity_type_and_action() {
        let envelope = DjinnEventEnvelope::task_deleted("t1");
        let event_name = format!("{}.{}", envelope.entity_type, envelope.action);
        assert_eq!(event_name, "task.deleted");
    }

    #[test]
    fn sse_payload_serializes_djinn_event_shape() {
        let envelope = DjinnEventEnvelope::task_deleted("t1");
        let json = serde_json::to_value(&envelope).expect("serialize envelope");

        assert_eq!(
            json.get("entity_type").and_then(|v| v.as_str()),
            Some("task")
        );
        assert_eq!(json.get("action").and_then(|v| v.as_str()), Some("deleted"));
        assert_eq!(
            json.get("payload")
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str()),
            Some("t1")
        );
    }

    #[test]
    fn adr_045_immediate_events_bypass_batching() {
        let mut accumulator = BatchAccumulator::new();
        let ready = accumulator.push(DjinnEventEnvelope::task_deleted("t1"));
        assert_eq!(ready.len(), 1);
        assert!(accumulator.flush().is_empty());
    }

    #[test]
    fn dispatch_pause_changed_events_bypass_batching() {
        let envelope = DjinnEventEnvelope {
            entity_type: "dispatch_pause",
            action: "changed",
            payload: json!({
                "scope": "project",
                "target_id": "project-1",
                "current": {
                    "paused_by": "admin-user",
                    "paused_at": "2026-06-12T00:00:00.000Z",
                    "reason": "maintenance"
                },
                "previous": null,
                "paused_by": "admin-user",
                "actor": "admin-user",
                "changed_at": "2026-06-12T00:00:00.000Z",
                "reason": "maintenance"
            }),
            id: Some("project-1".to_owned()),
            project_id: Some("project-1".to_owned()),
            from_sync: false,
        };

        let mut accumulator = BatchAccumulator::new();
        let ready = accumulator.push(envelope);

        assert_eq!(ready.len(), 1);
        assert_eq!(event_name(&ready[0]), "dispatch_pause.changed");
        assert_eq!(ready[0].project_id.as_deref(), Some("project-1"));
        assert_eq!(ready[0].payload()["scope"], "project");
        assert_eq!(ready[0].payload()["current"]["reason"], "maintenance");
        assert!(accumulator.flush().is_empty());
    }

    #[test]
    fn adr_045_coalesced_task_updates_keep_latest_per_entity() {
        let mut accumulator = BatchAccumulator::new();

        assert!(
            accumulator
                .push(task_updated_envelope("task-1", "old"))
                .is_empty()
        );
        assert!(
            accumulator
                .push(task_updated_envelope("task-1", "new"))
                .is_empty()
        );

        let flushed = accumulator.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].payload()["task"]["title"], "new");
    }

    #[test]
    fn adr_045_coalesced_flush_keeps_latest_for_each_entity_type() {
        let mut accumulator = BatchAccumulator::new();

        assert!(
            accumulator
                .push(task_updated_envelope("task-1", "task latest"))
                .is_empty()
        );
        assert!(
            accumulator
                .push(epic_updated_envelope("epic-1", "epic latest"))
                .is_empty()
        );
        assert!(
            accumulator
                .push(project_updated_envelope("project-1", "project latest"))
                .is_empty()
        );
        assert!(
            accumulator
                .push(agent_updated_envelope("agent-1", "busy"))
                .is_empty()
        );

        let flushed = accumulator.flush();
        assert_eq!(flushed.len(), 4);
        assert!(flushed.iter().any(|event| event.entity_type() == "task"
            && event.payload()["task"]["title"] == "task latest"));
        assert!(flushed.iter().any(
            |event| event.entity_type() == "epic" && event.payload()["title"] == "epic latest"
        ));
        assert!(
            flushed.iter().any(|event| event.entity_type() == "project"
                && event.payload()["name"] == "project latest")
        );
        assert!(
            flushed
                .iter()
                .any(|event| event.entity_type() == "agent" && event.payload()["status"] == "busy")
        );
    }

    #[test]
    fn adr_045_throttled_session_messages_keep_latest_until_interval_elapses() {
        let mut accumulator = BatchAccumulator::new();
        let first =
            DjinnEventEnvelope::session_message("s1", "t1", "worker", &json!({"content": "a"}));
        let second =
            DjinnEventEnvelope::session_message("s1", "t1", "worker", &json!({"content": "b"}));
        let ready = accumulator.push(first);
        assert_eq!(ready.len(), 1);
        assert!(accumulator.push(second).is_empty());
        assert!(accumulator.flush().is_empty());
        accumulator.throttled_last_sent.insert(
            "session.message",
            Instant::now() - SESSION_MESSAGE_MIN_INTERVAL,
        );
        let flushed = accumulator.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].payload()["message"]["content"], "b");
    }

    #[test]
    fn adr_045_throttled_token_updates_stay_pending_before_interval_and_flush_afterwards() {
        let mut accumulator = BatchAccumulator::new();
        let first = DjinnEventEnvelope {
            entity_type: "session",
            action: "token_update",
            payload: json!({"session_id": "s1", "task_id": "t1", "tokens": 1}),
            id: Some("s1".to_string()),
            project_id: None,
            from_sync: false,
        };
        let second = DjinnEventEnvelope {
            entity_type: "session",
            action: "token_update",
            payload: json!({"session_id": "s1", "task_id": "t1", "tokens": 2}),
            id: Some("s1".to_string()),
            project_id: None,
            from_sync: false,
        };

        assert_eq!(accumulator.push(first).len(), 1);
        assert!(accumulator.push(second).is_empty());
        assert!(accumulator.flush().is_empty());

        accumulator.throttled_last_sent.insert(
            "session.token_update",
            Instant::now() - SESSION_TOKEN_UPDATE_MIN_INTERVAL,
        );
        let flushed = accumulator.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].payload()["tokens"], 2);
    }

    #[test]
    fn adr_045_lagged_signal_still_bypasses_batch_state() {
        let event = lagged_event();
        let text = format!("{:?}", event);
        assert!(text.contains("lagged"));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn adr_045_mixed_execution_traffic_reduces_frames_and_preserves_batching_contract() {
        let mut accumulator = BatchAccumulator::new();

        let immediate = accumulator.push(DjinnEventEnvelope::task_deleted("task-1"));
        assert_eq!(immediate.len(), 1);
        assert_eq!(immediate[0].entity_type(), "task");
        assert_eq!(immediate[0].action(), "deleted");

        let raw_source_events = vec![
            task_updated_envelope("task-1", "oldest title"),
            task_updated_envelope("task-1", "latest title"),
            DjinnEventEnvelope {
                entity_type: "session",
                action: "token_update",
                payload: json!({"session_id": "s1", "task_id": "task-1", "tokens": 1}),
                id: Some("s1".to_string()),
                project_id: None,
                from_sync: false,
            },
            DjinnEventEnvelope {
                entity_type: "session",
                action: "token_update",
                payload: json!({"session_id": "s1", "task_id": "task-1", "tokens": 2}),
                id: Some("s1".to_string()),
                project_id: None,
                from_sync: false,
            },
            DjinnEventEnvelope::session_message(
                "s1",
                "task-1",
                "worker",
                &json!({"content": "first delta"}),
            ),
            DjinnEventEnvelope::session_message(
                "s1",
                "task-1",
                "worker",
                &json!({"content": "latest delta"}),
            ),
        ];

        let mut emitted_frames = immediate.len();
        let mut first_token_frame = None;
        let mut first_message_frame = None;

        for event in raw_source_events {
            let ready = accumulator.push(event);
            emitted_frames += ready.len();
            for envelope in ready {
                match (envelope.entity_type(), envelope.action()) {
                    ("session", "token_update") => first_token_frame = Some(envelope),
                    ("session", "message") => first_message_frame = Some(envelope),
                    _ => {}
                }
            }
        }

        let first_token_frame = first_token_frame.expect("first token frame");
        assert_eq!(first_token_frame.payload()["tokens"], 1);

        let first_message_frame = first_message_frame.expect("first message frame");
        assert_eq!(
            first_message_frame.payload()["message"]["content"],
            "first delta"
        );

        let initial_flush = accumulator.flush();
        assert_eq!(initial_flush.len(), 1);
        assert_eq!(initial_flush[0].entity_type(), "task");
        assert_eq!(initial_flush[0].action(), "updated");
        assert_eq!(initial_flush[0].payload()["task"]["title"], "latest title");

        accumulator.throttled_last_sent.insert(
            "session.message",
            Instant::now() - SESSION_MESSAGE_MIN_INTERVAL,
        );
        let message_flush = accumulator.flush();
        assert_eq!(message_flush.len(), 1);
        assert_eq!(message_flush[0].entity_type(), "session");
        assert_eq!(message_flush[0].action(), "message");
        assert_eq!(
            message_flush[0].payload()["message"]["content"],
            "latest delta"
        );

        accumulator.throttled_last_sent.insert(
            "session.token_update",
            Instant::now() - SESSION_TOKEN_UPDATE_MIN_INTERVAL,
        );
        let token_flush = accumulator.flush();
        assert_eq!(token_flush.len(), 1);
        assert_eq!(token_flush[0].entity_type(), "session");
        assert_eq!(token_flush[0].action(), "token_update");
        assert_eq!(token_flush[0].payload()["tokens"], 2);

        let total_emitted_frames =
            emitted_frames + initial_flush.len() + message_flush.len() + token_flush.len();
        assert_eq!(total_emitted_frames, 6);
        assert!(
            total_emitted_frames < 1 + 6,
            "mixed burst should emit fewer frames than raw source events"
        );
    }

    #[test]
    fn ping_event_has_named_event_and_empty_object_payload() {
        let event = ping_event();
        let text = format!("{event:?}");

        // The named event type is what the browser EventSource listener keys
        // off; assert the literal `event: ping` line is on the wire.
        assert!(
            text.contains("event: ping"),
            "ping event should declare `event: ping`, got: {text}"
        );
        // The data field is a stable empty JSON object so consumers don't have
        // to defend against a moving schema.
        assert!(
            text.contains("data: {}"),
            "ping event should carry a stable `data: {{}}` payload, got: {text}"
        );
        // The event must NOT carry an id so the browser's Last-Event-ID
        // cursor continues to track meaningful domain event ids.
        assert!(
            !text.contains("id:"),
            "ping event must not carry an id field, got: {text}"
        );
    }

    #[test]
    fn ping_interval_constant_matches_browser_liveness_cadence() {
        // 25s matches the proposal: server emits a client-detectable named
        // liveness signal about every 25s on /events, while the client
        // force-reconnects after about 60s of silence.
        assert_eq!(PING_INTERVAL, Duration::from_secs(25));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ping_tick_emits_named_ping_when_no_domain_events_pending() {
        // Empty broadcast channel — there are no domain events for the
        // stream to deliver, so the next ping tick must surface the named
        // `ping` event. We use a 50ms ping interval and a 5s flush interval
        // so the ping branch fires first; the production 25s cadence is
        // asserted separately in `ping_interval_constant_matches_browser_liveness_cadence`.
        let (_tx, mut rx) = broadcast::channel::<DjinnEventEnvelope>(8);
        let mut accumulator = BatchAccumulator::new();
        let mut flush_interval = tokio::time::interval(Duration::from_secs(5));
        let mut ping_interval = tokio::time::interval(Duration::from_millis(50));

        // tokio::time::interval fires the first tick instantly; consume it
        // so the next call to `next_batched_event` is racing the *second*
        // tick of the ping interval, not the immediate first one.
        flush_interval.tick().await;
        ping_interval.tick().await;

        let event = tokio::time::timeout(
            Duration::from_secs(1),
            next_batched_event(
                &mut rx,
                &mut accumulator,
                &mut flush_interval,
                &mut ping_interval,
            ),
        )
        .await
        .expect("ping tick should fire before timeout")
        .expect("next_batched_event should yield Some")
        .expect("ping result is Ok");

        let text = format!("{event:?}");
        assert!(
            text.contains("event: ping"),
            "ping tick should emit a named `ping` event, got: {text}"
        );
        assert!(
            text.contains("data: {}"),
            "ping event should carry the stable `data: {{}}` payload, got: {text}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ping_tick_flushes_pending_coalesced_event_before_emitting_ping() {
        // Use a 5s flush interval and 50ms ping interval. The coalesced
        // event in the accumulator must be flushed on the *ping* tick,
        // proving the ping branch drains pending work before emitting
        // a ping.
        let (tx, mut rx) = broadcast::channel::<DjinnEventEnvelope>(8);
        let mut accumulator = BatchAccumulator::new();
        let mut flush_interval = tokio::time::interval(Duration::from_secs(5));
        let mut ping_interval = tokio::time::interval(Duration::from_millis(50));

        // Burn the immediate first ticks.
        flush_interval.tick().await;
        ping_interval.tick().await;

        // Push a coalescable update; it should land in the accumulator and
        // not be returned immediately because the event is coalesced.
        let pushed = accumulator.push(task_updated_envelope("task-1", "oldest"));
        assert!(
            pushed.is_empty(),
            "coalesced events do not emit immediately"
        );
        assert_eq!(accumulator.coalesced.len(), 1);

        // Send a second coalescable update through the broadcast channel
        // and drain it into the accumulator so the coalesced map still
        // holds a single key (coalesce_key is the same for both pushes).
        tx.send(task_updated_envelope("task-1", "stale"))
            .expect("broadcast send");
        let _ = rx.recv().await;
        assert_eq!(
            accumulator.coalesced.len(),
            1,
            "coalesced map should keep a single entry per (entity, id) key"
        );

        let event = tokio::time::timeout(
            Duration::from_secs(1),
            next_batched_event(
                &mut rx,
                &mut accumulator,
                &mut flush_interval,
                &mut ping_interval,
            ),
        )
        .await
        .expect("ping tick should fire before timeout")
        .expect("next_batched_event should yield Some")
        .expect("ping result is Ok");

        // The ping tick must have flushed the coalesced domain event
        // *before* emitting a ping — pings must never delay meaningful
        // updates.
        let text = format!("{event:?}");
        assert!(
            text.contains("event: task.updated"),
            "ping tick should flush the pending coalesced domain event first, got: {text}"
        );
        assert!(
            !text.contains("event: ping"),
            "ping event must not be emitted when a domain event was pending, got: {text}"
        );
        assert!(
            accumulator.coalesced.is_empty(),
            "flush should drain the coalesced map"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn immediate_domain_event_is_delivered_before_next_ping_tick() {
        // Sanity check: an immediate-class event pushed after the ping tick
        // arm has been exercised must still flow through the rx branch on
        // the next call. This guards against the ping branch starving the
        // normal rx branch.
        let (tx, mut rx) = broadcast::channel::<DjinnEventEnvelope>(8);
        let mut accumulator = BatchAccumulator::new();
        let mut flush_interval = tokio::time::interval(Duration::from_secs(5));
        let mut ping_interval = tokio::time::interval(Duration::from_millis(50));

        flush_interval.tick().await;
        ping_interval.tick().await;

        // Send an immediate (not coalescable) event so the rx branch can
        // return it directly without waiting for either tick.
        let envelope = DjinnEventEnvelope::task_deleted("task-1");
        tx.send(envelope).expect("broadcast send");

        let event = tokio::time::timeout(
            Duration::from_secs(1),
            next_batched_event(
                &mut rx,
                &mut accumulator,
                &mut flush_interval,
                &mut ping_interval,
            ),
        )
        .await
        .expect("rx branch should deliver the immediate event before timeout")
        .expect("next_batched_event should yield Some")
        .expect("immediate result is Ok");

        let text = format!("{event:?}");
        assert!(
            text.contains("event: task.deleted"),
            "immediate event should be delivered as-is, got: {text}"
        );
    }
}
