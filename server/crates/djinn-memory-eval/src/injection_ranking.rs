//! Knowledge-injection ranking evaluation (proposal `5205`).
//!
//! # What lives here and what deliberately does not
//!
//! This module is the *machinery*: the fixture / manifest / baseline schema,
//! the loader, SHA-256 hashing, the integrity and provenance validators, the
//! graded metrics (nDCG@k, Recall@k, MRR@k), and the deterministic replay
//! adapter that runs captured per-signal lists through the **real** production
//! fusion (`RankingProfile::KnowledgeInjectionV1`) and the **real** production
//! packer (`pack_ranked_knowledge_notes`).
//!
//! The judged corpus itself is **not** in this repository and must never be.
//! Relevance judgments, production trace IDs, and a captured production
//! baseline are empirical artifacts of one specific deployment; committing them
//! would bake one operator's note IDs, task IDs, and commits into shared code
//! and would let a hand-authored file make every downstream metric green while
//! proving nothing. Each deployment therefore supplies its own corpus through
//! `--manifest`, and the default location is git-ignored.
//!
//! Because the corpus is not in git, the proposal's `oracle-pin` commit-SHA
//! file and its `git diff` guard have no artifact to pin. Integrity is enforced
//! instead by the manifest's own content hashes, which this module verifies on
//! every run: a mismatch is a hard failure, exactly as a diff guard would be.
//!
//! # Corpus layout
//!
//! ```text
//! <manifest>.json                 # identity + provenance, hashes everything else
//! <fixture>.jsonl                 # one InjectionRankingCase per line
//! <baseline>.json                 # captured pre-cutover ordering per case
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use djinn_db::repositories::note::{
    KNOWLEDGE_INJECTION_CANDIDATE_WINDOW, RankingProfile, ScopeCandidate, injection_rrf_k,
    rank_scope_candidates, rrf_fuse_with_profile,
};
use djinn_slot::helpers::{KnowledgePackConfig, NotePackDisposition, pack_ranked_knowledge_notes};

/// Schema identity. A later corpus revision requires a new version rather than
/// mutation of `v1`.
pub const SCHEMA_VERSION: &str = "injection-ranking-v1";

/// The evaluated cutoff. Fixed by the proposal.
pub const REQUIRED_CUTOFF: usize = 10;

/// Minimum corpus size and breadth the proposal requires.
pub const MIN_CASES: usize = 50;
pub const MIN_REPOSITORY_SCOPES: usize = 5;

/// Git-ignored default corpus directory, relative to the crate root.
pub const DEFAULT_CORPUS_DIR: &str = "fixtures/local";

/// Ordered per-signal lists in the fixed fusion order.
pub const SIGNAL_ORDER: [&str; 5] = ["lexical", "semantic", "temporal", "graph", "task_affinity"];

// ── Schema ─────────────────────────────────────────────────────────────────

/// One immutable candidate snapshot entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixtureCandidate {
    pub note_id: String,
    pub note_type: String,
    pub permalink: String,
    pub title: String,
    #[serde(default)]
    pub scope_paths: Vec<String>,
    pub content: String,
    #[serde(default)]
    pub abstract_: Option<String>,
    pub confidence: f64,
}

/// The task anchor a case pins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixtureTask {
    pub task_id: String,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
}

/// One de-identified judged case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InjectionRankingCase {
    pub case_id: String,
    /// Opaque label grouping cases by repository area, used only to prove the
    /// corpus spans several unrelated scopes.
    pub repository_scope: String,
    pub task: FixtureTask,
    pub base_commit: String,
    /// What the base-tree path provider is expected to return for this case.
    #[serde(default)]
    pub expected_scope_paths: Vec<String>,
    /// Set when the case pins a provider-unavailable derivation instead.
    #[serde(default)]
    pub expected_scope_fallback_reason: Option<String>,
    pub candidates: Vec<FixtureCandidate>,
    /// Captured per-signal ordered note IDs, keyed by [`SIGNAL_ORDER`] names.
    /// A missing or empty signal contributes an empty list.
    #[serde(default)]
    pub signals: BTreeMap<String, Vec<String>>,
    /// Graded relevance, note ID → 0..=3. At least one entry must be positive.
    pub judgments: BTreeMap<String, u8>,
}

/// Per-case provenance recorded by the manifest, in stable case-ID order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestCaseRecord {
    pub case_id: String,
    pub source_trace_id: String,
    pub captured_at: String,
    pub task_id: String,
    pub base_commit: String,
    pub candidate_snapshot_sha256: String,
    pub judgment_provenance_record_id: String,
    pub judgment_recorded_at: String,
    pub fixture_row_sha256: String,
}

/// The immutable identity and provenance record for the fixture and baseline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InjectionRankingManifest {
    pub schema_version: String,
    /// Paths relative to the manifest's own directory.
    pub fixture_path: String,
    pub baseline_path: String,
    pub fixture_sha256: String,
    pub baseline_sha256: String,
    pub pinned_vector_sha256: String,
    /// The revision the baseline outputs were captured from.
    pub baseline_commit: String,
    pub cutoff: usize,
    pub candidate_window: usize,
    pub prompt_byte_budget: usize,
    pub minimum_confidence: f64,
    pub line_byte_cap: usize,
    pub cases: Vec<ManifestCaseRecord>,
    /// Any key the schema does not define. Used to reject a manifest that tries
    /// to record the identity of its own commit.
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Manifest keys that would record the identity of the manifest's own commit.
/// The proposal forbids that: the manifest must never name the commit that
/// contains it.
const FORBIDDEN_SELF_IDENTITY_KEYS: [&str; 6] = [
    "commit",
    "commit_sha",
    "manifest_commit",
    "manifest_commit_sha",
    "own_commit",
    "self_commit",
];

/// One case's captured pre-cutover outputs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineCase {
    pub case_id: String,
    /// Ordered injected note IDs the pre-cutover path produced.
    pub ordered_note_ids: Vec<String>,
    #[serde(default)]
    pub bytes_packed: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InjectionRankingBaseline {
    pub schema_version: String,
    pub captured_from_commit: String,
    pub cases: Vec<BaselineCase>,
}

// ── Errors ─────────────────────────────────────────────────────────────────

