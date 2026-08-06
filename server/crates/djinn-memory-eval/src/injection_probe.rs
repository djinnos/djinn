//! Session-start injection probe: `query_by_scope_overlap` → `pack_ranked_knowledge_notes`.
//!
//! Phase 1 metrics and the Phase 2 QA run both stop at *ranking*. This probe
//! closes the remaining gap for proposal `u46i` by exercising the production
//! session-start retrieval entry point — `NoteRepository::query_by_scope_overlap`
//! — and rendering its result through the same
//! [`djinn_slot::helpers::pack_ranked_knowledge_notes`] the slot lifecycle
//! calls, so assertions can be made against the **final packed prompt text**
//! that an agent actually receives, not merely against repository ranking.
//!
//! # Determinism
//!
//! No LLM calls. Fixtures are loaded into an isolated in-memory database and
//! packed under the shipped `KnowledgeInjectionConfig::DEFAULT_*` settings.

use anyhow::{Context, Result};

use djinn_core::models::settings::KnowledgeInjectionConfig;
use djinn_db::database::Database;
use djinn_db::repositories::note::NoteRepository;
use djinn_slot::helpers::{
    ActionExcerptDetail, KnowledgePackConfig, NotePackDisposition, pack_ranked_knowledge_notes,
};

use crate::fixtures::Phase1Fixtures;
use crate::loader;

/// Minimum confidence used by the session-start knowledge query
/// (`djinn-agent/src/actors/slot/lifecycle/prompt_context.rs`).
pub const KNOWLEDGE_MIN_CONFIDENCE: f64 = 0.3;

/// Note types selected by the session-start knowledge query.
pub const KNOWLEDGE_NOTE_TYPES: &[&str] = &["pattern", "pitfall", "case"];

/// One candidate's terminal packing outcome, flattened for reporting.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct InjectionProbeCandidate {
    pub permalink: String,
    pub title: String,
    pub disposition: NotePackDisposition,
    pub action_excerpt: Option<ActionExcerptDetail>,
    pub rendered_bytes: Option<usize>,
}

/// Result of one probe run.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct InjectionProbeOutput {
    /// Task scope paths the scope-overlap query was run with.
    pub task_paths: Vec<String>,
    /// Notes returned by `query_by_scope_overlap`, in rank order.
    pub retrieved_permalinks: Vec<String>,
    /// The final packed prompt text delivered to the agent.
    pub packed_prompt: String,
    /// Terminal outcome per candidate, one per retrieved note.
    pub candidates: Vec<InjectionProbeCandidate>,
    /// Exact injected byte count.
    pub injected_bytes: usize,
    /// Effective packing configuration for this run.
    pub total_byte_budget: usize,
    pub line_byte_cap: usize,
    pub top_k: usize,
}

/// The shipped default injection configuration, expressed as a pack config.
pub fn default_pack_config() -> KnowledgePackConfig {
    KnowledgePackConfig {
        minimum_confidence: KNOWLEDGE_MIN_CONFIDENCE,
        top_k: KnowledgeInjectionConfig::DEFAULT_KNOWLEDGE_INJECTION_LIMIT as usize,
        total_byte_budget: KnowledgeInjectionConfig::DEFAULT_KNOWLEDGE_INJECTION_BUDGET_BYTES
            as usize,
        line_byte_cap: KnowledgeInjectionConfig::DEFAULT_KNOWLEDGE_INJECTION_LINE_CAP_BYTES
            as usize,
    }
}

/// Run the probe against pre-loaded fixtures under an explicit pack config.
pub async fn execute_injection_probe_with_config(
    fixtures: &Phase1Fixtures,
    task_paths: &[String],
    config: KnowledgePackConfig,
) -> Result<InjectionProbeOutput> {
    let db = Database::open_in_memory().context("opening isolated probe database")?;
    let state = loader::load_fixtures(&db, fixtures)
        .await
        .context("loading fixtures into database")?;
    let repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());

    // Production session-start retrieval entry point.
    let notes = repo
        .query_by_scope_overlap(
            &state.project.id,
            task_paths,
            KNOWLEDGE_NOTE_TYPES,
            KNOWLEDGE_MIN_CONFIDENCE,
            config.top_k,
        )
        .await
        .context("executing NoteRepository::query_by_scope_overlap")?;

    // Production prompt packing — the same call the slot lifecycle makes.
    let packed = pack_ranked_knowledge_notes(&notes, config);

    Ok(InjectionProbeOutput {
        task_paths: task_paths.to_vec(),
        retrieved_permalinks: notes.iter().map(|note| note.permalink.clone()).collect(),
        packed_prompt: packed.rendered,
        candidates: packed
            .outcomes
            .iter()
            .map(|outcome| InjectionProbeCandidate {
                permalink: outcome.permalink.clone(),
                title: outcome.title.clone(),
                disposition: outcome.disposition.clone(),
                action_excerpt: outcome.action_excerpt,
                rendered_bytes: outcome.estimated_rendered_chars,
            })
            .collect(),
        injected_bytes: packed.total_injected_chars,
        total_byte_budget: config.total_byte_budget,
        line_byte_cap: config.line_byte_cap,
        top_k: config.top_k,
    })
}

