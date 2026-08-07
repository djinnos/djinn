//! Real SQL round-trip instrumentation for repository fixtures.
//!
//! # Why this and not a hand-maintained counter
//!
//! A counter incremented by repository code proves nothing: a refactor that
//! moves or adds a query simply stops incrementing it, and the bound stays
//! green while the fan-out returns. This observer instead counts the events
//! **`sqlx` itself emits from inside its own execution path** — one
//! `target: "sqlx::query"` record per executed statement, produced by
//! `sqlx_core`'s query logger, not by anything in this crate.
//!
//! Consequently there is no way for repository code to issue a statement
//! through the pool without this observer seeing it, and no way to satisfy a
//! bound by editing our own bookkeeping.
//!
//! # Scope
//!
//! Compiled only under `cfg(test)` / the `test-support` feature. The subscriber
//! is installed once per process as the global default and only accepts the
//! `sqlx::query` target, so it neither swallows nor reorders anything else. The
//! test harness runs one test per process, so a single active capture window is
//! sufficient.
//!
//! A capture that never installs (because some other global subscriber won the
//! race) records zero statements. Every assertion built on this observer must
//! therefore carry a **lower** bound as well as an upper one, so an
//! instrumentation failure fails the test instead of passing it vacuously.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// Statements observed during one capture window.
#[derive(Debug, Clone, Default)]
pub struct QueryTrace {
    pub statements: Vec<String>,
}

impl QueryTrace {
    /// Total SQL round trips issued through `sqlx` during the window.
    pub fn round_trips(&self) -> usize {
        self.statements.len()
    }

    /// Round trips whose statement text contains `needle`.
    ///
    /// Use a fragment unique to the query under test (for example the
    /// score-matrix CTE name) so the count is attributable.
    pub fn matching(&self, needle: &str) -> usize {
        self.statements
            .iter()
            .filter(|statement| statement.contains(needle))
            .count()
    }

    /// Every observed statement, newline separated. Useful in assertion
    /// messages when a bound is exceeded.
    pub fn rendered(&self) -> String {
        self.statements
            .iter()
            .enumerate()
            .map(|(index, statement)| format!("[{index}] {}", statement.replace('\n', " ")))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Default)]
struct Recorder {
    active: AtomicBool,
    statements: Mutex<Vec<String>>,
}

fn recorder() -> &'static Arc<Recorder> {
    static RECORDER: OnceLock<Arc<Recorder>> = OnceLock::new();
    RECORDER.get_or_init(|| Arc::new(Recorder::default()))
}

struct QueryCountingSubscriber {
    recorder: Arc<Recorder>,
}

impl Subscriber for QueryCountingSubscriber {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        // `sqlx` gates its own emission on a dynamic `enabled` check against the
        // installed dispatcher, so returning true here is what makes the query
        // records exist at all.
        metadata.target() == "sqlx::query"
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        // Spans are never enabled for this subscriber; a stable non-zero id is
        // all the contract requires.
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        if event.metadata().target() != "sqlx::query" {
            return;
        }
        if !self.recorder.active.load(Ordering::SeqCst) {
            return;
        }
        let mut visitor = StatementVisitor::default();
        event.record(&mut visitor);
        if let Ok(mut statements) = self.recorder.statements.lock() {
            statements.push(visitor.statement.unwrap_or_default());
        }
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

#[derive(Default)]
struct StatementVisitor {
    statement: Option<String>,
}

impl Visit for StatementVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "db.statement" {
            self.statement = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "db.statement" {
            self.statement = Some(format!("{value:?}"));
        }
    }
}

/// Open a capture window. Any previously buffered statements are discarded.
pub fn start_query_capture() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let subscriber = QueryCountingSubscriber {
            recorder: recorder().clone(),
        };
        // Ignore an already-installed global default: the resulting empty trace
        // fails the lower bound of every assertion built on it.
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
    let recorder = recorder();
    if let Ok(mut statements) = recorder.statements.lock() {
        statements.clear();
    }
    recorder.active.store(true, Ordering::SeqCst);
}

/// Close the capture window and take the observed statements.
pub fn finish_query_capture() -> QueryTrace {
    let recorder = recorder();
    recorder.active.store(false, Ordering::SeqCst);
    let statements = recorder
        .statements
        .lock()
        .map(|mut guard| guard.drain(..).collect::<Vec<_>>())
        .unwrap_or_default();
    QueryTrace { statements }
}