/// Every condition on which the acceptance command must exit non-zero.
#[derive(Debug)]
pub enum EvalError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Malformed {
        path: PathBuf,
        detail: String,
    },
    /// A recorded hash does not match the bytes on disk.
    HashMismatch {
        what: String,
        expected: String,
        actual: String,
    },
    /// Missing or contradictory provenance.
    Provenance(String),
    /// A schema/window/cutoff/budget constraint is violated.
    Contract(String),
    /// Macro nDCG@10 improvement below the required threshold.
    NdcgBelowThreshold {
        delta: f64,
        required: f64,
    },
    /// Macro Recall@10 fell further below baseline than allowed.
    RecallDropTooLarge {
        drop: f64,
        allowed: f64,
    },
    /// Repeated runs disagreed on ordering or dispositions.
    Nondeterministic(String),
    /// A packed result exceeded the manifest byte ceiling.
    ByteCeilingExceeded {
        case_id: String,
        bytes: usize,
        ceiling: usize,
    },
    /// The oversized/no-backfill invariant did not hold.
    NoBackfillRegression(String),
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Malformed { path, detail } => {
                write!(f, "malformed {}: {detail}", path.display())
            }
            Self::HashMismatch {
                what,
                expected,
                actual,
            } => write!(
                f,
                "hash mismatch for {what}: expected {expected}, got {actual}"
            ),
            Self::Provenance(detail) => write!(f, "provenance failure: {detail}"),
            Self::Contract(detail) => write!(f, "contract failure: {detail}"),
            Self::NdcgBelowThreshold { delta, required } => write!(
                f,
                "macro nDCG@{REQUIRED_CUTOFF} improvement {delta:.4} is below the required {required:.4}"
            ),
            Self::RecallDropTooLarge { drop, allowed } => write!(
                f,
                "macro Recall@{REQUIRED_CUTOFF} dropped {drop:.4} below baseline, more than the allowed {allowed:.4}"
            ),
            Self::Nondeterministic(detail) => write!(f, "nondeterministic result: {detail}"),
            Self::ByteCeilingExceeded {
                case_id,
                bytes,
                ceiling,
            } => write!(
                f,
                "case {case_id} packed {bytes} bytes, above the manifest ceiling of {ceiling}"
            ),
            Self::NoBackfillRegression(detail) => {
                write!(f, "no-backfill regression: {detail}")
            }
        }
    }
}

impl std::error::Error for EvalError {}

pub type EvalResult<T> = Result<T, EvalError>;

// ── Hashing ────────────────────────────────────────────────────────────────

/// Lowercase hex SHA-256 of arbitrary bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// The canonical hash of one candidate snapshot: SHA-256 over the candidates
/// serialized in note-ID order, so a reordering of the JSON array cannot change
/// the hash while a content change always does.
pub fn candidate_snapshot_sha256(candidates: &[FixtureCandidate]) -> EvalResult<String> {
    let mut sorted: Vec<&FixtureCandidate> = candidates.iter().collect();
    sorted.sort_by(|a, b| a.note_id.cmp(&b.note_id));
    let encoded = serde_json::to_vec(&sorted).map_err(|error| EvalError::Malformed {
        path: PathBuf::from("<candidate snapshot>"),
        detail: error.to_string(),
    })?;
    Ok(sha256_hex(&encoded))
}

// ── Loading and validation ─────────────────────────────────────────────────

/// A fully validated corpus.
#[derive(Debug, Clone)]
pub struct LoadedCorpus {
    pub manifest: InjectionRankingManifest,
    pub cases: Vec<InjectionRankingCase>,
    pub baseline: InjectionRankingBaseline,
}

fn read_bytes(path: &Path) -> EvalResult<Vec<u8>> {
    std::fs::read(path).map_err(|source| EvalError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Load and fully validate the corpus named by `manifest_path`.
///
/// Every failure here is a hard evaluation failure, never a warning: a missing
/// file, a malformed document, any hash mismatch, missing provenance, a corpus
/// that is too small or too narrow, an out-of-range judgment, a case with no
/// positive judgment, a judgment naming an unknown candidate, a manifest that
/// records the identity of its own commit, or a cutoff/window/budget that does
/// not match the contract.
pub fn load_corpus(manifest_path: &Path) -> EvalResult<LoadedCorpus> {
    let manifest_bytes = read_bytes(manifest_path)?;
    let manifest: InjectionRankingManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|error| EvalError::Malformed {
            path: manifest_path.to_path_buf(),
            detail: error.to_string(),
        })?;

    validate_manifest_shape(&manifest)?;

    let base_dir = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let fixture_path = base_dir.join(&manifest.fixture_path);
    let baseline_path = base_dir.join(&manifest.baseline_path);

    let fixture_bytes = read_bytes(&fixture_path)?;
    let actual_fixture_hash = sha256_hex(&fixture_bytes);
    if actual_fixture_hash != manifest.fixture_sha256 {
        return Err(EvalError::HashMismatch {
            what: format!("fixture {}", fixture_path.display()),
            expected: manifest.fixture_sha256.clone(),
            actual: actual_fixture_hash,
        });
    }

    let baseline_bytes = read_bytes(&baseline_path)?;
    let actual_baseline_hash = sha256_hex(&baseline_bytes);
    if actual_baseline_hash != manifest.baseline_sha256 {
        return Err(EvalError::HashMismatch {
            what: format!("baseline {}", baseline_path.display()),
            expected: manifest.baseline_sha256.clone(),
            actual: actual_baseline_hash,
        });
    }

    let fixture_text = String::from_utf8(fixture_bytes).map_err(|error| EvalError::Malformed {
        path: fixture_path.clone(),
        detail: error.to_string(),
    })?;
    let mut cases = Vec::new();
    for (index, line) in fixture_text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let case: InjectionRankingCase =
            serde_json::from_str(trimmed).map_err(|error| EvalError::Malformed {
                path: fixture_path.clone(),
                detail: format!("line {}: {error}", index + 1),
            })?;
        // The row hash covers the exact bytes of the line, so any edit to the
        // corpus invalidates the manifest.
        cases.push((case, sha256_hex(trimmed.as_bytes())));
    }

    let baseline: InjectionRankingBaseline =
        serde_json::from_slice(&baseline_bytes).map_err(|error| EvalError::Malformed {
            path: baseline_path.clone(),
            detail: error.to_string(),
        })?;
    if baseline.schema_version != SCHEMA_VERSION {
        return Err(EvalError::Contract(format!(
            "baseline schema_version {} != {SCHEMA_VERSION}",
            baseline.schema_version
        )));
    }
    if baseline.captured_from_commit != manifest.baseline_commit {
        return Err(EvalError::Provenance(format!(
            "baseline captured_from_commit {} does not match manifest baseline_commit {}",
            baseline.captured_from_commit, manifest.baseline_commit
        )));
    }

    validate_cases(&manifest, &cases, &baseline)?;

    Ok(LoadedCorpus {
        manifest,
        cases: cases.into_iter().map(|(case, _)| case).collect(),
        baseline,
    })
}

