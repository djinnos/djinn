//! Process-owned aggregate telemetry for memory retrieval.
//!
//! This module deliberately does not retain queries, project identifiers, or
//! individual observations. Callers construct and retain a metrics instance in
//! their process state, which makes its lifetime and ownership explicit.

use std::sync::Mutex;
use std::time::{Duration, SystemTime};

const RETRIEVAL_DURATION_SECONDS: &str = "djinn_memory_retrieval_duration_seconds";
const RETRIEVAL_CANDIDATES: &str = "djinn_memory_retrieval_candidates";
const RETRIEVAL_STAGE_DURATION_SECONDS: &str = "djinn_memory_retrieval_stage_duration_seconds";
const ENTRY_POINT_COUNT: usize = 4;
const OUTCOME_COUNT: usize = 3;
const STAGE_COUNT: usize = 6;

/// A fixed retrieval workload entry point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RetrievalEntryPoint {
    Dispatch,
    JitPitfalls,
    LoadKnowledgeContext,
    FormatKnowledgeNotes,
}

impl RetrievalEntryPoint {
    pub const ALL: [Self; ENTRY_POINT_COUNT] = [
        Self::Dispatch,
        Self::JitPitfalls,
        Self::LoadKnowledgeContext,
        Self::FormatKnowledgeNotes,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Dispatch => "dispatch",
            Self::JitPitfalls => "jit_pitfalls",
            Self::LoadKnowledgeContext => "load_knowledge_context",
            Self::FormatKnowledgeNotes => "format_knowledge_notes",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Dispatch => 0,
            Self::JitPitfalls => 1,
            Self::LoadKnowledgeContext => 2,
            Self::FormatKnowledgeNotes => 3,
        }
    }
}

/// A fixed terminal outcome for a retrieval attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RetrievalOutcome {
    Success,
    Empty,
    Error,
}

impl RetrievalOutcome {
    pub const ALL: [Self; OUTCOME_COUNT] = [Self::Success, Self::Empty, Self::Error];

    const fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Empty => "empty",
            Self::Error => "error",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Success => 0,
            Self::Empty => 1,
            Self::Error => 2,
        }
    }
}

/// A fixed retrieval pipeline stage whose duration is forwarded from the
/// repository search result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RetrievalStage {
    Lexical,
    Semantic,
    Temporal,
    Graph,
    RrfFuse,
    Embedding,
}

impl RetrievalStage {
    pub const ALL: [Self; STAGE_COUNT] = [
        Self::Lexical,
        Self::Semantic,
        Self::Temporal,
        Self::Graph,
        Self::RrfFuse,
        Self::Embedding,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Lexical => "lexical",
            Self::Semantic => "semantic",
            Self::Temporal => "temporal",
            Self::Graph => "graph",
            Self::RrfFuse => "rrf_fuse",
            Self::Embedding => "embedding",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Lexical => 0,
            Self::Semantic => 1,
            Self::Temporal => 2,
            Self::Graph => 3,
            Self::RrfFuse => 4,
            Self::Embedding => 5,
        }
    }
}

/// Count and sums for one fixed `(entry_point, outcome)` bucket.
///
/// Averages are intentionally not retained; consumers derive them from sums
/// and counts.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RetrievalAggregate {
    pub count: u64,
    pub duration_sum_seconds: f64,
    /// Sum using the same `f64` observation semantics as the Prometheus
    /// candidates histogram.
    pub candidate_sum: f64,
}

/// Immutable copy of all bounded retrieval aggregates.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryRetrievalSnapshot {
    aggregates: [[RetrievalAggregate; OUTCOME_COUNT]; ENTRY_POINT_COUNT],
    stage_aggregates: [[RetrievalAggregate; STAGE_COUNT]; ENTRY_POINT_COUNT],
}

impl MemoryRetrievalSnapshot {
    /// Return the aggregate for a fixed metric dimension pair.
    pub fn aggregate(
        &self,
        entry_point: RetrievalEntryPoint,
        outcome: RetrievalOutcome,
    ) -> RetrievalAggregate {
        self.aggregates[entry_point.index()][outcome.index()]
    }