/// Run the probe against pre-loaded fixtures under the shipped defaults.
pub async fn execute_injection_probe(
    fixtures: &Phase1Fixtures,
    task_paths: &[String],
) -> Result<InjectionProbeOutput> {
    execute_injection_probe_with_config(fixtures, task_paths, default_pack_config()).await
}

/// Run the probe against the committed fixtures on disk.
pub async fn execute_injection_probe_from_disk(
    crate_root: &std::path::Path,
    task_paths: &[String],
) -> Result<InjectionProbeOutput> {
    let fixtures =
        loader::load_fixtures_from_disk(crate_root).context("loading fixtures from disk")?;
    execute_injection_probe(&fixtures, task_paths).await
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use crate::fixtures::{CorpusNoteRow, LifecycleTimestamps};

    /// Applicability anchor of the production tool-schema pitfall.
    const TOOL_SCHEMA_ANCHOR: &str = "Editing MCP tool schemas/param descriptions in the djinn repo, or the merge-queue suite fails on schema snapshot/golden mismatches.";

    const TOOL_SCHEMA_PERMALINK: &str = "pitfalls/tool-schema-edits-must-regenerate-all-derived-goldens-or-the-merge-queue-breaks-for-everyone";

    /// The production note's body, byte for byte, with ONE structural edit: a
    /// `## Prevention` heading before its regeneration block and a `## Notes`
    /// heading after it. The file's first line says so. The five bullets are
    /// byte-identical to the real note.
    ///
    /// Deliberately shared with the `djinn-slot` unit tests by path rather than
    /// copied, so the eval and the renderer can never drift onto different
    /// bytes. See `djinn-slot/src/helpers/code_context_action_tests.rs`, which
    /// also pins the *un*edited body and asserts it yields no excerpt.
    const TOOL_SCHEMA_REAUTHORED_BODY: &str =
        include_str!("../../djinn-slot/src/helpers/fixtures/tool_schema_note_reauthored.md");

    /// The three regeneration commands the proposal's objective names, as
    /// complete source lines, verbatim — including the test filter on the
    /// second, without which the command does not do the thing.
    const SERVER_SNAP_COMMAND: &str = "- Server insta snap: `INSTA_UPDATE=always cargo test --all-features tool_schemas` (in `server/`)";
    const CORPUS_FIXTURE_COMMAND: &str = "- Corpus fixture: `UPDATE_DJINN_MCP_SERVER_FIXTURE=1 cargo test -p djinn-control-plane --lib server_tests::tests::djinn_mcp_server_corpus_fixture_is_current` → writes `crates/djinn-provider/tests/fixtures/tool_schema_projection/builtin/djinn_mcp_server.json` (fails CI as \"Server Test shard\" — easy to miss because it is NOT named a schema check)";
    const UI_TYPES_COMMAND: &str = "- UI types: `pnpm mcp:types:snapshot` (in `ui/`; reads the server insta snap, so regenerate that FIRST)";

    fn tool_schema_note() -> CorpusNoteRow {
        CorpusNoteRow {
            permalink: TOOL_SCHEMA_PERMALINK.to_string(),
            title: "Tool-schema edits must regenerate ALL derived goldens or the merge queue breaks for everyone".to_string(),
            content: TOOL_SCHEMA_REAUTHORED_BODY.to_string(),
            note_type: "pitfall".to_string(),
            folder: "pitfalls".to_string(),
            status: "active".to_string(),
            tags: vec!["mcp-schema".to_string(), "goldens".to_string()],
            retrieval_anchor: Some(TOOL_SCHEMA_ANCHOR.to_string()),
            timestamps: LifecycleTimestamps {
                created_at: "2026-07-09T14:47:11.133Z".to_string(),
                updated_at: "2026-07-30T13:00:25.018Z".to_string(),
                last_accessed: "2026-08-01T10:00:00.000Z".to_string(),
            },
            confidence: 0.9,
            embedding: None,
            labels: vec![],
            graph_edges: vec![],
            expected_signals: Default::default(),
        }
    }

    /// A legacy note with no eligible action section and no anchor — it must
    /// still be injected, as a summary-only entry.
    fn legacy_note() -> CorpusNoteRow {
        CorpusNoteRow {
            permalink: "patterns/legacy-free-form".to_string(),
            title: "Legacy free-form pattern".to_string(),
            content: "A free-form body with no headings at all.".to_string(),
            note_type: "pattern".to_string(),
            folder: "patterns".to_string(),
            status: "active".to_string(),
            tags: vec![],
            retrieval_anchor: None,
            timestamps: LifecycleTimestamps {
                created_at: "2026-05-01T00:00:00.000Z".to_string(),
                updated_at: "2026-05-01T00:00:00.000Z".to_string(),
                last_accessed: "2026-05-01T00:00:00.000Z".to_string(),
            },
            confidence: 0.8,
            embedding: None,
            labels: vec![],
            graph_edges: vec![],
            expected_signals: Default::default(),
        }
    }

    fn probe_fixtures() -> Phase1Fixtures {
        Phase1Fixtures {
            corpus_notes: vec![tool_schema_note(), legacy_note()],
            memory_ref_queries: vec![],
            bad_cases: vec![],
            manifest: None,
        }
    }

    /// The probe must reach the production scope-overlap query and produce the
    /// packed prompt text, not just a ranking.
    #[tokio::test]
    async fn probe_reaches_scope_overlap_and_produces_packed_prompt_text() {
        let task_paths = vec!["server/crates/djinn-slot".to_string()];
        let output = execute_injection_probe(&probe_fixtures(), &task_paths)
            .await
            .expect("injection probe should succeed");

        assert!(
            output
                .retrieved_permalinks
                .contains(&TOOL_SCHEMA_PERMALINK.to_string()),
            "query_by_scope_overlap must return the tool-schema note: {:?}",
            output.retrieved_permalinks
        );
        assert!(
            !output.packed_prompt.is_empty(),
            "the probe must assert on packed prompt text, not ranking alone"
        );
        assert_eq!(
            output.candidates.len(),
            output.retrieved_permalinks.len(),
            "exactly one terminal outcome per candidate"
        );
        assert_eq!(output.injected_bytes, output.packed_prompt.len());
        assert_eq!(
            output.total_byte_budget, 8192,
            "the 8192-byte pack budget is preserved"
        );
        assert_eq!(output.top_k, 10, "the selected-note count is preserved");
    }

    /// Proposal u46i AC7/AC8 end-to-end: under the shipped defaults the final
    /// packed prompt carries the note's applicability anchor and all three
    /// complete regeneration command lines.
    #[tokio::test]
    async fn packed_prompt_carries_anchor_and_all_three_regeneration_commands() {
        let task_paths = vec!["server/crates/djinn-mcp-extension".to_string()];
        let output = execute_injection_probe(&probe_fixtures(), &task_paths)
            .await
            .expect("injection probe should succeed");
        let prompt = &output.packed_prompt;

        assert!(
            prompt.contains(TOOL_SCHEMA_ANCHOR),
            "applicability anchor missing from the packed prompt:\n{prompt}"
        );
        // R1: the permalink is the line's label, and the title is not rendered.
        assert!(
            prompt.contains(&format!("**[Pitfall] {TOOL_SCHEMA_PERMALINK}**: ")),
            "permalink label missing from the packed prompt:\n{prompt}"
        );

        // Each command must appear as a COMPLETE physical line — a `contains`
        // on a fragment would pass on a truncated command.
        let lines: Vec<&str> = prompt.split('\n').collect();
        for (label, command) in [
            ("INSTA_UPDATE", SERVER_SNAP_COMMAND),
            ("UPDATE_DJINN_MCP_SERVER_FIXTURE", CORPUS_FIXTURE_COMMAND),
            ("pnpm mcp:types:snapshot", UI_TYPES_COMMAND),
        ] {
            assert!(
                lines.contains(&format!("  action: {command}").as_str())
                    || lines.contains(&format!("          {command}").as_str()),
                "{label} command line missing or incomplete:\n{prompt}"
            );
        }
        // The filter that makes the corpus-fixture command actually assert.
        assert!(
            prompt.contains("server_tests::tests::djinn_mcp_server_corpus_fixture_is_current"),
            "the corpus-fixture command lost its test filter:\n{prompt}"
        );

        let tool_schema = output
            .candidates
            .iter()
            .find(|candidate| candidate.permalink == TOOL_SCHEMA_PERMALINK)
            .expect("the tool-schema candidate must be present");
        assert_eq!(tool_schema.disposition, NotePackDisposition::Injected);
        // The real section renders to 1097 B, over the 1024 B cap, so the tail
        // is dropped at a line boundary and the pull marker closes the block —
        // after all three commands have been delivered.
        assert_eq!(
            tool_schema.action_excerpt,
            Some(ActionExcerptDetail::Truncated),
            "the real section overflows the cap and must end in the pull marker"
        );
        // The pack holds more than one entry, so the marker is the last line of
        // *this* entry, not of the whole prompt.
        let marker = format!("  action: … truncated; memory_read({TOOL_SCHEMA_PERMALINK})");
        let marker_at = lines
            .iter()
            .position(|line| *line == marker)
            .expect("a truncated excerpt must carry the pull marker");
        let entry_start = lines
            .iter()
            .position(|line| line.contains(TOOL_SCHEMA_PERMALINK))
            .expect("the tool-schema entry must be present");
        assert!(
            marker_at > entry_start,
            "the marker must close the tool-schema entry:\n{prompt}"
        );
        assert!(
            lines[entry_start + 1..marker_at]
                .iter()
                .all(|line| line.starts_with("  action: ") || line.starts_with("          ")),
            "every line between the summary and the marker must be an action line:\n{prompt}"
        );

        // The byte contract holds on the final prompt text.
        for line in &lines {
            assert!(
                line.len() <= output.line_byte_cap,
                "physical line exceeds line_byte_cap: {line}"
            );
        }
        assert!(prompt.len() <= output.total_byte_budget);
    }

    /// A legacy note without an eligible section is never dropped: it is
    /// injected as a summary-only entry with no action trace detail.
    #[tokio::test]
    async fn legacy_note_without_a_section_is_injected_summary_only() {
        let task_paths = vec!["server/crates/djinn-slot".to_string()];
        let output = execute_injection_probe(&probe_fixtures(), &task_paths)
            .await
            .expect("injection probe should succeed");

        let legacy = output
            .candidates
            .iter()
            .find(|candidate| candidate.permalink == "patterns/legacy-free-form")
            .expect("the legacy candidate must be present");
        assert_eq!(legacy.disposition, NotePackDisposition::Injected);
        assert_eq!(legacy.action_excerpt, None);

        let legacy_line = output
            .packed_prompt
            .split('\n')
            .find(|line| line.contains("**[Pattern] patterns/legacy-free-form**: "))
            .expect("the legacy entry must appear in the packed prompt");
        assert!(
            legacy_line.contains("A free-form body with no headings at all."),
            "content fallback expected for an anchorless, abstractless note: {legacy_line}"
        );
    }

    /// Entry atomicity on the real packing path: a budget that cannot fit the
    /// tool-schema entry prunes it wholly and still injects the smaller one.
    #[tokio::test]
    async fn oversized_entry_is_pruned_wholly_and_a_smaller_entry_still_injects() {
        let task_paths = vec!["server/crates/djinn-slot".to_string()];
        let baseline = execute_injection_probe(&probe_fixtures(), &task_paths)
            .await
            .expect("injection probe should succeed");
        let tool_schema_bytes = baseline
            .candidates
            .iter()
            .find(|candidate| candidate.permalink == TOOL_SCHEMA_PERMALINK)
            .and_then(|candidate| candidate.rendered_bytes)
            .expect("the tool-schema entry must have been injected");

        let config = KnowledgePackConfig {
            total_byte_budget: tool_schema_bytes - 1,
            ..default_pack_config()
        };
        let output = execute_injection_probe_with_config(&probe_fixtures(), &task_paths, config)
            .await
            .expect("injection probe should succeed");

        let tool_schema = output
            .candidates
            .iter()
            .find(|candidate| candidate.permalink == TOOL_SCHEMA_PERMALINK)
            .expect("the tool-schema candidate must be present");
        assert_eq!(
            tool_schema.disposition,
            NotePackDisposition::BudgetPruned,
            "a non-fitting entry is wholly budget-pruned"
        );
        assert!(
            !output.packed_prompt.contains(TOOL_SCHEMA_ANCHOR),
            "no part of a pruned entry may reach the prompt:\n{}",
            output.packed_prompt
        );
        assert!(
            !output.packed_prompt.contains("INSTA_UPDATE=always"),
            "no action line of a pruned entry may reach the prompt:\n{}",
            output.packed_prompt
        );

        let legacy = output
            .candidates
            .iter()
            .find(|candidate| candidate.permalink == "patterns/legacy-free-form")
            .expect("the legacy candidate must be present");
        assert_eq!(
            legacy.disposition,
            NotePackDisposition::Injected,
            "packing must continue so a later smaller candidate is still injected"
        );
        assert!(
            output
                .packed_prompt
                .contains("**[Pattern] patterns/legacy-free-form**: "),
            "the smaller entry must appear in the packed prompt:\n{}",
            output.packed_prompt
        );
    }
}