fn validate_manifest_shape(manifest: &InjectionRankingManifest) -> EvalResult<()> {
    if manifest.schema_version != SCHEMA_VERSION {
        return Err(EvalError::Contract(format!(
            "manifest schema_version {} != {SCHEMA_VERSION}",
            manifest.schema_version
        )));
    }
    for key in manifest.extra.keys() {
        if FORBIDDEN_SELF_IDENTITY_KEYS.contains(&key.as_str()) {
            return Err(EvalError::Provenance(format!(
                "manifest records the identity of its own commit via `{key}`; \
                 the manifest must never name the commit that contains it"
            )));
        }
    }
    if manifest.cutoff != REQUIRED_CUTOFF {
        return Err(EvalError::Contract(format!(
            "manifest cutoff {} != {REQUIRED_CUTOFF}",
            manifest.cutoff
        )));
    }
    if manifest.candidate_window != KNOWLEDGE_INJECTION_CANDIDATE_WINDOW {
        return Err(EvalError::Contract(format!(
            "manifest candidate_window {} != {KNOWLEDGE_INJECTION_CANDIDATE_WINDOW}",
            manifest.candidate_window
        )));
    }
    if manifest.prompt_byte_budget == 0 || manifest.line_byte_cap == 0 {
        return Err(EvalError::Contract(
            "manifest prompt_byte_budget and line_byte_cap must be positive".to_owned(),
        ));
    }
    if manifest.baseline_commit.trim().is_empty() {
        return Err(EvalError::Provenance(
            "manifest baseline_commit is empty".to_owned(),
        ));
    }
    if manifest.pinned_vector_sha256.trim().is_empty() {
        return Err(EvalError::Provenance(
            "manifest pinned_vector_sha256 is empty".to_owned(),
        ));
    }
    Ok(())
}

fn validate_cases(
    manifest: &InjectionRankingManifest,
    cases: &[(InjectionRankingCase, String)],
    baseline: &InjectionRankingBaseline,
) -> EvalResult<()> {
    if cases.len() < MIN_CASES {
        return Err(EvalError::Contract(format!(
            "corpus has {} cases, fewer than the required {MIN_CASES}",
            cases.len()
        )));
    }
    let scopes: BTreeSet<&str> = cases
        .iter()
        .map(|(case, _)| case.repository_scope.as_str())
        .collect();
    if scopes.len() < MIN_REPOSITORY_SCOPES {
        return Err(EvalError::Contract(format!(
            "corpus spans {} repository scopes, fewer than the required {MIN_REPOSITORY_SCOPES}",
            scopes.len()
        )));
    }

    if manifest.cases.len() != cases.len() {
        return Err(EvalError::Provenance(format!(
            "manifest records {} cases but the fixture holds {}",
            manifest.cases.len(),
            cases.len()
        )));
    }
    // Provenance is recorded in stable case-ID order.
    let mut sorted_ids: Vec<&str> = manifest.cases.iter().map(|r| r.case_id.as_str()).collect();
    let recorded_order = sorted_ids.clone();
    sorted_ids.sort_unstable();
    if recorded_order != sorted_ids {
        return Err(EvalError::Provenance(
            "manifest case records are not in stable case-ID order".to_owned(),
        ));
    }

    let records: BTreeMap<&str, &ManifestCaseRecord> = manifest
        .cases
        .iter()
        .map(|record| (record.case_id.as_str(), record))
        .collect();
    let baseline_ids: BTreeSet<&str> = baseline
        .cases
        .iter()
        .map(|case| case.case_id.as_str())
        .collect();

    for (case, row_hash) in cases {
        let Some(record) = records.get(case.case_id.as_str()) else {
            return Err(EvalError::Provenance(format!(
                "no manifest record for case {}",
                case.case_id
            )));
        };
        if record.source_trace_id.trim().is_empty()
            || record.captured_at.trim().is_empty()
            || record.judgment_provenance_record_id.trim().is_empty()
            || record.judgment_recorded_at.trim().is_empty()
        {
            return Err(EvalError::Provenance(format!(
                "case {} is missing source-trace or judgment provenance",
                case.case_id
            )));
        }
        if record.task_id != case.task.task_id || record.base_commit != case.base_commit {
            return Err(EvalError::Provenance(format!(
                "case {} task/base-commit anchor disagrees with the manifest",
                case.case_id
            )));
        }
        if &record.fixture_row_sha256 != row_hash {
            return Err(EvalError::HashMismatch {
                what: format!("fixture row for case {}", case.case_id),
                expected: record.fixture_row_sha256.clone(),
                actual: row_hash.clone(),
            });
        }
        let snapshot_hash = candidate_snapshot_sha256(&case.candidates)?;
        if record.candidate_snapshot_sha256 != snapshot_hash {
            return Err(EvalError::HashMismatch {
                what: format!("candidate snapshot for case {}", case.case_id),
                expected: record.candidate_snapshot_sha256.clone(),
                actual: snapshot_hash,
            });
        }
        if !baseline_ids.contains(case.case_id.as_str()) {
            return Err(EvalError::Provenance(format!(
                "baseline has no captured outputs for case {}",
                case.case_id
            )));
        }

        if case.candidates.len() > manifest.candidate_window {
            return Err(EvalError::Contract(format!(
                "case {} snapshots {} candidates, above the {} window",
                case.case_id,
                case.candidates.len(),
                manifest.candidate_window
            )));
        }
        let known: BTreeSet<&str> = case
            .candidates
            .iter()
            .map(|candidate| candidate.note_id.as_str())
            .collect();
        if case.judgments.is_empty() {
            return Err(EvalError::Contract(format!(
                "case {} has no relevance judgments",
                case.case_id
            )));
        }
        let mut positive = false;
        for (note_id, grade) in &case.judgments {
            if *grade > 3 {
                return Err(EvalError::Contract(format!(
                    "case {} grades note {note_id} as {grade}, outside 0..=3",
                    case.case_id
                )));
            }
            if *grade > 0 {
                positive = true;
            }
            if !known.contains(note_id.as_str()) {
                return Err(EvalError::Contract(format!(
                    "case {} judges note {note_id}, which is not in its candidate snapshot",
                    case.case_id
                )));
            }
        }
        if !positive {
            return Err(EvalError::Contract(format!(
                "case {} has no positive relevance judgment",
                case.case_id
            )));
        }
        for (signal, ids) in &case.signals {
            if !SIGNAL_ORDER.contains(&signal.as_str()) {
                return Err(EvalError::Contract(format!(
                    "case {} carries unknown signal `{signal}`",
                    case.case_id
                )));
            }
            if ids.len() > manifest.candidate_window {
                return Err(EvalError::Contract(format!(
                    "case {} signal `{signal}` holds {} entries, above the {} window",
                    case.case_id,
                    ids.len(),
                    manifest.candidate_window
                )));
            }
        }
    }
    Ok(())
}

