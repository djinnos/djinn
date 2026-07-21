// djinn:allow-oversize — MDX/block-patch module is intentionally larger than the
// size-guard threshold for this bootstrap step because the test suite for
// import/export/selector behavior is moved with the owning code. This module will
// be further split as the epic continues (feedback, signoff, lifecycle, etc.).
//! MDX frontmatter import/export parsing and block-patch selector resolution.
//!
//! This module owns the pure helpers shared by the server-side proposal tools
//! (`proposal_import`, `proposal_export`, `proposal_block_patch`) and the
//! in-pod agent dispatch path (`djinn-mcp-extension`), so both paths validate
//! selectors and resulting MDX identically and produce byte-identical
//! revisions.
//!
//! Native-skill provenance: `ProposalBlockPatchParams` carries
//! `native_skill_name` and `native_skill_version` fields that are persisted in
//! revision event_metadata. These fields are block-patch infrastructure (the
//! block patch is the surface through which native skills produce targeted
//! revisions), so they remain here rather than in a separate native-skill
//! module.

use djinn_spec_lint::analyze_mdx_document;
use rmcp::schemars;
use serde::Deserialize;
use serde_json::Value as JsonValue;

use crate::tools::validation::{
    resolve_body_format_and_validate, validate_ac_count, validate_design,
};

// ── MDX frontmatter parsing (import path) ───────────────────────────────────

#[derive(Debug)]
pub(crate) struct ImportedProposalMdx<'a> {
    pub(crate) id: Option<String>,
    pub(crate) title: String,
    pub(crate) body_format: String,
    pub(crate) body: &'a str,
    pub(crate) acceptance_criteria_json: String,
}

#[derive(Debug, Deserialize)]
struct ProposalMdxFrontmatter {
    id: Option<String>,
    title: Option<String>,
    body_format: Option<String>,
    acceptance_criteria: Option<JsonValue>,
}

pub(crate) fn split_proposal_mdx_frontmatter(mdx: &str) -> Result<(Option<&str>, &str), String> {
    let Some((rest, close, close_len)) = mdx
        .strip_prefix("---\n")
        .map(|rest| (rest, "\n---\n", 5usize))
        .or_else(|| {
            mdx.strip_prefix("---\r\n")
                .map(|rest| (rest, "\r\n---\r\n", 7usize))
        })
    else {
        return Ok((None, mdx));
    };

    if let Some(end) = rest.find(close) {
        return Ok((Some(&rest[..end]), &rest[end + close_len..]));
    }

    let terminal = close.trim_end_matches(['\n', '\r']);
    if let Some(frontmatter) = rest.strip_suffix(terminal) {
        return Ok((Some(frontmatter.trim_end_matches(['\n', '\r'])), ""));
    }

    Err("invalid proposal.mdx frontmatter: missing closing --- delimiter".to_string())
}

pub(crate) fn parse_proposal_mdx(mdx: &str) -> Result<ImportedProposalMdx<'_>, String> {
    let (frontmatter_raw, body) = split_proposal_mdx_frontmatter(mdx)?;
    let frontmatter = match frontmatter_raw {
        Some(raw) => serde_yaml::from_str::<ProposalMdxFrontmatter>(raw)
            .map_err(|e| format!("invalid proposal.mdx YAML frontmatter: {e}"))?,
        None => ProposalMdxFrontmatter {
            id: None,
            title: None,
            body_format: None,
            acceptance_criteria: None,
        },
    };

    let title = frontmatter
        .title
        .unwrap_or_else(|| "Imported proposal".to_string());
    if title.is_empty() {
        return Err("title must not be empty".to_string());
    }
    if title.len() > 200 {
        return Err(format!("title exceeds 200 chars (got {})", title.len()));
    }

    let body_format = frontmatter
        .body_format
        .unwrap_or_else(|| "markdown".to_string());
    if body_format != "markdown" && body_format != "mdx" {
        return Err(format!(
            "invalid body_format: {body_format:?} (allowed: markdown, mdx)"
        ));
    }

    let acceptance_criteria = frontmatter
        .acceptance_criteria
        .unwrap_or_else(|| JsonValue::Array(Vec::new()));
    let ac_len = acceptance_criteria
        .as_array()
        .ok_or_else(|| "acceptance_criteria must be an array".to_string())?
        .len();
    validate_ac_count(ac_len)?;
    let acceptance_criteria_json = serde_json::to_string(&acceptance_criteria)
        .map_err(|e| format!("invalid acceptance_criteria: {e}"))?;

    Ok(ImportedProposalMdx {
        id: frontmatter.id,
        title,
        body_format,
        body,
        acceptance_criteria_json,
    })
}

// ── Block-patch selectors ───────────────────────────────────────────────────

/// Selector for targeting a specific range in the proposal body.
///
/// Exactly one field must be provided. The selector identifies a deterministic
/// byte range in the current body that will be replaced or wrapped.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(description = "Selector for targeting a specific range in the proposal body")]
pub struct BlockPatchSelector {
    /// Match a markdown heading by its text (without the `#` prefix). The
    /// matched range includes the heading line itself and all content up to
    /// (but not including) the next heading at the same or higher level, or
    /// the end of the body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading_text: Option<String>,
    /// Match a contiguous substring of the body. Must occur exactly once;
    /// zero matches or ambiguous (multiple) matches are rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact_text: Option<String>,
    /// Byte range selector: start is inclusive, end is exclusive. If
    /// `expected_text` is provided and does not match the body at that byte
    /// range, the patch is rejected (stale-range guard).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_range: Option<ByteRangeSelector>,
}

/// A byte-range selector with an optional verification text.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ByteRangeSelector {
    /// Inclusive byte offset of the target range.
    pub start: i64,
    /// Exclusive byte offset of the target range.
    pub end: i64,
    /// When set, the body text at `[start..end)` must equal this value or the
    /// patch is rejected. Guards against stale ranges after concurrent edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_text: Option<String>,
}

/// Resolved byte range in the proposal body.
#[derive(Debug)]
pub(crate) struct ResolvedRange {
    start: usize,
    end: usize,
    /// Human-readable description for metadata.
    selector_description: String,
}

/// Resolve a [`BlockPatchSelector`] to a byte range in `body`.
fn resolve_selector(body: &str, selector: &BlockPatchSelector) -> Result<ResolvedRange, String> {
    let mut provided = 0usize;
    if selector.heading_text.is_some() {
        provided += 1;
    }
    if selector.exact_text.is_some() {
        provided += 1;
    }
    if selector.byte_range.is_some() {
        provided += 1;
    }
    if provided != 1 {
        return Err(
            "selector must specify exactly one of heading_text, exact_text, or byte_range".into(),
        );
    }

    if let Some(ref heading) = selector.heading_text {
        return resolve_heading_selector(body, heading);
    }
    if let Some(ref text) = selector.exact_text {
        return resolve_exact_text_selector(body, text);
    }
    if let Some(ref br) = selector.byte_range {
        return resolve_byte_range_selector(body, br);
    }
    unreachable!()
}