    /// Return the aggregate for a fixed `(entry_point, stage)` dimension pair.
    pub fn stage_aggregate(
        &self,
        entry_point: RetrievalEntryPoint,
        stage: RetrievalStage,
    ) -> RetrievalAggregate {
        self.stage_aggregates[entry_point.index()][stage.index()]
    }
}

/// The mutex that protects the bounded aggregate state was poisoned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryRetrievalMetricsError {
    Poisoned,
}

impl std::fmt::Display for MemoryRetrievalMetricsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("memory retrieval metrics lock is poisoned")
    }
}

impl std::error::Error for MemoryRetrievalMetricsError {}

/// Dependency-injected, process-owned memory retrieval telemetry.
///
/// `started_at` never changes after construction. The only mutable state is a
/// single mutex-protected, fixed-size aggregate snapshot.
pub struct MemoryRetrievalMetrics {
    started_at: SystemTime,
    snapshot: Mutex<MemoryRetrievalSnapshot>,
}

impl Default for MemoryRetrievalMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryRetrievalMetrics {
    /// Create an empty process-owned metrics object, recording the wall-clock
    /// construction time once.
    #[allow(clippy::disallowed_methods)] // approved boundary: this constructor captures the process construction wall-clock; callers do not read time directly.
    pub fn new() -> Self {
        Self {
            started_at: SystemTime::now(),
            snapshot: Mutex::new(MemoryRetrievalSnapshot {
                aggregates: [[RetrievalAggregate::default(); OUTCOME_COUNT]; ENTRY_POINT_COUNT],
                stage_aggregates: [[RetrievalAggregate::default(); STAGE_COUNT]; ENTRY_POINT_COUNT],
            }),
        }
    }

    /// Time at which this instance was constructed.
    pub fn started_at(&self) -> SystemTime {
        self.started_at
    }

    /// Record one retrieval attempt.
    ///
    /// Snapshot mutation and both Prometheus histogram observations happen
    /// while the same mutex is held. A snapshot obtained before or after this
    /// call therefore cannot expose a partially recorded observation.
    pub fn observe(
        &self,
        entry_point: RetrievalEntryPoint,
        outcome: RetrievalOutcome,
        duration: Duration,
        candidates: u64,
    ) -> Result<(), MemoryRetrievalMetricsError> {
        let mut snapshot = self
            .snapshot
            .lock()
            .map_err(|_| MemoryRetrievalMetricsError::Poisoned)?;
        let aggregate = &mut snapshot.aggregates[entry_point.index()][outcome.index()];
        aggregate.count += 1;
        aggregate.duration_sum_seconds += duration.as_secs_f64();
        // Keep the in-memory aggregate's rounding behavior identical to the
        // histogram: each public `u64` input is converted before it is added.
        // Converting only an accumulated integer total would diverge once a
        // total exceeds the exactly representable `f64` integer range.
        let candidates = candidates as f64;
        aggregate.candidate_sum += candidates;

        metrics::histogram!(
            RETRIEVAL_DURATION_SECONDS,
            "entry_point" => entry_point.label(),
            "outcome" => outcome.label(),
        )
        .record(duration.as_secs_f64());
        metrics::histogram!(
            RETRIEVAL_CANDIDATES,
            "entry_point" => entry_point.label(),
            "outcome" => outcome.label(),
        )
        .record(candidates);
        Ok(())
    }

    /// Record one retrieval pipeline stage duration.
    ///
    /// Snapshot mutation and Prometheus histogram observation happen while the
    /// same mutex is held.
    pub fn observe_stage(
        &self,
        entry_point: RetrievalEntryPoint,
        stage: RetrievalStage,
        duration: Duration,
    ) -> Result<(), MemoryRetrievalMetricsError> {
        let mut snapshot = self
            .snapshot
            .lock()
            .map_err(|_| MemoryRetrievalMetricsError::Poisoned)?;
        let aggregate = &mut snapshot.stage_aggregates[entry_point.index()][stage.index()];
        aggregate.count += 1;
        aggregate.duration_sum_seconds += duration.as_secs_f64();

        metrics::histogram!(
            RETRIEVAL_STAGE_DURATION_SECONDS,
            "entry_point" => entry_point.label(),
            "stage" => stage.label(),
        )
        .record(duration.as_secs_f64());
        Ok(())
    }