// ── Graded metrics ─────────────────────────────────────────────────────────

fn gain(grade: u8) -> f64 {
    (2f64.powi(i32::from(grade)) - 1.0).max(0.0)
}

/// nDCG@`k` with exponential gain and `log2(rank + 1)` discount.
pub fn ndcg_at_k(ranked: &[String], judgments: &BTreeMap<String, u8>, k: usize) -> f64 {
    let dcg: f64 = ranked
        .iter()
        .take(k)
        .enumerate()
        .map(|(index, note_id)| {
            let grade = judgments.get(note_id).copied().unwrap_or(0);
            gain(grade) / ((index + 2) as f64).log2()
        })
        .sum();

    let mut ideal: Vec<u8> = judgments.values().copied().collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let idcg: f64 = ideal
        .into_iter()
        .take(k)
        .enumerate()
        .map(|(index, grade)| gain(grade) / ((index + 2) as f64).log2())
        .sum();

    if idcg == 0.0 { 0.0 } else { dcg / idcg }
}

/// Recall@`k` over judged-relevant (`grade > 0`) notes.
pub fn recall_at_k(ranked: &[String], judgments: &BTreeMap<String, u8>, k: usize) -> f64 {
    let relevant: BTreeSet<&str> = judgments
        .iter()
        .filter(|(_, grade)| **grade > 0)
        .map(|(note_id, _)| note_id.as_str())
        .collect();
    if relevant.is_empty() {
        return 0.0;
    }
    let hits = ranked
        .iter()
        .take(k)
        .filter(|note_id| relevant.contains(note_id.as_str()))
        .count();
    hits as f64 / relevant.len() as f64
}

/// MRR@`k`: reciprocal rank of the first judged-relevant note, else 0.
pub fn mrr_at_k(ranked: &[String], judgments: &BTreeMap<String, u8>, k: usize) -> f64 {
    ranked
        .iter()
        .take(k)
        .position(|note_id| judgments.get(note_id).copied().unwrap_or(0) > 0)
        .map_or(0.0, |index| 1.0 / (index + 1) as f64)
}

fn macro_average(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

// ── Deterministic replay adapter ───────────────────────────────────────────

/// One case's candidate-run outputs.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseRun {
    pub case_id: String,
    /// The full fused order handed to packing (at most the candidate window).
    pub fused_note_ids: Vec<String>,
    /// The ordered note IDs that were actually injected.
    pub injected_note_ids: Vec<String>,
    /// One terminal disposition per fused candidate, in fused order.
    pub dispositions: Vec<(String, NotePackDisposition)>,
    pub bytes_packed: usize,
}

fn note_from_candidate(candidate: &FixtureCandidate) -> djinn_memory::Note {
    djinn_memory::Note {
        id: candidate.note_id.clone(),
        project_id: "fixture".to_owned(),
        permalink: candidate.permalink.clone(),
        title: candidate.title.clone(),
        file_path: String::new(),
        storage: "db".to_owned(),
        note_type: candidate.note_type.clone(),
        folder: candidate
            .permalink
            .split('/')
            .next()
            .unwrap_or("")
            .to_owned(),
        status: "active".to_owned(),
        tags: "[]".to_owned(),
        content: candidate.content.clone(),
        retrieval_anchor: None,
        created_at: "1970-01-01T00:00:00.000Z".to_owned(),
        updated_at: "1970-01-01T00:00:00.000Z".to_owned(),
        lifecycle_changed_at: None,
        last_accessed: "1970-01-01T00:00:00.000Z".to_owned(),
        access_count: 0,
        confidence: candidate.confidence,
        abstract_: candidate.abstract_.clone(),
        overview: None,
        scope_paths: serde_json::to_string(&candidate.scope_paths).unwrap_or_else(|_| "[]".into()),
    }
}