/// Match a markdown heading and its section content.
///
/// Scans for `^(#{1,6}) <heading_text>` (exact heading text match after `#`
/// prefix). The matched range is from the `#` character through the last byte
/// before the next heading at the same or higher level, or end-of-body.
fn resolve_heading_selector(body: &str, heading_text: &str) -> Result<ResolvedRange, String> {
    let needle = heading_text.trim();
    if needle.is_empty() {
        return Err("heading_text must not be empty".into());
    }

    let document =
        analyze_mdx_document(body).map_err(|error| format!("could not parse document: {error}"))?;
    let mut matches = Vec::new();
    for (index, node) in document.top_level_nodes.iter().enumerate() {
        let Some(level) = node.heading_level else {
            continue;
        };
        let text = atx_heading_text(&body[node.span.start..node.span.end], level);
        if text == needle {
            matches.push((index, level));
        }
    }

    if matches.is_empty() {
        return Err(format!("no heading matching {needle:?} found in body"));
    }
    if matches.len() > 1 {
        return Err(format!(
            "heading_text {needle:?} is ambiguous: found {} matches",
            matches.len()
        ));
    }

    let (heading_index, heading_level) = matches[0];
    let heading_start = document.top_level_nodes[heading_index].span.start;
    let mut section_end = body.len();
    for node in document.top_level_nodes.iter().skip(heading_index + 1) {
        if node
            .heading_level
            .is_some_and(|level| level <= heading_level)
        {
            section_end = node.span.start;
            break;
        }
    }

    Ok(ResolvedRange {
        start: heading_start,
        end: section_end,
        selector_description: format!("heading: {needle}"),
    })
}

/// Return visible text from an AST-confirmed ATX heading. A trailing `#` only
/// closes a heading when it is separated from its text by whitespace, so
/// `# C#` correctly selects the literal heading text `C#`.
fn atx_heading_text(source: &str, level: u8) -> &str {
    let after_prefix = source
        .get(level as usize..)
        .unwrap_or(source)
        .trim_start_matches([' ', '\t']);
    let text = after_prefix.trim_end_matches(['\r', '\n']);
    let closing_start = text.trim_end_matches('#').len();
    if closing_start < text.len()
        && text[..closing_start]
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_whitespace)
    {
        text[..closing_start].trim_end()
    } else {
        text.trim()
    }
}

/// Match an exact text substring. Must occur exactly once.
fn resolve_exact_text_selector(body: &str, text: &str) -> Result<ResolvedRange, String> {
    let needle = text;
    if needle.is_empty() {
        return Err("exact_text must not be empty".into());
    }
    let mut matches = Vec::new();
    let mut search_from = 0usize;
    while let Some(pos) = body[search_from..].find(needle) {
        matches.push(search_from + pos);
        search_from += pos + needle.len();
    }
    if matches.is_empty() {
        return Err("exact_text not found in body".into());
    }
    if matches.len() > 1 {
        return Err(format!(
            "exact_text is ambiguous: found {} matches",
            matches.len()
        ));
    }
    let start = matches[0];
    let end = start + needle.len();
    ensure_node_aligned(body, start, end)?;
    Ok(ResolvedRange {
        start,
        end,
        selector_description: format!("exact_text ({} bytes)", needle.len()),
    })
}

/// Match a byte range with optional verification.
fn resolve_byte_range_selector(
    body: &str,
    br: &ByteRangeSelector,
) -> Result<ResolvedRange, String> {
    if br.start < 0 {
        return Err("byte_range start must be non-negative".into());
    }
    if br.end < 0 {
        return Err("byte_range end must be non-negative".into());
    }
    let start = br.start as usize;
    let end = br.end as usize;
    if start >= body.len() {
        return Err(format!(
            "byte_range start {} is beyond body length {}",
            start,
            body.len()
        ));
    }
    if end > body.len() {
        return Err(format!(
            "byte_range end {} is beyond body length {}",
            end,
            body.len()
        ));
    }
    if start >= end {
        return Err("byte_range start must be less than end".into());
    }
    if !body.is_char_boundary(start) || !body.is_char_boundary(end) {
        return Err("byte_range start and end must be UTF-8 character boundaries".into());
    }
    if let Some(ref expected) = br.expected_text {
        let actual = &body[start..end];
        if actual != expected {
            return Err(format!(
                "byte_range text mismatch: expected {:?} but found {:?}",
                expected, actual
            ));
        }
    }
    ensure_node_aligned(body, start, end)?;
    Ok(ResolvedRange {
        start,
        end,
        selector_description: format!("byte_range[{}..{}]", start, end),
    })
}

const PATCH_RANGE_NOT_NODE_ALIGNED: &str = "PATCH_RANGE_NOT_NODE_ALIGNED";