    /// Copy the complete aggregate state while holding its single mutex.
    pub fn snapshot(&self) -> Result<MemoryRetrievalSnapshot, MemoryRetrievalMetricsError> {
        self.snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .map_err(|_| MemoryRetrievalMetricsError::Poisoned)
    }
}

pub(crate) fn register_metrics() {
    metrics::describe_histogram!(
        RETRIEVAL_DURATION_SECONDS,
        "Memory retrieval duration in seconds, partitioned by fixed entry_point and outcome labels."
    );
    metrics::describe_histogram!(
        RETRIEVAL_CANDIDATES,
        "Candidates considered by memory retrieval, partitioned by fixed entry_point and outcome labels."
    );
    metrics::describe_histogram!(
        RETRIEVAL_STAGE_DURATION_SECONDS,
        "Memory retrieval stage duration in seconds, partitioned by fixed entry_point and stage labels."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn sample_value(rendered: &str, metric: &str, entry_point: &str, outcome: &str) -> f64 {
        rendered
            .lines()
            .find(|line| {
                line.starts_with(metric)
                    && line.contains(&format!("entry_point=\"{entry_point}\""))
                    && line.contains(&format!("outcome=\"{outcome}\""))
            })
            .and_then(|line| line.rsplit_once(' '))
            .and_then(|(_, value)| value.parse().ok())
            .unwrap_or_else(|| panic!("missing {metric} sample in:\n{rendered}"))
    }

    #[test]
    fn snapshot_matches_prometheus_histogram_count_and_sum() {
        crate::init().expect("install recorder");
        let metrics = MemoryRetrievalMetrics::new();
        metrics
            .observe(
                RetrievalEntryPoint::Dispatch,
                RetrievalOutcome::Success,
                Duration::from_millis(250),
                3,
            )
            .unwrap();
        metrics
            .observe(
                RetrievalEntryPoint::Dispatch,
                RetrievalOutcome::Success,
                Duration::from_millis(750),
                5,
            )
            .unwrap();

        let aggregate = metrics
            .snapshot()
            .unwrap()
            .aggregate(RetrievalEntryPoint::Dispatch, RetrievalOutcome::Success);
        let rendered = crate::render().unwrap();
        assert_eq!(
            aggregate.count as f64,
            sample_value(
                &rendered,
                "djinn_memory_retrieval_duration_seconds_count",
                "dispatch",
                "success"
            )
        );
        assert_eq!(
            aggregate.duration_sum_seconds,
            sample_value(
                &rendered,
                "djinn_memory_retrieval_duration_seconds_sum",
                "dispatch",
                "success"
            )
        );
        assert_eq!(
            aggregate.count as f64,
            sample_value(
                &rendered,
                "djinn_memory_retrieval_candidates_count",
                "dispatch",
                "success"
            )
        );
        assert_eq!(
            aggregate.candidate_sum,
            sample_value(
                &rendered,
                "djinn_memory_retrieval_candidates_sum",
                "dispatch",
                "success"
            )
        );
    }

    #[test]
    fn candidate_sum_uses_prometheus_observation_rounding() {
        crate::init().expect("install recorder");
        let metrics = MemoryRetrievalMetrics::new();
        for candidates in [9_007_199_254_740_993, 1] {
            metrics
                .observe(
                    RetrievalEntryPoint::JitPitfalls,
                    RetrievalOutcome::Empty,
                    Duration::ZERO,
                    candidates,
                )
                .unwrap();
        }

        let aggregate = metrics
            .snapshot()
            .unwrap()
            .aggregate(RetrievalEntryPoint::JitPitfalls, RetrievalOutcome::Empty);
        let rendered = crate::render().unwrap();
        let prometheus_sum = sample_value(
            &rendered,
            "djinn_memory_retrieval_candidates_sum",
            "jit_pitfalls",
            "empty",
        );

        // `2^53 + 1` is first rounded to `2^53`, then adding one remains at
        // `2^53`; this must be the snapshot's exact histogram semantics too.
        assert_eq!(aggregate.candidate_sum, 9_007_199_254_740_992.0);
        assert_eq!(aggregate.candidate_sum, prometheus_sum);
    }

    #[test]
    fn poisoned_lock_is_reported_by_observe_and_snapshot() {
        let metrics = Arc::new(MemoryRetrievalMetrics::new());
        let poisoned = Arc::clone(&metrics);
        let _ = thread::spawn(move || {
            let _guard = poisoned.snapshot.lock().unwrap();
            panic!("poison metrics lock");
        })
        .join();

        assert_eq!(
            metrics.snapshot(),
            Err(MemoryRetrievalMetricsError::Poisoned)
        );
        assert_eq!(
            metrics.observe(
                RetrievalEntryPoint::Dispatch,
                RetrievalOutcome::Success,
                Duration::ZERO,
                0,
            ),
            Err(MemoryRetrievalMetricsError::Poisoned)
        );
    }

    #[test]
    fn concurrent_readers_only_observe_complete_aggregates() {
        let metrics = Arc::new(MemoryRetrievalMetrics::new());
        let writers = 4;
        let per_writer = 250;
        let mut handles = Vec::new();
        for _ in 0..writers {
            let metrics = Arc::clone(&metrics);
            handles.push(thread::spawn(move || {
                for _ in 0..per_writer {
                    metrics
                        .observe(
                            RetrievalEntryPoint::LoadKnowledgeContext,
                            RetrievalOutcome::Success,
                            Duration::from_millis(1),
                            2,
                        )
                        .unwrap();
                }
            }));
        }
        let reader = Arc::clone(&metrics);
        let reader_handle = thread::spawn(move || {
            for _ in 0..1_000 {
                let aggregate = reader.snapshot().unwrap().aggregate(
                    RetrievalEntryPoint::LoadKnowledgeContext,
                    RetrievalOutcome::Success,
                );
                assert!(
                    (aggregate.duration_sum_seconds - aggregate.count as f64 * 0.001).abs() < 1e-12
                );
                assert_eq!(aggregate.candidate_sum, aggregate.count as f64 * 2.0);
            }
        });
        for handle in handles {
            handle.join().unwrap();
        }
        reader_handle.join().unwrap();
        let aggregate = metrics.snapshot().unwrap().aggregate(
            RetrievalEntryPoint::LoadKnowledgeContext,
            RetrievalOutcome::Success,
        );
        assert_eq!(aggregate.count, (writers * per_writer) as u64);
        assert_eq!(aggregate.candidate_sum, (writers * per_writer * 2) as f64);
    }

    fn stage_sample_value(rendered: &str, entry_point: &str, stage: &str) -> f64 {
        rendered
            .lines()
            .find(|line| {
                line.starts_with("djinn_memory_retrieval_stage_duration_seconds_sum")
                    && line.contains(&format!("entry_point=\"{entry_point}\""))
                    && line.contains(&format!("stage=\"{stage}\""))
            })
            .and_then(|line| line.rsplit_once(' '))
            .and_then(|(_, value)| value.parse().ok())
            .unwrap_or_else(|| panic!("missing stage sample in:\n{rendered}"))
    }

    #[test]
    fn stage_duration_snapshot_matches_prometheus_histogram() {
        crate::init().expect("install recorder");
        let metrics = MemoryRetrievalMetrics::new();
        metrics
            .observe_stage(
                RetrievalEntryPoint::Dispatch,
                RetrievalStage::Lexical,
                Duration::from_millis(100),
            )
            .unwrap();
        metrics
            .observe_stage(
                RetrievalEntryPoint::Dispatch,
                RetrievalStage::Semantic,
                Duration::from_millis(200),
            )
            .unwrap();

        let snapshot = metrics.snapshot().unwrap();
        let lexical =
            snapshot.stage_aggregate(RetrievalEntryPoint::Dispatch, RetrievalStage::Lexical);
        let semantic =
            snapshot.stage_aggregate(RetrievalEntryPoint::Dispatch, RetrievalStage::Semantic);

        assert_eq!(lexical.count, 1);
        assert_eq!(semantic.count, 1);
        assert!((lexical.duration_sum_seconds - 0.1).abs() < 1e-12);
        assert!((semantic.duration_sum_seconds - 0.2).abs() < 1e-12);

        let rendered = crate::render().unwrap();
        assert_eq!(
            lexical.duration_sum_seconds,
            stage_sample_value(&rendered, "dispatch", "lexical")
        );
        assert_eq!(
            semantic.duration_sum_seconds,
            stage_sample_value(&rendered, "dispatch", "semantic")
        );
    }
}