/// Replay one case through the real fusion and the real packer.
///
/// Captured per-signal lists are converted back into `(id, score)` pairs whose
/// descending score reproduces the captured order exactly; the ranked-scope
/// list is recomputed from the pinned scope paths by the same function
/// production uses. Nothing here re-implements ranking.
pub fn run_case(case: &InjectionRankingCase, manifest: &InjectionRankingManifest) -> CaseRun {
    let rrf_k = injection_rrf_k(manifest.cutoff);
    let mut signals: Vec<(Vec<(String, f64)>, f64)> = Vec::with_capacity(SIGNAL_ORDER.len() + 1);
    for name in SIGNAL_ORDER {
        let list = case
            .signals
            .get(name)
            .map(|ids| {
                let len = ids.len();
                ids.iter()
                    .enumerate()
                    .map(|(index, id)| (id.clone(), (len - index) as f64))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        signals.push((list, rrf_k));
    }

    let scope_candidates: Vec<ScopeCandidate> = case
        .candidates
        .iter()
        .map(|candidate| ScopeCandidate {
            note_id: candidate.note_id.clone(),
            scope_paths: candidate.scope_paths.clone(),
        })
        .collect();
    signals.push((
        rank_scope_candidates(
            &case.expected_scope_paths,
            &scope_candidates,
            manifest.candidate_window,
        ),
        rrf_k,
    ));

    let confidence_map: std::collections::HashMap<String, f64> = case
        .candidates
        .iter()
        .map(|candidate| (candidate.note_id.clone(), candidate.confidence))
        .collect();

    let by_id: BTreeMap<&str, &FixtureCandidate> = case
        .candidates
        .iter()
        .map(|candidate| (candidate.note_id.as_str(), candidate))
        .collect();

    let fused = rrf_fuse_with_profile(
        &signals,
        &confidence_map,
        RankingProfile::KnowledgeInjectionV1,
    );
    let ordered: Vec<&FixtureCandidate> = fused
        .into_iter()
        .filter_map(|(id, _)| by_id.get(id.as_str()).copied())
        .take(manifest.candidate_window)
        .collect();

    let notes: Vec<djinn_memory::Note> = ordered.iter().map(|c| note_from_candidate(c)).collect();
    let packed = pack_ranked_knowledge_notes(
        &notes,
        KnowledgePackConfig {
            minimum_confidence: manifest.minimum_confidence,
            top_k: manifest.cutoff,
            total_byte_budget: manifest.prompt_byte_budget,
            line_byte_cap: manifest.line_byte_cap,
        },
    );

    let dispositions: Vec<(String, NotePackDisposition)> = notes
        .iter()
        .zip(&packed.outcomes)
        .map(|(note, outcome)| (note.id.clone(), outcome.disposition.clone()))
        .collect();
    let injected_note_ids = dispositions
        .iter()
        .filter(|(_, disposition)| *disposition == NotePackDisposition::Injected)
        .map(|(id, _)| id.clone())
        .collect();

    CaseRun {
        case_id: case.case_id.clone(),
        fused_note_ids: notes.iter().map(|note| note.id.clone()).collect(),
        injected_note_ids,
        dispositions,
        bytes_packed: packed.total_injected_chars,
    }
}

// ── Report ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct MetricSet {
    pub ndcg_at_10: f64,
    pub recall_at_10: f64,
    pub mrr_at_10: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct InjectionRankingReport {
    pub schema_version: String,
    pub case_count: usize,
    pub repository_scope_count: usize,
    pub baseline: MetricSet,
    pub candidate: MetricSet,
    pub ndcg_delta: f64,
    pub recall_drop: f64,
    pub result_set_overlap: f64,
    pub type_mix: BTreeMap<String, usize>,
    pub total_bytes_packed: usize,
    pub disposition_totals: BTreeMap<String, usize>,
}

fn disposition_key(disposition: &NotePackDisposition) -> &'static str {
    match disposition {
        NotePackDisposition::ConfidenceFiltered => "confidence_filtered",
        NotePackDisposition::NotTopK => "not_top_k",
        NotePackDisposition::OversizedSkipped => "oversized_skipped",
        NotePackDisposition::Injected => "injected",
        NotePackDisposition::BudgetPruned => "budget_pruned",
    }
}

/// Thresholds supplied by the acceptance command.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub repeat: usize,
    pub require_ndcg_delta: f64,
    pub max_recall_drop: f64,
}