fn ensure_node_aligned(body: &str, start: usize, end: usize) -> Result<(), String> {
    let document =
        analyze_mdx_document(body).map_err(|error| format!("could not parse document: {error}"))?;
    let aligned = document
        .top_level_nodes
        .iter()
        .any(|node| node.span.start == start)
        && document
            .top_level_nodes
            .iter()
            .any(|node| node.span.end == end);
    let patchable_property = document.registered_blocks.iter().any(|block| {
        block
            .patchable_property_spans
            .iter()
            .any(|span| span.start <= start && end <= span.end)
    });
    if aligned || patchable_property {
        Ok(())
    } else {
        Err(format!(
            "{PATCH_RANGE_NOT_NODE_ALIGNED}: exact_text and byte_range selectors must cover complete top-level AST nodes or one canonical code/template property"
        ))
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalBlockPatchParams {
    /// Proposal UUID or short_id.
    pub id: String,
    /// Target selector: identifies the range in the body to patch.
    pub selector: BlockPatchSelector,
    /// Operation: `replace` replaces the selected range, `wrap` wraps it.
    pub operation: String,
    /// The MDX content to insert (a single block or arbitrary MDX text).
    pub block_mdx: String,
    /// If set, reject the patch when the proposal's latest_revision_seq does
    /// not equal this value. Guards against concurrent edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_latest_revision_seq: Option<i32>,
    /// Name of the native skill producing this patch (e.g. "visual-spec").
    /// Persisted in the revision event_metadata for provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_skill_name: Option<String>,
    /// Pinned version of the native skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_skill_version: Option<String>,
    /// Optional free-form note persisted alongside the revision metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Pure outcome of applying a block patch to a proposal body — the new body,
/// its (possibly upgraded) `body_format`, and the revision `event_metadata`.
pub struct BlockPatchOutcome {
    pub new_body: String,
    pub new_body_format: &'static str,
    pub event_metadata: serde_json::Value,
}

/// Apply a single targeted MDX block patch to `existing_body`, returning the
/// new body, its body_format, and the revision metadata.
///
/// This is the pure transformation shared by the server-side
/// `proposal_block_patch` tool and the in-pod agent dispatch
/// (`djinn-mcp-extension`), so both paths validate selectors and resulting MDX
/// identically and produce byte-identical revisions. It does NOT resolve the
/// proposal, enforce the author edit-gate, guard stale revisions, or persist —
/// callers own those steps.
pub fn apply_block_patch(
    existing_body: &str,
    existing_body_format: &str,
    p: &ProposalBlockPatchParams,
) -> Result<BlockPatchOutcome, String> {
    if !matches!(p.operation.as_str(), "replace" | "wrap") {
        return Err(format!(
            "invalid operation: {:?} (expected replace or wrap)",
            p.operation
        ));
    }
    if p.block_mdx.is_empty() {
        return Err("block_mdx must not be empty".to_string());
    }

    let range =
        resolve_selector(existing_body, &p.selector).map_err(|e| format!("selector error: {e}"))?;

    let new_body = match p.operation.as_str() {
        "replace" => {
            let mut body = String::with_capacity(
                existing_body.len() - (range.end - range.start) + p.block_mdx.len(),
            );
            body.push_str(&existing_body[..range.start]);
            body.push_str(&p.block_mdx);
            body.push_str(&existing_body[range.end..]);
            body
        }
        "wrap" => {
            let selected = &existing_body[range.start..range.end];
            let mut body = String::with_capacity(
                existing_body.len() - (range.end - range.start)
                    + p.block_mdx.len()
                    + selected.len(),
            );
            body.push_str(&existing_body[..range.start]);
            body.push_str(&p.block_mdx);
            body.push_str(selected);
            body.push_str(&existing_body[range.end..]);
            body
        }
        _ => unreachable!(),
    };

    // Resolve the persisted body_format (upgrading markdown→mdx when the patch
    // introduces block tags) and run the shared MDX block-validation stack — the
    // same resolution + validation the create/update/import write paths use, so
    // all four paths reject unknown tags and empty children-based blocks
    // identically.
    let new_body_format = resolve_body_format_and_validate(&new_body, Some(existing_body_format))?;
    validate_design(&new_body)?;

    let mut event_metadata = serde_json::json!({
        "change_kind": "targeted_block_patch",
        "selector": range.selector_description,
        "range_start_byte": range.start,
        "range_end_byte": range.end,
        "native_skill_name": p.native_skill_name.as_deref().unwrap_or(""),
        "native_skill_version": p.native_skill_version.as_deref().unwrap_or(""),
    });
    if let Some(ref note) = p.note {
        event_metadata["note"] = serde_json::Value::String(note.clone());
    }

    Ok(BlockPatchOutcome {
        new_body,
        new_body_format,
        event_metadata,
    })
}

#[cfg(test)]
mod import_tests {
    use super::super::*;
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_core::events::EventBus;
    use djinn_db::{Database, ProposalCreateInput};
    use serde_json::Value as JsonValue;

    async fn test_server() -> (DjinnMcpServer, Database) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_import_creates_valid_mdx_and_preserves_fields() {
        let (server, db) = test_server().await;
        let body = "Intro\n\n<Diagram id=\"arch\" title=\"Architecture\">\nA -> B\n</Diagram>\n";
        let mdx = format!(
            "---\ntitle: Portable title\nbody_format: mdx\nacceptance_criteria:\n  - Keep the exact string\n  - criterion: Structured AC\n    met: true\n---\n{body}"
        );

        let response = server
            .dispatch_tool("proposal_import", serde_json::json!({ "mdx": mdx }))
            .await
            .expect("proposal_import should be registered");

        let id = response.get("id").and_then(|v| v.as_str()).unwrap();
        let repo = ProposalRepository::new(db, EventBus::noop());
        let stored = repo.get(id).await.unwrap().unwrap();
        assert_eq!(stored.title, "Portable title");
        assert_eq!(stored.body_format, "mdx");
        assert_eq!(stored.body, body);
        assert_eq!(
            serde_json::from_str::<JsonValue>(&stored.acceptance_criteria).unwrap(),
            serde_json::json!([
                "Keep the exact string",
                { "criterion": "Structured AC", "met": true }
            ])
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_import_rejects_unknown_block_with_tag_name() {
        let (server, _db) = test_server().await;
        let mdx = "---\ntitle: Bad block\nbody_format: mdx\n---\n<FancyUnknown id=\"x\" />";

        let response = server
            .dispatch_tool("proposal_import", serde_json::json!({ "mdx": mdx }))
            .await
            .unwrap();

        let error = response.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(error.contains("FancyUnknown"), "error was {error}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_import_updates_when_id_is_present() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let existing = repo
            .create(ProposalCreateInput {
                title: "Before",
                body: "old body",
                acceptance_criteria: Some("[\"old\"]"),
                status: None,
                body_format: Some("markdown"),
            })
            .await
            .unwrap();
        let mdx = format!(
            "---\nid: {}\ntitle: After\nbody_format: markdown\nacceptance_criteria:\n  - new one\n---\nupdated body",
            existing.short_id
        );

        let response = server
            .dispatch_tool("proposal_import", serde_json::json!({ "mdx": mdx }))
            .await
            .unwrap();

        assert_eq!(
            response.get("id").and_then(|v| v.as_str()),
            Some(existing.id.as_str())
        );
        let stored = repo.get(&existing.id).await.unwrap().unwrap();
        assert_eq!(stored.title, "After");
        assert_eq!(stored.body, "updated body");
        assert_eq!(stored.body_format, "markdown");
        assert_eq!(
            serde_json::from_str::<JsonValue>(&stored.acceptance_criteria).unwrap(),
            serde_json::json!(["new one"])
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_import_without_frontmatter_defaults_to_plain_markdown() {
        let (server, db) = test_server().await;
        let body = "# Offline notes\n\nPlain markdown only.";

        let response = server
            .proposal_import(Parameters(ProposalImportParams {
                mdx: body.to_string(),
            }))
            .await
            .0;

        let imported = response.proposal.unwrap();
        let repo = ProposalRepository::new(db, EventBus::noop());
        let stored = repo.get(&imported.id).await.unwrap().unwrap();
        assert_eq!(stored.title, "Imported proposal");
        assert_eq!(stored.body_format, "markdown");
        assert_eq!(stored.body, body);
        assert_eq!(
            serde_json::from_str::<JsonValue>(&stored.acceptance_criteria).unwrap(),
            serde_json::json!([])
        );
    }

    /// A `proposal.mdx` whose frontmatter declares `body_format: markdown` but
    /// whose body carries block tags is upgraded to mdx and validated on import
    /// (previously the blocks were stored unvalidated as raw markdown).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_import_markdown_frontmatter_with_blocks_upgrades_to_mdx() {
        let (server, db) = test_server().await;
        let mdx = "---\ntitle: Imported blocks\nbody_format: markdown\n---\nIntro\n\n<Callout id=\"c\" tone=\"info\">\nNote.\n</Callout>\n";

        let response = server
            .dispatch_tool("proposal_import", serde_json::json!({ "mdx": mdx }))
            .await
            .unwrap();
        assert!(
            response.get("error").is_none(),
            "import should succeed: {:?}",
            response.get("error")
        );
        let id = response.get("id").and_then(|v| v.as_str()).unwrap();
        let repo = ProposalRepository::new(db, EventBus::noop());
        let stored = repo.get(id).await.unwrap().unwrap();
        assert_eq!(
            stored.body_format, "mdx",
            "markdown frontmatter + block tags must be stored as mdx"
        );
    }

    /// Import likewise rejects an empty children-based block written in the
    /// self-closing attribute form, even when the frontmatter says markdown.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_import_rejects_self_closing_children_block() {
        let (server, _db) = test_server().await;
        let mdx = "---\ntitle: Bad decisions\nbody_format: markdown\n---\n<Decisions id=\"d\" decisions={[{\"decision\":\"x\"}]} />\n";

        let response = server
            .dispatch_tool("proposal_import", serde_json::json!({ "mdx": mdx }))
            .await
            .unwrap();
        let err = response
            .get("error")
            .and_then(|v| v.as_str())
            .expect("error");
        assert!(err.contains("Decisions block"), "error was: {err}");
        assert!(err.contains("###"), "error was: {err}");
    }
}

#[cfg(test)]
mod export_tests {
    use super::super::*;
    use super::split_proposal_mdx_frontmatter;
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use crate::tools::proposal_blocks::parse_mdx_blocks;
    use djinn_core::events::EventBus;
    use djinn_db::{Database, ProposalCreateInput};

    async fn test_server() -> (DjinnMcpServer, Database) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_export_markdown_preserves_frontmatter_and_body() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db, EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "My Proposal",
                body: "# Hello\n\nSome markdown body.",
                acceptance_criteria: Some(
                    r#"["First criterion", {"criterion": "Second criterion", "met": true}]"#,
                ),
                status: None,
                body_format: Some("markdown"),
            })
            .await
            .unwrap();

        let response = server
            .proposal_export(Parameters(ProposalExportParams {
                id: proposal.id.clone(),
            }))
            .await
            .0;

        let mdx = response.mdx.as_deref().expect("mdx field should be set");
        // Verify frontmatter structure.
        assert!(mdx.starts_with("---\n"), "mdx should start with ---");
        assert!(mdx.contains("title: My Proposal\n"));
        assert!(mdx.contains("body_format: markdown\n"));
        assert!(mdx.contains("  - First criterion\n"));
        assert!(mdx.contains("  - criterion: Second criterion\n    met: true\n"));

        // Verify the body is exactly preserved after the closing delimiter.
        let body_start = mdx.find("\n---\n").unwrap() + 5;
        assert_eq!(&mdx[body_start..], "# Hello\n\nSome markdown body.");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_export_mdx_round_trips_through_block_parser() {
        let (server, db) = test_server().await;
        let body = "Intro\n\n<Diagram id=\"arch\" title=\"Architecture\">\nA -> B\n</Diagram>\n";
        let repo = ProposalRepository::new(db, EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "MDX Proposal",
                body,
                acceptance_criteria: Some(
                    r#"[{"criterion": "Round-trip fidelity", "met": false}]"#,
                ),
                status: None,
                body_format: Some("mdx"),
            })
            .await
            .unwrap();

        let response = server
            .proposal_export(Parameters(ProposalExportParams {
                id: proposal.id.clone(),
            }))
            .await
            .0;

        let mdx = response.mdx.as_deref().expect("mdx field should be set");
        assert!(mdx.contains("body_format: mdx\n"));

        // Round-trip: re-parse the exported mdx and verify structural equality.
        let (_, exported_body) = split_proposal_mdx_frontmatter(mdx).unwrap();
        let original_blocks = parse_mdx_blocks(body).unwrap();
        let exported_blocks = parse_mdx_blocks(exported_body).unwrap();
        assert_eq!(
            original_blocks, exported_blocks,
            "exported MDX blocks must be structurally identical to the original"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_export_canonical_fixture_body_is_byte_identical() {
        // A canonical-fixture-derived MDX body containing the trickier blocks:
        // a `>` inside an attribute value, bare-brace JSON children
        // (JsonExplorer), and a nested same-tag child. The exported body must be
        // byte-equal to the stored body AND the structural round-trip equality
        // check inside `proposal_export` must pass.
        let (server, db) = test_server().await;
        let body = r#"# Canonical Proposal

<RichText id="intro">
Welcome to the proposal.
</RichText>

<ApiEndpoint id="get-users" method="GET" path="/api/users?filter=>active">
Returns active users.
</ApiEndpoint>

<JsonExplorer id="config-sample">
{
  "enabled": true,
  "nested": { "a": [1, 2, 3] },
  "labels": ["alpha", "beta"]
}
</JsonExplorer>

<Callout id="outer" tone="warning">
before
<Callout id="inner">nested</Callout>
after
</Callout>

<QuestionForm id="open-questions" title="Open Questions">
Should we use Redis or Memcached?
</QuestionForm>
"#;
        let repo = ProposalRepository::new(db, EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Canonical",
                body,
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: Some("mdx"),
            })
            .await
            .unwrap();

        let response = server
            .proposal_export(Parameters(ProposalExportParams {
                id: proposal.id.clone(),
            }))
            .await
            .0;

        // The structural round-trip equality check inside `proposal_export`
        // must pass (no error surfaced).
        assert!(
            response.error.is_none(),
            "round-trip should succeed, got: {:?}",
            response.error
        );
        let mdx = response.mdx.as_deref().expect("mdx field should be set");
        let (_, exported_body) = split_proposal_mdx_frontmatter(mdx).unwrap();

        // Export is verbatim: the body after the frontmatter is byte-identical
        // to what was stored.
        assert_eq!(
            exported_body, body,
            "exported body must be byte-identical to the stored body"
        );

        // And the parsed blocks are structurally identical across the two parses.
        let original_blocks = parse_mdx_blocks(body).unwrap();
        let exported_blocks = parse_mdx_blocks(exported_body).unwrap();
        assert_eq!(original_blocks, exported_blocks);
        // The nested same-tag child is preserved inside the outer block's
        // raw_content (the old regex would have truncated it here).
        let outer = original_blocks
            .iter()
            .find(|b| b.id == "outer")
            .expect("outer callout present");
        assert!(
            outer
                .raw_content
                .contains("<Callout id=\"inner\">nested</Callout>"),
            "nested same-tag child must survive in raw_content"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_export_nonexistent_id_returns_error() {
        let (server, _db) = test_server().await;

        let response = server
            .proposal_export(Parameters(ProposalExportParams {
                id: "nonexistent-id".to_string(),
            }))
            .await
            .0;

        assert!(response.proposal.is_none());
        assert!(response.mdx.is_none());
        let error = response.error.as_deref().expect("should have error");
        assert!(error.contains("proposal not found"), "error was: {error}");
    }
}

#[cfg(test)]
mod block_patch_tests {
    use super::{
        ByteRangeSelector, PATCH_RANGE_NOT_NODE_ALIGNED, resolve_byte_range_selector,
        resolve_exact_text_selector, resolve_heading_selector,
    };
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_core::events::EventBus;
    use djinn_db::{Database, ProposalCreateInput, ProposalRepository};

    async fn test_server() -> (DjinnMcpServer, Database) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_patch_replace_by_exact_text_preserves_unrelated_content() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Patch Test",
                body: "# Intro\n\nOld paragraph here.\n\n## Details\n\nMore content.",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let response = server
            .dispatch_tool(
                "proposal_block_patch",
                serde_json::json!({
                    "id": proposal.id,
                    "selector": { "exact_text": "Old paragraph here." },
                    "operation": "replace",
                    "block_mdx": "<RichText id=\"new\">\nNew rich text content.\n</RichText>",
                }),
            )
            .await
            .unwrap();

        assert!(
            response.get("error").is_none(),
            "patch failed: {:?}",
            response.get("error")
        );
        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert!(
            stored.body.contains("# Intro\n\n"),
            "unrelated intro must survive"
        );
        assert!(
            stored.body.contains("## Details\n\nMore content."),
            "unrelated details must survive"
        );
        assert!(
            stored.body.contains("<RichText id=\"new\">"),
            "new block must be inserted"
        );
        assert!(
            !stored.body.contains("Old paragraph here."),
            "old content must be replaced"
        );
        assert_eq!(
            stored.body_format, "mdx",
            "format must upgrade to mdx when block inserted"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_patch_increments_revision_seq_once() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Revision Test",
                body: "First paragraph.\n\nSecond paragraph.",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        assert_eq!(proposal.latest_revision_seq, 1);

        let response = server
            .dispatch_tool(
                "proposal_block_patch",
                serde_json::json!({
                    "id": proposal.id,
                    "selector": { "exact_text": "Second paragraph." },
                    "operation": "replace",
                    "block_mdx": "<Callout id=\"c1\">\nImportant note.\n</Callout>",
                }),
            )
            .await
            .unwrap();
        assert!(response.get("error").is_none());

        let updated = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(
            updated.latest_revision_seq, 2,
            "revision must increment exactly once"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_patch_rejects_stale_expected_revision() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Stale Test",
                body: "Content here.",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let response = server
            .dispatch_tool(
                "proposal_block_patch",
                serde_json::json!({
                    "id": proposal.id,
                    "selector": { "exact_text": "Content here." },
                    "operation": "replace",
                    "block_mdx": "<Diagram id=\"d1\" />\n",
                    "expected_latest_revision_seq": 99,
                }),
            )
            .await
            .unwrap();

        let error = response.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(error.contains("stale revision"), "error was: {error}");
        // Verify nothing was modified.
        let unchanged = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(unchanged.latest_revision_seq, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_patch_rejects_missing_selector() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Missing Selector",
                body: "Some body.",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let response = server
            .dispatch_tool(
                "proposal_block_patch",
                serde_json::json!({
                    "id": proposal.id,
                    "selector": {},
                    "operation": "replace",
                    "block_mdx": "<RichText id=\"r1\">hi</RichText>",
                }),
            )
            .await
            .unwrap();

        let error = response.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(error.contains("exactly one"), "error was: {error}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_patch_rejects_ambiguous_exact_text() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Ambiguous",
                body: "Repeated text.\n\nMore.\n\nRepeated text.",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let response = server
            .dispatch_tool(
                "proposal_block_patch",
                serde_json::json!({
                    "id": proposal.id,
                    "selector": { "exact_text": "Repeated text." },
                    "operation": "replace",
                    "block_mdx": "<RichText id=\"r1\">hi</RichText>",
                }),
            )
            .await
            .unwrap();

        let error = response.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(error.contains("ambiguous"), "error was: {error}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_patch_heading_selector_replaces_section() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Heading Patch",
                body: "# First\n\nFirst content.\n\n# Second\n\nSecond content.\n\n# Third\n\nThird content.",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let response = server
            .dispatch_tool(
                "proposal_block_patch",
                serde_json::json!({
                    "id": proposal.id,
                    "selector": { "heading_text": "Second" },
                    "operation": "replace",
                    "block_mdx": "# Second\n\n<Callout id=\"c1\">\nReplaced second section.\n</Callout>\n",
                }),
            )
            .await
            .unwrap();

        assert!(
            response.get("error").is_none(),
            "patch failed: {:?}",
            response.get("error")
        );
        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert!(
            stored.body.contains("# First\n\nFirst content."),
            "first section preserved"
        );
        assert!(
            stored.body.contains("<Callout id=\"c1\">"),
            "callout inserted"
        );
        assert!(
            !stored.body.contains("Second content."),
            "old second content replaced"
        );
        assert!(
            stored.body.contains("# Third\n\nThird content."),
            "third section preserved"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_patch_wrap_preserves_selected_content_and_inserts_before() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Wrap Test",
                body: "Before.\n\nTarget text.\n\nAfter.",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Wrap inserts block_mdx before the selected text. Use a plain markdown
        // prefix (no MDX block tags) to keep validation simple.
        let response = server
            .dispatch_tool(
                "proposal_block_patch",
                serde_json::json!({
                    "id": proposal.id,
                    "selector": { "exact_text": "Target text." },
                    "operation": "wrap",
                    "block_mdx": "> **Note:** ",
                }),
            )
            .await
            .unwrap();

        assert!(
            response.get("error").is_none(),
            "wrap failed: {:?}",
            response.get("error")
        );
        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert!(
            stored.body.contains("> **Note:** Target text."),
            "wrapped content present"
        );
        assert!(stored.body.contains("Before.\n"), "before preserved");
        assert!(stored.body.contains("\nAfter."), "after preserved");
        assert_eq!(
            stored.body_format, "markdown",
            "no MDX tags so stays markdown"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_patch_records_event_metadata() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Metadata Test",
                body: "Some content.",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        server
            .dispatch_tool(
                "proposal_block_patch",
                serde_json::json!({
                    "id": proposal.id,
                    "selector": { "exact_text": "Some content." },
                    "operation": "replace",
                    "block_mdx": "<RichText id=\"r1\">New content.</RichText>",
                    "native_skill_name": "visual-spec",
                    "native_skill_version": "1.0.0",
                    "note": "test patch",
                }),
            )
            .await
            .unwrap();

        let revisions = repo.revisions(&proposal.id).await.unwrap();
        // Revision 1 is the create seed; revision 2 is the block patch.
        assert_eq!(revisions.len(), 2);
        let patch_rev = &revisions[1];
        let metadata: serde_json::Value =
            serde_json::from_str(patch_rev.event_metadata.as_deref().unwrap_or("null")).unwrap();
        assert_eq!(metadata["change_kind"], "targeted_block_patch");
        assert_eq!(metadata["native_skill_name"], "visual-spec");
        assert_eq!(metadata["native_skill_version"], "1.0.0");
        assert_eq!(metadata["note"], "test patch");
        assert!(metadata["range_start_byte"].is_number());
        assert!(metadata["range_end_byte"].is_number());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_patch_byte_range_selector() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let body = "aaa bbb ccc";
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Byte Range Test",
                body,
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // "bbb" starts at byte 4, ends at byte 7.
        let response = server
            .dispatch_tool(
                "proposal_block_patch",
                serde_json::json!({
                    "id": proposal.id,
                    "selector": {
                        "byte_range": {
                            "start": 0,
                            "end": 11,
                            "expected_text": "aaa bbb ccc"
                        }
                    },
                    "operation": "replace",
                    "block_mdx": "DDD",
                }),
            )
            .await
            .unwrap();

        assert!(
            response.get("error").is_none(),
            "patch failed: {:?}",
            response.get("error")
        );
        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(stored.body, "DDD");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_patch_byte_range_rejects_stale_text() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Stale Range",
                body: "paragraph one.\n\nparagraph two.",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let response = server
            .dispatch_tool(
                "proposal_block_patch",
                serde_json::json!({
                    "id": proposal.id,
                    "selector": {
                        "byte_range": {
                            "start": 1,
                            "end": 5,
                            "expected_text": "XXX"
                        }
                    },
                    "operation": "replace",
                    "block_mdx": "DDD",
                }),
            )
            .await
            .unwrap();

        let error = response.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(error.contains("text mismatch"), "error was: {error}");
        assert!(
            !error.contains(PATCH_RANGE_NOT_NODE_ALIGNED),
            "expected-text guard must precede alignment: {error}"
        );
        let unchanged = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(unchanged.latest_revision_seq, 1);
        assert_eq!(repo.revisions(&proposal.id).await.unwrap().len(), 1);
    }

    #[test]
    fn selectors_use_ast_boundaries_and_parsed_heading_outline() {
        let body = "# Root\r\n\r\n## Nested\r\n\r\né paragraph\r\n\r\n# C#\r\nEOF";
        let nested = resolve_heading_selector(body, "Nested").unwrap();
        assert_eq!(
            &body[nested.start..nested.end],
            "## Nested\r\n\r\né paragraph\r\n\r\n"
        );
        let eof = resolve_heading_selector(body, "C#").unwrap();
        assert_eq!(&body[eof.start..eof.end], "# C#\r\nEOF");
        assert!(
            resolve_heading_selector("# Same\n\n# Same", "Same")
                .unwrap_err()
                .contains("ambiguous")
        );
        assert!(
            resolve_exact_text_selector("first paragraph.\n\nsecond paragraph.", "first")
                .unwrap_err()
                .contains(PATCH_RANGE_NOT_NODE_ALIGNED)
        );
        assert!(
            resolve_byte_range_selector(
                "first paragraph.\n\nsecond paragraph.",
                &ByteRangeSelector {
                    start: 1,
                    end: 5,
                    expected_text: None
                },
            )
            .unwrap_err()
            .contains(PATCH_RANGE_NOT_NODE_ALIGNED)
        );
    }

    #[test]
    fn code_property_is_the_only_property_alignment_exception() {
        let body = "<AnnotatedCode id=\"a\" code={`let x = 1;`} />";
        let start = body.find("let x = 1;").unwrap();
        let end = start + "let x = 1;".len();
        assert!(
            resolve_byte_range_selector(
                body,
                &ByteRangeSelector {
                    start: start as i64,
                    end: end as i64,
                    expected_text: Some("let x = 1;".into()),
                }
            )
            .is_ok()
        );
        let id_start = body.find("\"a\"").unwrap() + 1;
        assert!(
            resolve_byte_range_selector(
                body,
                &ByteRangeSelector {
                    start: id_start as i64,
                    end: id_start as i64 + 1,
                    expected_text: None,
                }
            )
            .unwrap_err()
            .contains(PATCH_RANGE_NOT_NODE_ALIGNED)
        );
    }
}

// ── Regression coverage for the block-patch primitive (task 1787) ──────
//
// These tests exercise the full `proposal_create` → `proposal_block_patch`
// → `proposal_show` → `proposal_export` flow end to end, proving the
// acceptance criteria the single-patch tests above only prove in isolation:
//
//  1. Two sequential targeted patches increment `latest_revision_seq`
//     exactly twice from the starting proposal revision (1 → 3).
//  2. Unrelated body sections survive both patches byte-for-byte — the
//     body is not a monolithic rewrite fixture.
//  3. Revision history exposes targeted-block-patch metadata including
//     native-skill name/version attribution on every patch revision.
//  4. The enriched `body_format=mdx` proposal exports and round-trips
//     cleanly through `proposal_export` (parses the same blocks twice).
//  5. Bare `<` / `>` MDX-authoring guidance stays backticked: a body
//     containing a backtick-wrapped `Vec<String>` or `a < b` round-trips
//     through the parser without producing bare angle prose in the
//     output, while a bare `Vec<String>` line is detected as prose that
//     needs backtick-wrapping.
#[cfg(test)]
mod block_patch_regression_tests {
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use crate::tools::proposal_blocks::{parse_mdx_blocks, validate_mdx_blocks};
    use djinn_core::events::EventBus;
    use djinn_db::{Database, ProposalCreateInput, ProposalRepository};

    async fn test_server() -> (DjinnMcpServer, Database) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db)
    }

    /// Multi-section markdown body used by the regression suite. Each anchor
    /// substring is a stable byte-exact marker for the "unrelated content
    /// preserved" assertion in test 2.
    const REGRESSION_BODY: &str = "\
# Proposal Title

This is the opening paragraph that we will wrap in a RichText block.

## Approach

The approach section explains the high-level plan.

## Tradeoffs

The tradeoffs section enumerates the costs.

## Open Questions

The open-questions section collects uncertainty.
";

    /// AC #1: two sequential targeted block patches each increment
    /// `latest_revision_seq` exactly once. Starting seq is 1 (create
    /// seed); after two patches it must be 3 — never 2, never 4.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn regression_two_patches_increment_latest_revision_seq_exactly_twice() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Regression Revision Seq",
                body: REGRESSION_BODY,
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        assert_eq!(
            proposal.latest_revision_seq, 1,
            "create seed must leave latest_revision_seq at 1"
        );

        // Patch #1: wrap the opening paragraph in a RichText block.
        let patch_one = server
            .dispatch_tool(
                "proposal_block_patch",
                serde_json::json!({
                    "id": proposal.id,
                    "selector": { "exact_text": "This is the opening paragraph that we will wrap in a RichText block." },
                    "operation": "replace",
                    "block_mdx": "<RichText id=\"opening\">\nThe wrapped opening paragraph.\n</RichText>",
                }),
            )
            .await
            .unwrap();
        assert!(
            patch_one.get("error").is_none(),
            "patch one failed: {:?}",
            patch_one.get("error")
        );
        let after_one = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(
            after_one.latest_revision_seq, 2,
            "first patch must increment latest_revision_seq from 1 to 2"
        );

        // Patch #2: replace a different target (the tradeoffs paragraph).
        let patch_two = server
            .dispatch_tool(
                "proposal_block_patch",
                serde_json::json!({
                    "id": proposal.id,
                    "selector": { "exact_text": "The tradeoffs section enumerates the costs." },
                    "operation": "replace",
                    "block_mdx": "<Callout id=\"tradeoffs\">\nThe new tradeoff callout.\n</Callout>",
                }),
            )
            .await
            .unwrap();
        assert!(
            patch_two.get("error").is_none(),
            "patch two failed: {:?}",
            patch_two.get("error")
        );
        let after_two = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(
            after_two.latest_revision_seq, 3,
            "second patch must increment latest_revision_seq from 2 to 3 (exactly +2 from the starting revision)"
        );

        // The proposal's surface (proposal_show) must also report the same seq.
        let shown = server
            .dispatch_tool("proposal_show", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();
        assert_eq!(
            shown
                .get("proposal")
                .and_then(|p| p.get("latest_revision_seq"))
                .and_then(|v| v.as_i64()),
            Some(3),
            "proposal_show.proposal.latest_revision_seq must match the repo state"
        );
    }

    /// AC #2: unrelated body content survives targeted patches byte-for-byte.
    /// The body must not be replaced by a monolithic rewrite fixture.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn regression_unrelated_body_content_preserved_across_patches() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Regression Preservation",
                body: REGRESSION_BODY,
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Patch #1: replace the opening paragraph.
        let _ = server
            .dispatch_tool(
                "proposal_block_patch",
                serde_json::json!({
                    "id": proposal.id,
                    "selector": { "exact_text": "This is the opening paragraph that we will wrap in a RichText block." },
                    "operation": "replace",
                    "block_mdx": "<RichText id=\"opening\">\nThe wrapped opening paragraph.\n</RichText>",
                }),
            )
            .await
            .unwrap();

        // Patch #2: replace the tradeoffs paragraph with a different target.
        let _ = server
            .dispatch_tool(
                "proposal_block_patch",
                serde_json::json!({
                    "id": proposal.id,
                    "selector": { "exact_text": "The tradeoffs section enumerates the costs." },
                    "operation": "replace",
                    "block_mdx": "<Callout id=\"tradeoffs\">\nThe new tradeoff callout.\n</Callout>",
                }),
            )
            .await
            .unwrap();

        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        // Bytes that are NOT in either selected range must appear verbatim
        // in the final body. This is the byte-identity guard: a monolithic
        // rewrite fixture would change wording, ordering, or whitespace
        // around these markers and the asserts would fail.
        for anchor in [
            "# Proposal Title",
            "## Approach",
            "The approach section explains the high-level plan.",
            "## Open Questions",
            "The open-questions section collects uncertainty.",
            "## Tradeoffs",
        ] {
            assert!(
                stored.body.contains(anchor),
                "unrelated anchor {anchor:?} must be preserved verbatim after targeted patches; \
                 body was:\n{}",
                stored.body
            );
        }

        // The replaced paragraphs must NOT survive in their original form.
        assert!(
            !stored
                .body
                .contains("This is the opening paragraph that we will wrap in a RichText block."),
            "replaced opening paragraph must not survive"
        );
        assert!(
            !stored
                .body
                .contains("The tradeoffs section enumerates the costs."),
            "replaced tradeoffs paragraph must not survive"
        );

        // The new blocks must be present, proving the patches actually landed
        // (otherwise this would be a no-op passing test).
        assert!(
            stored.body.contains("<RichText id=\"opening\">"),
            "patch #1 must insert its RichText block"
        );
        assert!(
            stored.body.contains("<Callout id=\"tradeoffs\">"),
            "patch #2 must insert its Callout block"
        );
    }

    /// AC #3: revision history exposes targeted-block-patch metadata on
    /// every patch revision, including the native-skill name/version
    /// attribution for the planner-driven refinement loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn regression_revisions_expose_targeted_block_patch_metadata_with_skill_attribution() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Regression Metadata",
                body: REGRESSION_BODY,
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Patch #1 attributed to visual-spec@1.0.0.
        let _ = server
            .dispatch_tool(
                "proposal_block_patch",
                serde_json::json!({
                    "id": proposal.id,
                    "selector": { "exact_text": "This is the opening paragraph that we will wrap in a RichText block." },
                    "operation": "replace",
                    "block_mdx": "<RichText id=\"opening\">\nThe wrapped opening paragraph.\n</RichText>",
                    "native_skill_name": "visual-spec",
                    "native_skill_version": "1.0.0",
                    "note": "first patch",
                }),
            )
            .await
            .unwrap();

        // Patch #2 attributed to a different visual-spec revision (1.1.0) so
        // the metadata assertion distinguishes the two entries.
        let _ = server
            .dispatch_tool(
                "proposal_block_patch",
                serde_json::json!({
                    "id": proposal.id,
                    "selector": { "exact_text": "The tradeoffs section enumerates the costs." },
                    "operation": "replace",
                    "block_mdx": "<Callout id=\"tradeoffs\">\nThe new tradeoff callout.\n</Callout>",
                    "native_skill_name": "visual-spec",
                    "native_skill_version": "1.1.0",
                    "note": "second patch",
                }),
            )
            .await
            .unwrap();

        // Walk the revisions through the proposal_show surface (the
        // public-facing view) so this test exercises the same shape that
        // the planner / reviewer agents consume.
        let shown = server
            .dispatch_tool("proposal_show", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();
        let revisions = shown
            .get("revisions")
            .and_then(|v| v.as_array())
            .expect("proposal_show.revisions must be a JSON array");

        // 1 create seed + 2 targeted patches = 3 revisions.
        assert_eq!(
            revisions.len(),
            3,
            "expected 3 revisions (1 seed + 2 targeted patches); got {}",
            revisions.len()
        );

        // The create seed (seq 1) must NOT carry targeted-block-patch
        // metadata — that signal is reserved for the patch primitive.
        let seed = &revisions[0];
        assert!(
            seed.get("event_metadata").is_none_or(|v| v.is_null()),
            "create seed revision must not carry event_metadata, got {:?}",
            seed.get("event_metadata")
        );

        // The two patch revisions (seq 2 and seq 3) must each carry the
        // targeted-block-patch metadata + native-skill attribution.
        let patch_rev_one = &revisions[1];
        let patch_rev_two = &revisions[2];
        let meta_one: serde_json::Value = serde_json::from_str(
            patch_rev_one
                .get("event_metadata")
                .and_then(|v| v.as_str())
                .expect("patch rev #1 must expose event_metadata as a JSON string"),
        )
        .expect("patch rev #1 event_metadata must be valid JSON");
        let meta_two: serde_json::Value = serde_json::from_str(
            patch_rev_two
                .get("event_metadata")
                .and_then(|v| v.as_str())
                .expect("patch rev #2 must expose event_metadata as a JSON string"),
        )
        .expect("patch rev #2 event_metadata must be valid JSON");

        // change_kind identifies the patch primitive for downstream tooling.
        assert_eq!(
            meta_one["change_kind"], "targeted_block_patch",
            "patch rev #1 must identify as targeted_block_patch"
        );
        assert_eq!(
            meta_two["change_kind"], "targeted_block_patch",
            "patch rev #2 must identify as targeted_block_patch"
        );

        // Native-skill name + version attribution per patch.
        assert_eq!(
            meta_one["native_skill_name"], "visual-spec",
            "patch rev #1 must attribute the native skill name"
        );
        assert_eq!(
            meta_one["native_skill_version"], "1.0.0",
            "patch rev #1 must attribute the native skill version"
        );
        assert_eq!(
            meta_two["native_skill_name"], "visual-spec",
            "patch rev #2 must attribute the native skill name"
        );
        assert_eq!(
            meta_two["native_skill_version"], "1.1.0",
            "patch rev #2 must attribute the native skill version"
        );

        // The byte-range fields are present and well-typed — these are the
        // hook a regression in selector resolution would first break.
        assert!(
            meta_one["range_start_byte"].is_number(),
            "patch rev #1 must expose range_start_byte"
        );
        assert!(
            meta_one["range_end_byte"].is_number(),
            "patch rev #1 must expose range_end_byte"
        );
        assert!(
            meta_one["range_end_byte"].as_u64().unwrap()
                > meta_one["range_start_byte"].as_u64().unwrap(),
            "patch rev #1 range_end_byte must exceed range_start_byte"
        );
        assert!(
            meta_two["range_start_byte"].is_number() && meta_two["range_end_byte"].is_number(),
            "patch rev #2 must expose numeric range fields"
        );

        // Free-form notes were preserved too.
        assert_eq!(meta_one["note"], "first patch");
        assert_eq!(meta_two["note"], "second patch");
    }

    /// AC #4: after enrichment via targeted block-patches, the proposal
    /// exports cleanly through `proposal_export` and the returned MDX
    /// round-trips through the block parser (no parse error, structural
    /// equality of the parsed blocks).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn regression_block_patches_then_export_round_trips_cleanly() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Regression Export",
                body: REGRESSION_BODY,
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Promote a couple of prose sections to MDX blocks via targeted
        // patches — this is the planner refinement loop's "convert
        // markdown drafts to block-enriched MDX" path.
        for (selector_text, block_mdx) in [
            (
                "This is the opening paragraph that we will wrap in a RichText block.",
                "<RichText id=\"opening\">\nThe wrapped opening paragraph.\n</RichText>",
            ),
            (
                "The approach section explains the high-level plan.",
                "<FileTree id=\"repo\" name=\"repo\">\nsrc/\n</FileTree>",
            ),
        ] {
            let response = server
                .dispatch_tool(
                    "proposal_block_patch",
                    serde_json::json!({
                        "id": proposal.id,
                        "selector": { "exact_text": selector_text },
                        "operation": "replace",
                        "block_mdx": block_mdx,
                    }),
                )
                .await
                .unwrap();
            assert!(
                response.get("error").is_none(),
                "patch failed for {selector_text:?}: {:?}",
                response.get("error")
            );
        }

        // The proposal body_format must have upgraded to mdx (the patches
        // introduced MDX block tags). proposal_export round-trips mdx
        // bodies by re-parsing the exported output, so the export path
        // is the load-bearing check for this acceptance criterion.
        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(
            stored.body_format, "mdx",
            "body_format must upgrade to mdx once the first MDX block is patched in"
        );

        let exported = server
            .dispatch_tool("proposal_export", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();
        assert!(
            exported.get("error").is_none(),
            "proposal_export failed after MDX enrichment: {:?}",
            exported.get("error")
        );

        let mdx = exported
            .get("mdx")
            .and_then(|v| v.as_str())
            .expect("proposal_export.mdx must be a non-empty string for mdx proposals");
        assert!(
            mdx.contains("body_format: mdx"),
            "exported MDX frontmatter must record body_format: mdx"
        );
        assert!(
            mdx.contains("---\n") && mdx.matches("---").count() >= 2,
            "exported MDX must include the YAML frontmatter delimiters"
        );

        // The exported MDX body (everything after the second ---) must
        // parse into the same blocks as the stored body. This is the
        // round-trip fidelity contract the export path enforces for
        // mdx proposals.
        let original_blocks =
            parse_mdx_blocks(&stored.body).expect("stored body must parse as MDX");
        let exported_body = mdx
            .splitn(3, "---")
            .nth(2)
            .expect("exported MDX must have a body section after frontmatter")
            .trim_start_matches('\n');
        let exported_blocks =
            parse_mdx_blocks(exported_body).expect("exported body must parse as MDX");
        // The export path enforces structural equality of the parsed
        // blocks — if it errored above, that already proves the
        // round-trip parse succeeded. Re-assert the equality here so a
        // future regression that loosens the export check is caught.
        assert_eq!(
            exported_blocks, original_blocks,
            "exported MDX blocks must match the stored body blocks byte-for-byte"
        );
        let exported_ids: Vec<&str> = exported_blocks.iter().map(|b| b.id.as_str()).collect();
        assert!(
            exported_ids.contains(&"opening"),
            "exported blocks must include the patched RichText id; got {exported_ids:?}"
        );
        assert!(
            exported_ids.contains(&"repo"),
            "exported blocks must include the patched FileTree id; got {exported_ids:?}"
        );
    }

    /// AC #5: the bare `<` / `>` constraint — MDX authoring guidance
    /// requires generics / operators to be backtick-wrapped, e.g.
    /// `Vec<String>` and `a < b`. This regression asserts that the
    /// parser treats the backtick-wrapped forms as valid prose, and
    /// pins the visual-spec SKILL.md guidance so a future edit that
    /// removes or weakens the backtick constraint is caught.
    ///
    /// Concretely:
    ///   - Backtick-fenced `\`Option<T>\`` and `\`a < b\`` round-trip
    ///     through `parse_mdx_blocks` without producing any registered
    ///     PascalCase tag (the backticks hide the angle brackets from
    ///     the JSX walker) and pass `validate_mdx_blocks`.
    ///   - A registered block beside backticked prose still parses
    ///     cleanly — the constraint applies to prose, not to the
    ///     registered block tags.
    ///   - The visual-spec SKILL.md itself contains the backtick-wrapped
    ///     examples — pin the constraint by asserting the guidance text
    ///     is present.
    #[test]
    fn regression_bare_angle_bracket_guidance_is_backticked() {
        // (a) Backtick-wrapped generics and operators: zero registered
        //     blocks, valid parse, valid validation.
        let backticked = "\
This body mentions the `Vec<String>` type, the `Option<T>` enum, and the
comparison `a < b` in prose. None of those angle brackets should produce a
registered block.
";
        let blocks = parse_mdx_blocks(backticked)
            .expect("backtick-wrapped angle brackets must parse without error");
        assert!(
            blocks.is_empty(),
            "backtick-wrapped angle brackets must not produce any registered blocks; got {blocks:?}"
        );
        assert!(
            validate_mdx_blocks(backticked).is_ok(),
            "backtick-wrapped angle brackets must pass block validation"
        );

        // (b) A registered MDX block beside backticked prose still
        //     parses cleanly — the constraint applies to prose, not to
        //     the registered block tags.
        let mixed = "\
Use `Option<T>` to express optional values, and the inequality `a > b` is
fine too.

<RichText id=\"summary\">
Summary paragraph.
</RichText>
";
        let mixed_blocks = parse_mdx_blocks(mixed)
            .expect("mixed body with backticked angle brackets + registered block must parse");
        assert_eq!(
            mixed_blocks.len(),
            1,
            "only the registered <RichText> block should be recognised; got {mixed_blocks:?}"
        );
        assert_eq!(mixed_blocks[0].id, "summary");
        assert!(
            validate_mdx_blocks(mixed).is_ok(),
            "mixed body must pass block validation"
        );

        // (c) Pin the authoring guidance itself so a future edit that
        //     removes or weakens the backtick constraint is caught.
        let skill_body = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("crates")
                .join("djinn-agent")
                .join("src")
                .join("native_assets")
                .join("visual-spec")
                .join("SKILL.md"),
        )
        .expect("visual-spec SKILL.md must be readable from the control-plane test sandbox");
        assert!(
            skill_body.contains("## Bare `<` / `>` backtick constraint"),
            "visual-spec SKILL.md must keep the bare-angle backtick constraint heading; \
             found body:\n{skill_body}"
        );
        assert!(
            skill_body.contains("`Option<T>`") || skill_body.contains("`Vec<String>`"),
            "visual-spec SKILL.md must include a backticked generics example (Option<T> or Vec<String>)"
        );
        assert!(
            skill_body.contains("backtick"),
            "visual-spec SKILL.md must mention the backtick mechanism"
        );
    }
}
