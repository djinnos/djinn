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
//! `sqlx::query` target, so it neither swallows nor reorders anything else.
//!
//! Capture is **task-scoped**, not global. `cargo test` runs the tests of one
//! crate concurrently inside a single process, so a shared open/close window
//! lets one test's `finish` truncate another test's in-flight measurement — and
//! an empty trace is exactly what that looks like. Buffering into a
//! `tokio::task_local` instead means each `capture_queries` call measures the
//! statements issued by *its own* future and nothing else, under both `cargo
//! test` and `cargo nextest`.
//!
//! A capture whose subscriber never installed (because some other global
//! subscriber won the race) records zero statements. Every assertion built on
//! this observer must therefore carry a **lower** bound as well as an upper
//! one, so an instrumentation failure fails the test instead of passing it
//! vacuously.

use std::future::Future;
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

type CaptureBuffer = Arc<Mutex<Vec<String>>>;

tokio::task_local! {
    /// Buffer for the innermost enclosing [`capture_queries`] call. Absent
    /// outside a capture, in which case observed statements are discarded.
    static CAPTURE_BUFFER: CaptureBuffer;
}

struct QueryCountingSubscriber;

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
        let mut visitor = StatementVisitor::default();
        event.record(&mut visitor);
        let statement = visitor.statement.unwrap_or_default();
        // Outside a capture the task-local is unset and the statement is
        // dropped, so unrelated concurrent tests cost nothing and cannot
        // contaminate a measurement.
        let _ = CAPTURE_BUFFER.try_with(|buffer| {
            if let Ok(mut statements) = buffer.lock() {
                statements.push(statement);
            }
        });
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

fn install_subscriber() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        // Ignore an already-installed global default: the resulting empty trace
        // fails the lower bound of every assertion built on it.
        let _ = tracing::subscriber::set_global_default(QueryCountingSubscriber);
    });
}

/// Run `future` and return its output alongside every SQL statement it issued.
///
/// Measurement is scoped to this future's task, so concurrent tests in the same
/// process cannot truncate or pollute each other's traces.
pub async fn capture_queries<F, T>(future: F) -> (T, QueryTrace)
where
    F: Future<Output = T>,
{
    install_subscriber();
    let buffer: CaptureBuffer = Arc::new(Mutex::new(Vec::new()));
    let value = CAPTURE_BUFFER.scope(buffer.clone(), future).await;
    let statements = buffer.lock().map(|guard| guard.clone()).unwrap_or_default();
    (value, QueryTrace { statements })
}