/// Run the full evaluation and enforce every gate.
///
/// Returns `Err` — and therefore a non-zero exit — on any oracle-integrity
/// failure, an nDCG improvement below `require_ndcg_delta`, a Recall drop above
/// `max_recall_drop`, any repeated ordering/disposition mismatch, a packed
/// result above the manifest byte ceiling, or a no-backfill violation.
pub fn evaluate(
    corpus: &LoadedCorpus,
    thresholds: Thresholds,
) -> EvalResult<InjectionRankingReport> {
    if thresholds.repeat == 0 {
        return Err(EvalError::Contract(
            "--repeat must be at least 1".to_owned(),
        ));
    }

    let baseline_by_case: BTreeMap<&str, &BaselineCase> = corpus
        .baseline
        .cases
        .iter()
        .map(|case| (case.case_id.as_str(), case))
        .collect();

    let mut first_runs: Vec<CaseRun> = Vec::with_capacity(corpus.cases.len());
    for case in &corpus.cases {
        let run = run_case(case, &corpus.manifest);
        // Determinism: repeated runs must agree on ordering *and* dispositions.
        for repeat in 1..thresholds.repeat {
            let again = run_case(case, &corpus.manifest);
            if again.fused_note_ids != run.fused_note_ids {
                return Err(EvalError::Nondeterministic(format!(
                    "case {} produced a different fused order on repeat {}",
                    case.case_id,
                    repeat + 1
                )));
            }
            if again.dispositions != run.dispositions {
                return Err(EvalError::Nondeterministic(format!(
                    "case {} produced different dispositions on repeat {}",
                    case.case_id,
                    repeat + 1
                )));
            }
            if again.bytes_packed != run.bytes_packed {
                return Err(EvalError::Nondeterministic(format!(
                    "case {} packed a different byte count on repeat {}",
                    case.case_id,
                    repeat + 1
                )));
            }
        }
        if run.bytes_packed > corpus.manifest.prompt_byte_budget {
            return Err(EvalError::ByteCeilingExceeded {
                case_id: case.case_id.clone(),
                bytes: run.bytes_packed,
                ceiling: corpus.manifest.prompt_byte_budget,
            });
        }
        check_no_backfill(case, &run, corpus.manifest.cutoff)?;
        first_runs.push(run);
    }

    let mut baseline_ndcg = Vec::new();
    let mut baseline_recall = Vec::new();
    let mut baseline_mrr = Vec::new();
    let mut candidate_ndcg = Vec::new();
    let mut candidate_recall = Vec::new();
    let mut candidate_mrr = Vec::new();
    let mut overlaps = Vec::new();
    let mut type_mix: BTreeMap<String, usize> = BTreeMap::new();
    let mut disposition_totals: BTreeMap<String, usize> = BTreeMap::new();
    let mut total_bytes = 0usize;

    for (case, run) in corpus.cases.iter().zip(&first_runs) {
        let baseline_case = baseline_by_case
            .get(case.case_id.as_str())
            .expect("validated above");
        let baseline_ids = &baseline_case.ordered_note_ids;

        baseline_ndcg.push(ndcg_at_k(baseline_ids, &case.judgments, REQUIRED_CUTOFF));
        baseline_recall.push(recall_at_k(baseline_ids, &case.judgments, REQUIRED_CUTOFF));
        baseline_mrr.push(mrr_at_k(baseline_ids, &case.judgments, REQUIRED_CUTOFF));

        candidate_ndcg.push(ndcg_at_k(
            &run.injected_note_ids,
            &case.judgments,
            REQUIRED_CUTOFF,
        ));
        candidate_recall.push(recall_at_k(
            &run.injected_note_ids,
            &case.judgments,
            REQUIRED_CUTOFF,
        ));
        candidate_mrr.push(mrr_at_k(
            &run.injected_note_ids,
            &case.judgments,
            REQUIRED_CUTOFF,
        ));

        let baseline_set: BTreeSet<&str> = baseline_ids
            .iter()
            .take(REQUIRED_CUTOFF)
            .map(String::as_str)
            .collect();
        let candidate_set: BTreeSet<&str> = run
            .injected_note_ids
            .iter()
            .take(REQUIRED_CUTOFF)
            .map(String::as_str)
            .collect();
        let union = baseline_set.union(&candidate_set).count();
        overlaps.push(if union == 0 {
            1.0
        } else {
            baseline_set.intersection(&candidate_set).count() as f64 / union as f64
        });

        let by_id: BTreeMap<&str, &FixtureCandidate> = case
            .candidates
            .iter()
            .map(|candidate| (candidate.note_id.as_str(), candidate))
            .collect();
        for note_id in &run.injected_note_ids {
            if let Some(candidate) = by_id.get(note_id.as_str()) {
                *type_mix.entry(candidate.note_type.clone()).or_default() += 1;
            }
        }
        for (_, disposition) in &run.dispositions {
            *disposition_totals
                .entry(disposition_key(disposition).to_owned())
                .or_default() += 1;
        }
        total_bytes += run.bytes_packed;
    }

    let baseline_metrics = MetricSet {
        ndcg_at_10: macro_average(&baseline_ndcg),
        recall_at_10: macro_average(&baseline_recall),
        mrr_at_10: macro_average(&baseline_mrr),
    };
    let candidate_metrics = MetricSet {
        ndcg_at_10: macro_average(&candidate_ndcg),
        recall_at_10: macro_average(&candidate_recall),
        mrr_at_10: macro_average(&candidate_mrr),
    };
    let ndcg_delta = candidate_metrics.ndcg_at_10 - baseline_metrics.ndcg_at_10;
    let recall_drop = baseline_metrics.recall_at_10 - candidate_metrics.recall_at_10;

    let scopes: BTreeSet<&str> = corpus
        .cases
        .iter()
        .map(|case| case.repository_scope.as_str())
        .collect();

    let report = InjectionRankingReport {
        schema_version: SCHEMA_VERSION.to_owned(),
        case_count: corpus.cases.len(),
        repository_scope_count: scopes.len(),
        baseline: baseline_metrics,
        candidate: candidate_metrics,
        ndcg_delta,
        recall_drop,
        result_set_overlap: macro_average(&overlaps),
        type_mix,
        total_bytes_packed: total_bytes,
        disposition_totals,
    };

    if ndcg_delta < thresholds.require_ndcg_delta {
        return Err(EvalError::NdcgBelowThreshold {
            delta: ndcg_delta,
            required: thresholds.require_ndcg_delta,
        });
    }
    if recall_drop > thresholds.max_recall_drop {
        return Err(EvalError::RecallDropTooLarge {
            drop: recall_drop,
            allowed: thresholds.max_recall_drop,
        });
    }

    Ok(report)
}

/// Enforce the no-backfill contract on one replayed case.
///
/// Every fused candidate must carry exactly one terminal disposition, and no
/// candidate at or beyond the `top_k`-th confidence-eligible position may be
/// injected — an oversized or budget-pruned note inside top-k never promotes a
/// later candidate.
fn check_no_backfill(case: &InjectionRankingCase, run: &CaseRun, top_k: usize) -> EvalResult<()> {
    if run.dispositions.len() != run.fused_note_ids.len() {
        return Err(EvalError::NoBackfillRegression(format!(
            "case {}: {} candidates but {} dispositions",
            case.case_id,
            run.fused_note_ids.len(),
            run.dispositions.len()
        )));
    }
    let mut eligible_rank = 0usize;
    for (note_id, disposition) in &run.dispositions {
        if *disposition == NotePackDisposition::ConfidenceFiltered {
            continue;
        }
        if eligible_rank >= top_k && *disposition != NotePackDisposition::NotTopK {
            return Err(EvalError::NoBackfillRegression(format!(
                "case {}: note {note_id} at eligible rank {eligible_rank} is beyond top-{top_k} \
                 but was dispositioned {:?}; backfill must never occur",
                case.case_id, disposition
            )));
        }
        eligible_rank += 1;
    }
    Ok(())
}

/// Resolve the manifest path, defaulting to the git-ignored corpus directory.
pub fn default_manifest_path(crate_root: &Path) -> PathBuf {
    crate_root
        .join(DEFAULT_CORPUS_DIR)
        .join("injection-ranking-v1.manifest.json")
}

/// The `injection-ranking` acceptance command.
///
/// Exits non-zero (by returning `Err`) on any oracle-integrity failure, an
/// insufficient nDCG improvement, an excessive Recall drop, repeated
/// ordering/disposition nondeterminism, a byte-ceiling violation, or a
/// no-backfill regression.
pub fn cmd_injection_ranking(
    crate_root: &Path,
    manifest: Option<PathBuf>,
    thresholds: Thresholds,
) -> anyhow::Result<()> {
    let manifest_path = manifest.unwrap_or_else(|| default_manifest_path(crate_root));
    if !manifest_path.exists() {
        anyhow::bail!(
            "no injection-ranking manifest at {}.\n\
             The judged corpus is intentionally NOT committed to this repository: judgments, \
             trace IDs, and captured baselines are per-deployment empirical data. Supply your \
             own corpus with --manifest, or place it at the git-ignored default \
             `{DEFAULT_CORPUS_DIR}/`.",
            manifest_path.display()
        );
    }
    let corpus = load_corpus(&manifest_path)?;
    let report = evaluate(&corpus, thresholds)?;
    // Reported through `tracing`, matching every other command in this crate.
    tracing::info!(
        report = %serde_json::to_string_pretty(&report)?,
        "=== Injection ranking evaluation ==="
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every identifier below is invented for this test. Nothing here is
    /// captured from any deployment, and this is not an acceptance corpus.
    fn synthetic_candidate(
        note_id: &str,
        scope_paths: &[&str],
        grade_hint: &str,
    ) -> FixtureCandidate {
        FixtureCandidate {
            note_id: note_id.to_owned(),
            note_type: "pitfall".to_owned(),
            permalink: format!("pitfalls/{note_id}"),
            title: format!("synthetic {note_id}"),
            scope_paths: scope_paths.iter().map(|p| (*p).to_owned()).collect(),
            content: format!("synthetic body for {grade_hint}"),
            abstract_: Some(format!("abstract for {note_id}")),
            confidence: 1.0,
        }
    }

    fn synthetic_manifest() -> InjectionRankingManifest {
        InjectionRankingManifest {
            schema_version: SCHEMA_VERSION.to_owned(),
            fixture_path: "fixture.jsonl".to_owned(),
            baseline_path: "baseline.json".to_owned(),
            fixture_sha256: String::new(),
            baseline_sha256: String::new(),
            pinned_vector_sha256: "0".repeat(64),
            baseline_commit: "0".repeat(40),
            cutoff: REQUIRED_CUTOFF,
            candidate_window: KNOWLEDGE_INJECTION_CANDIDATE_WINDOW,
            prompt_byte_budget: 8192,
            minimum_confidence: 0.3,
            line_byte_cap: 512,
            cases: Vec::new(),
            extra: BTreeMap::new(),
        }
    }

    fn synthetic_case() -> InjectionRankingCase {
        let mut judgments = BTreeMap::new();
        judgments.insert("note-exact".to_owned(), 3u8);
        judgments.insert("note-near".to_owned(), 1u8);
        judgments.insert("note-far".to_owned(), 0u8);
        InjectionRankingCase {
            case_id: "case-0001".to_owned(),
            repository_scope: "scope-a".to_owned(),
            task: FixtureTask {
                task_id: "synthetic-task".to_owned(),
                title: "synthetic".to_owned(),
                description: "synthetic".to_owned(),
                acceptance_criteria: vec![],
            },
            base_commit: "0".repeat(40),
            expected_scope_paths: vec!["alpha/src/lib.rs".to_owned()],
            expected_scope_fallback_reason: None,
            candidates: vec![
                synthetic_candidate("note-exact", &["alpha/src/lib.rs"], "exact"),
                synthetic_candidate("note-near", &["alpha/src"], "near"),
                synthetic_candidate("note-far", &["beta"], "far"),
            ],
            signals: BTreeMap::new(),
            judgments,
        }
    }

    #[test]
    fn sha256_hex_is_the_standard_digest() {
        // Known-answer test: SHA-256 of the empty input.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn candidate_snapshot_hash_is_order_independent_but_content_sensitive() {
        let case = synthetic_case();
        let forward = candidate_snapshot_sha256(&case.candidates).unwrap();
        let mut reversed = case.candidates.clone();
        reversed.reverse();
        assert_eq!(forward, candidate_snapshot_sha256(&reversed).unwrap());

        let mut mutated = case.candidates.clone();
        mutated[0].content.push('!');
        assert_ne!(
            forward,
            candidate_snapshot_sha256(&mutated).unwrap(),
            "a content edit must change the snapshot hash"
        );
    }

    #[test]
    fn ndcg_rewards_placing_the_best_note_first() {
        let mut judgments = BTreeMap::new();
        judgments.insert("a".to_owned(), 3u8);
        judgments.insert("b".to_owned(), 1u8);
        let best = vec!["a".to_owned(), "b".to_owned()];
        let worst = vec!["b".to_owned(), "a".to_owned()];
        assert_eq!(ndcg_at_k(&best, &judgments, 10), 1.0);
        assert!(ndcg_at_k(&worst, &judgments, 10) < 1.0);
        assert!(ndcg_at_k(&[], &judgments, 10) == 0.0);
    }

    #[test]
    fn ndcg_uses_exponential_gain_and_log2_discount() {
        // One relevant note at rank 2 with grade 2: DCG = (2^2-1)/log2(3),
        // IDCG = (2^2-1)/log2(2) = 3. The ratio is 1/log2(3).
        let mut judgments = BTreeMap::new();
        judgments.insert("hit".to_owned(), 2u8);
        let ranked = vec!["miss".to_owned(), "hit".to_owned()];
        let expected = 1.0 / 3f64.log2();
        assert!((ndcg_at_k(&ranked, &judgments, 10) - expected).abs() < 1e-12);
    }

    #[test]
    fn recall_and_mrr_count_only_positive_grades() {
        let mut judgments = BTreeMap::new();
        judgments.insert("a".to_owned(), 0u8);
        judgments.insert("b".to_owned(), 2u8);
        judgments.insert("c".to_owned(), 1u8);
        let ranked = vec!["a".to_owned(), "b".to_owned()];
        assert_eq!(recall_at_k(&ranked, &judgments, 10), 0.5);
        assert_eq!(mrr_at_k(&ranked, &judgments, 10), 0.5);
        // `a` is judged but not relevant, so it never contributes.
        assert_eq!(mrr_at_k(&["a".to_owned()], &judgments, 10), 0.0);
    }

    #[test]
    fn recall_at_k_respects_the_cutoff() {
        let mut judgments = BTreeMap::new();
        judgments.insert("hit".to_owned(), 3u8);
        let ranked: Vec<String> = (0..15)
            .map(|index| {
                if index == 12 {
                    "hit".to_owned()
                } else {
                    format!("filler-{index}")
                }
            })
            .collect();
        assert_eq!(recall_at_k(&ranked, &judgments, 10), 0.0);
        assert_eq!(recall_at_k(&ranked, &judgments, 15), 1.0);
    }

    #[test]
    fn replay_ranks_the_exact_scope_match_first_and_is_deterministic() {
        let case = synthetic_case();
        let manifest = synthetic_manifest();
        let first = run_case(&case, &manifest);
        let second = run_case(&case, &manifest);
        assert_eq!(first, second, "replay must be deterministic");
        assert_eq!(
            first.fused_note_ids.first().map(String::as_str),
            Some("note-exact"),
            "the exact scope match must outrank the coarser one"
        );
        // `note-far` is not scope-comparable and has no other signal, so it
        // never enters the fused list at all.
        assert!(!first.fused_note_ids.iter().any(|id| id == "note-far"));
        assert_eq!(first.dispositions.len(), first.fused_note_ids.len());
    }

    #[test]
    fn replay_honours_captured_signal_order() {
        let mut case = synthetic_case();
        case.expected_scope_paths.clear();
        case.signals.insert(
            "lexical".to_owned(),
            vec!["note-far".to_owned(), "note-near".to_owned()],
        );
        let run = run_case(&case, &synthetic_manifest());
        assert_eq!(
            run.fused_note_ids,
            vec!["note-far".to_owned(), "note-near".to_owned()],
            "with only a lexical signal the captured order must survive fusion"
        );
    }

    #[test]
    fn manifest_recording_its_own_commit_is_rejected() {
        let mut manifest = synthetic_manifest();
        manifest.extra.insert(
            "manifest_commit".to_owned(),
            serde_json::Value::String("deadbeef".to_owned()),
        );
        let error = validate_manifest_shape(&manifest).expect_err("must be rejected");
        assert!(matches!(error, EvalError::Provenance(_)), "{error}");
    }

    #[test]
    fn manifest_contract_values_are_enforced() {
        let mut manifest = synthetic_manifest();
        manifest.cutoff = 5;
        assert!(matches!(
            validate_manifest_shape(&manifest),
            Err(EvalError::Contract(_))
        ));

        let mut manifest = synthetic_manifest();
        manifest.candidate_window = 20;
        assert!(matches!(
            validate_manifest_shape(&manifest),
            Err(EvalError::Contract(_))
        ));

        let mut manifest = synthetic_manifest();
        manifest.pinned_vector_sha256 = String::new();
        assert!(matches!(
            validate_manifest_shape(&manifest),
            Err(EvalError::Provenance(_))
        ));

        let mut manifest = synthetic_manifest();
        manifest.schema_version = "injection-ranking-v2".to_owned();
        assert!(matches!(
            validate_manifest_shape(&manifest),
            Err(EvalError::Contract(_))
        ));
    }

    #[test]
    fn missing_manifest_file_is_a_hard_failure() {
        let error = load_corpus(Path::new("/nonexistent/injection-ranking.manifest.json"))
            .expect_err("must fail");
        assert!(matches!(error, EvalError::Io { .. }), "{error}");
    }

    #[test]
    fn malformed_manifest_is_a_hard_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("manifest.json");
        std::fs::write(&path, b"{ not json").expect("write");
        let error = load_corpus(&path).expect_err("must fail");
        assert!(matches!(error, EvalError::Malformed { .. }), "{error}");
    }

    #[test]
    fn fixture_hash_mismatch_is_a_hard_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = dir.path().join("fixture.jsonl");
        let baseline = dir.path().join("baseline.json");
        std::fs::write(&fixture, b"{}\n").expect("write");
        std::fs::write(&baseline, b"{}").expect("write");

        let mut manifest = synthetic_manifest();
        // A hash that is syntactically fine but does not describe the bytes.
        manifest.fixture_sha256 = "a".repeat(64);
        manifest.baseline_sha256 = sha256_hex(b"{}");
        let path = dir.path().join("manifest.json");
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).expect("write");

        let error = load_corpus(&path).expect_err("must fail");
        assert!(matches!(error, EvalError::HashMismatch { .. }), "{error}");
    }

    #[test]
    fn zero_repeat_is_rejected() {
        let corpus = LoadedCorpus {
            manifest: synthetic_manifest(),
            cases: vec![],
            baseline: InjectionRankingBaseline {
                schema_version: SCHEMA_VERSION.to_owned(),
                captured_from_commit: "0".repeat(40),
                cases: vec![],
            },
        };
        let error = evaluate(
            &corpus,
            Thresholds {
                repeat: 0,
                require_ndcg_delta: 0.10,
                max_recall_drop: 0.02,
            },
        )
        .expect_err("must fail");
        assert!(matches!(error, EvalError::Contract(_)), "{error}");
    }

    #[test]
    fn insufficient_ndcg_improvement_fails_the_gate() {
        // One case whose replayed ordering equals the baseline ordering, so the
        // delta is exactly zero and the 0.10 requirement cannot be met.
        let case = synthetic_case();
        let manifest = synthetic_manifest();
        let run = run_case(&case, &manifest);
        let corpus = LoadedCorpus {
            manifest,
            baseline: InjectionRankingBaseline {
                schema_version: SCHEMA_VERSION.to_owned(),
                captured_from_commit: "0".repeat(40),
                cases: vec![BaselineCase {
                    case_id: case.case_id.clone(),
                    ordered_note_ids: run.injected_note_ids.clone(),
                    bytes_packed: run.bytes_packed,
                }],
            },
            cases: vec![case],
        };
        let error = evaluate(
            &corpus,
            Thresholds {
                repeat: 2,
                require_ndcg_delta: 0.10,
                max_recall_drop: 0.02,
            },
        )
        .expect_err("identical orderings cannot clear a +0.10 requirement");
        assert!(
            matches!(error, EvalError::NdcgBelowThreshold { .. }),
            "{error}"
        );
    }

    #[test]
    fn excessive_recall_drop_fails_the_gate() {
        // The baseline retrieved the relevant note; the "candidate" ordering
        // here is engineered to lose it, so Recall drops by 1.0.
        let mut case = synthetic_case();
        // Remove every signal so nothing is retrieved at all.
        case.expected_scope_paths.clear();
        case.signals.clear();
        let manifest = synthetic_manifest();
        let corpus = LoadedCorpus {
            baseline: InjectionRankingBaseline {
                schema_version: SCHEMA_VERSION.to_owned(),
                captured_from_commit: "0".repeat(40),
                cases: vec![BaselineCase {
                    case_id: case.case_id.clone(),
                    ordered_note_ids: vec!["note-exact".to_owned(), "note-near".to_owned()],
                    bytes_packed: 0,
                }],
            },
            manifest,
            cases: vec![case],
        };
        let error = evaluate(
            &corpus,
            Thresholds {
                repeat: 2,
                // Deliberately trivial so the nDCG gate cannot mask the Recall
                // gate we are actually testing.
                require_ndcg_delta: -10.0,
                max_recall_drop: 0.02,
            },
        )
        .expect_err("losing every relevant note must fail the recall gate");
        assert!(
            matches!(error, EvalError::RecallDropTooLarge { .. }),
            "{error}"
        );
    }
}
