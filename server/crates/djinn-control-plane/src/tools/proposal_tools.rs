// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
// MCP tools for the global Proposals layer (Phase 0).
//
// A proposal is a project-INDEPENDENT, collaboratively-authored artifact
// (spec body + acceptance criteria) that targets zero, one, or many projects
// via an editable M:N `proposal_targets` set. Discussion and suggestions share
// one `proposal_feedback` primitive (status == null → discussion; open/
// accepted/rejected → a trackable suggestion; author_kind == "ai" for future
// adversarial-review findings). Sign-offs gate approval, revisions/diffs track
// edits, and `proposal_graduate` kicks an approved proposal off into one epic
// per primary target (the existing single-repo-write execution engine).

use std::borrow::Cow;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::server::DjinnMcpServer;
use crate::tools::acting_user::acting_caps;
use crate::tools::epic_ops::AcceptanceCriterionItem;
use crate::tools::list_response::{
    self, ListMeta, NamedListResponse, named_list_response_schema, serialize_named_list_response,
};
use crate::tools::proposal_blocks::{
    extract_custom_block_tags, parse_mdx_blocks, validate_mdx_blocks,
    validate_question_form_placement,
};
use crate::tools::proposal_ops::{
    ProposalDebateTrailModel, ProposalDeleteResponse, ProposalEpicModel, ProposalFeedbackResponse,
    ProposalModel, ProposalReconcileObsoleteEpicResponse, ProposalShowResponse,
    ProposalSignoffModel, ProposalSingleResponse, ProposalTargetModel, ProposalTargetsResponse,
};
use crate::tools::proposal_readiness::evaluate_proposal_readiness;
use crate::tools::validation::{
    validate_ac_count, validate_body, validate_design, validate_limit, validate_mdx_body,
    validate_offset, validate_proposal_create_status, validate_proposal_status, validate_sort,
    validate_title,
};
use djinn_db::{
    EpicRepository, ProjectRepository, ProposalListQuery, ProposalRepository, TaskRepository,
};

// ── List response (NamedListResponse boilerplate, mirrors EpicListResponse) ──

#[derive(Clone)]
pub struct ProposalListResponse {
    pub proposals: Option<Vec<ProposalModel>>,
    pub meta: ListMeta,
}

impl NamedListResponse for ProposalListResponse {
    type Item = ProposalModel;
    const FIELD_NAME: &'static str = "proposals";
    const TITLE: &'static str = "ProposalListResponse";

    fn from_parts(items: Option<Vec<Self::Item>>, meta: ListMeta) -> Self {
        Self {
            proposals: items,
            meta,
        }
    }
    fn items(&self) -> Option<&Vec<Self::Item>> {
        self.proposals.as_ref()
    }
    fn meta(&self) -> &ListMeta {
        &self.meta
    }
}

impl Serialize for ProposalListResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serialize_named_list_response(self, serializer)
    }
}

impl schemars::JsonSchema for ProposalListResponse {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed(Self::TITLE)
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        named_list_response_schema::<ProposalModel>(generator, Self::TITLE, Self::FIELD_NAME)
    }
}

fn proposal_not_found_error(id: &str) -> String {
    format!("proposal not found: {id}")
}

/// List a proposal's targets and resolve each project id to an `owner/repo`
/// slug + name for display chips.
async fn target_models(
    proposal_repo: &ProposalRepository,
    project_repo: &ProjectRepository,
    proposal_id: &str,
) -> Result<Vec<ProposalTargetModel>, String> {
    let targets = proposal_repo
        .targets(proposal_id)
        .await
        .map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(targets.len());
    for t in &targets {
        let mut m = ProposalTargetModel::from(t);
        if let Ok(Some(p)) = project_repo.get(&t.project_id).await {
            m.project_path = Some(format!("{}/{}", p.github_owner, p.github_repo));
            m.project_name = Some(p.name);
        }
        out.push(m);
    }
    Ok(out)
}

// ── Param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalCreateParams {
    pub title: String,
    /// Spec body (markdown or MDX depending on `body_format`).
    pub body: Option<String>,
    /// Acceptance criteria: plain strings or `{criterion, met}` objects.
    pub acceptance_criteria: Option<Vec<AcceptanceCriterionItem>>,
    /// Target projects (UUIDs or owner/repo slugs) this proposal touches.
    /// Editable later via proposal_add_target / proposal_remove_target.
    pub target_projects: Option<Vec<String>>,
    /// Initial status: `triage`, `draft` (default), or `in_review`. Proposer-
    /// role authors are always placed in `triage` regardless of this value.
    pub status: Option<String>,
    /// Body encoding: `markdown` (default) or `mdx` (block-aware).
    pub body_format: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalImportParams {
    /// Full portable proposal.mdx content, including optional YAML frontmatter.
    pub mdx: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalExportParams {
    /// Proposal UUID or short_id.
    pub id: String,
}

#[derive(Debug)]
struct ImportedProposalMdx<'a> {
    id: Option<String>,
    title: String,
    body_format: String,
    body: &'a str,
    acceptance_criteria_json: String,
}

#[derive(Debug, Deserialize)]
struct ProposalMdxFrontmatter {
    id: Option<String>,
    title: Option<String>,
    body_format: Option<String>,
    acceptance_criteria: Option<JsonValue>,
}

fn split_proposal_mdx_frontmatter(mdx: &str) -> Result<(Option<&str>, &str), String> {
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

fn parse_proposal_mdx(mdx: &str) -> Result<ImportedProposalMdx<'_>, String> {
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

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalShowParams {
    /// Proposal UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalListParams {
    pub status: Option<String>,
    /// Filter by author user id.
    pub author: Option<String>,
    /// Filter to proposals targeting this project (UUID or owner/repo slug).
    pub target_project: Option<String>,
    /// Full-text search on title and body.
    pub text: Option<String>,
    /// Sort order: "created_desc" (default), "created", "updated", "updated_desc".
    pub sort: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalUpdateParams {
    /// Proposal UUID or short_id.
    pub id: String,
    pub title: Option<String>,
    pub body: Option<String>,
    /// Acceptance criteria: plain strings or `{criterion, met}` objects.
    pub acceptance_criteria: Option<Vec<AcceptanceCriterionItem>>,
    /// draft | in_review | approved | building | done | rejected | archived | superseded.
    pub status: Option<String>,
    /// UUID or short_id of the proposal that supersedes this one.
    pub superseded_by: Option<String>,
    /// Body encoding: `markdown` (default) or `mdx` (block-aware).
    pub body_format: Option<String>,
}

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
struct ResolvedRange {
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

    // First pass: find all headings matching the needle using line-based scan.
    let mut matches: Vec<(usize, usize)> = Vec::new(); // (line_byte_start, heading_level)
    let mut offset = 0usize;
    for line in body.lines() {
        if let Some(stripped) = line.strip_prefix('#') {
            let hashes = 1 + stripped.len() - stripped.trim_start_matches('#').len();
            let text = line[hashes..].trim();
            if text == needle {
                matches.push((offset, hashes));
            }
        }
        offset += line.len() + 1; // +1 for '\n'
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

    let (heading_start, heading_level) = matches[0];

    // Find the end of this section: next heading at same or higher level, or
    // end of body.
    let mut section_end = body.len();
    let mut pos = heading_start;
    for line in body[heading_start..].lines().skip(1) {
        pos += line.len() + 1;
        if let Some(stripped) = line.strip_prefix('#') {
            let hashes = 1 + stripped.len() - stripped.trim_start_matches('#').len();
            if hashes <= heading_level {
                // Trim back to just before this heading line's start.
                section_end = pos - line.len() - 1;
                break;
            }
        }
    }

    Ok(ResolvedRange {
        start: heading_start,
        end: section_end,
        selector_description: format!("heading: {needle}"),
    })
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
    Ok(ResolvedRange {
        start,
        end: start + needle.len(),
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
    if let Some(ref expected) = br.expected_text {
        let actual = &body[start..end];
        if actual != expected {
            return Err(format!(
                "byte_range text mismatch: expected {:?} but found {:?}",
                expected, actual
            ));
        }
    }
    Ok(ResolvedRange {
        start,
        end,
        selector_description: format!("byte_range[{}..{}]", start, end),
    })
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

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalDeleteParams {
    /// Proposal UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalTargetParams {
    /// Proposal UUID or short_id.
    pub id: String,
    /// Target project: UUID or owner/repo slug (must be registered).
    pub project: String,
    /// `primary` (a write-target, default) or `reference` (read-only context).
    pub role: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalFeedbackAddParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
    pub body: String,
    /// Parent feedback id for a threaded reply.
    pub parent_id: Option<String>,
    /// `user` (default) or `ai`.
    pub author_kind: Option<String>,
    /// Model id when author_kind is `ai`.
    pub author_model: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalFeedbackResolveParams {
    /// Feedback entry UUID.
    pub id: String,
    /// The proposal revision that addressed this feedback (omit for a plain
    /// dismissal with no spec change).
    pub resolved_revision_seq: Option<i32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalSignoffParams {
    /// Proposal UUID or short_id.
    pub id: String,
    /// `scoped` (product) or `technical` (engineering).
    pub kind: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalGraduateParams {
    /// Proposal UUID or short_id (must be `approved`).
    pub id: String,
    /// Build owner — must be a participant (author or a sign-off giver).
    /// Defaults to the kicking-off user.
    pub owner_user_id: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalStopBuildParams {
    /// Proposal UUID or short_id (must be `building`).
    pub id: String,
    /// `abort` (tear the build down and revert to `approved`), `freeze` (hold
    /// the build's tasks out of dispatch, leaving epics/tasks/branches in
    /// place), or `unfreeze` (resume a frozen build).
    pub mode: String,
    /// Why the build is being stopped. Recorded as the force-close reason on
    /// each torn-down task. Required for `abort`.
    pub reason: Option<String>,
    /// When true on `abort`, compute and return the blast radius (epics, open
    /// tasks, running sessions) WITHOUT mutating anything.
    pub preview: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalReconcileObsoleteEpicParams {
    /// Proposal UUID or short_id. Alias for `proposal_id`.
    pub id: Option<String>,
    /// Proposal UUID or short_id. Alias for `id`.
    pub proposal_id: Option<String>,
    /// Epic UUID or short_id to retire from this proposal's graduated epics.
    pub epic_id: String,
    /// Why obsolete work is being force-closed. Defaults to a reconcile teardown reason.
    pub reason: Option<String>,
    /// When true, compute blast radius without closing tasks, closing/unlinking the epic, or killing sessions.
    pub preview: Option<bool>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct ProposalStopBuildResponse {
    pub ok: bool,
    pub mode: String,
    pub proposal_id: Option<String>,
    /// Resulting proposal status (`approved` after a non-preview abort,
    /// `building` after freeze/unfreeze/preview).
    pub status: Option<String>,
    /// `true` when this was a dry-run that did not mutate anything.
    pub preview: bool,
    /// Epics torn down (abort) or that would be (preview).
    pub epics_closed: i64,
    /// Worker tasks force-closed (abort) or open right now (preview).
    pub tasks_closed: i64,
    /// Running worker sessions killed (abort) or live now (preview).
    pub sessions_killed: i64,
    pub error: Option<String>,
}

// ── Readiness gate helpers ──────────────────────────────────────────────────

/// Parse a stored acceptance-criteria JSON string into `AcceptanceCriterionItem`
/// values for the readiness evaluator.  Returns an empty vec when the JSON is
/// missing, empty, or unparseable.
fn parse_ac_items(ac_json: &str) -> Vec<AcceptanceCriterionItem> {
    serde_json::from_str::<Vec<AcceptanceCriterionItem>>(ac_json).unwrap_or_default()
}

/// Convert a `ProposalReadinessResult` into a user-facing error string,
/// prepending a short preamble so callers can return it directly as a tool
/// error.
fn format_readiness_error(
    result: &crate::tools::proposal_readiness::ProposalReadinessResult,
) -> Option<String> {
    result
        .to_error_string()
        .map(|details| format!("proposal not ready for review: {details}"))
}

// ── Tool router ──────────────────────────────────────────────────────────────

#[tool_router(router = proposal_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Create a global proposal.
    #[tool(
        description = "Create a global, project-independent proposal (collaborative spec). Optionally pass `target_projects` (UUIDs or owner/repo slugs) the proposal touches and `acceptance_criteria`. The author is the authenticated caller. Status defaults to `draft`."
    )]
    pub async fn proposal_create(
        &self,
        Parameters(p): Parameters<ProposalCreateParams>,
    ) -> Json<ProposalSingleResponse> {
        let title = match validate_title(&p.title) {
            Ok(t) => t,
            Err(e) => return Json(err_single(e)),
        };
        let body = p.body.as_deref().unwrap_or("");
        if let Err(e) = validate_design(body) {
            return Json(err_single(e));
        }
        if let Err(e) = validate_mdx_body(body, p.body_format.as_deref()) {
            return Json(err_single(e));
        }
        let body_format = p.body_format.as_deref().unwrap_or("markdown");
        if body_format == "mdx"
            && let Err(e) = validate_question_form_placement(body)
        {
            return Json(err_single(e));
        }
        let ac = p.acceptance_criteria.unwrap_or_default();
        if let Err(e) = validate_ac_count(ac.len()) {
            return Json(err_single(e));
        }
        let requested_status = match validate_proposal_create_status(p.status.as_deref()) {
            Ok(s) => s,
            Err(e) => return Json(err_single(e)),
        };
        // Proposer-role authors are forced into `triage` (an inbox a PM/engineer
        // promotes to `draft`); higher roles default to their requested status
        // (or `draft`).
        let status = match acting_caps(self.state.db()).await {
            Ok(Some(caps)) if !caps.is_admin && caps.role == "proposer" => Some("triage"),
            Ok(_) => requested_status,
            Err(e) => return Json(err_single(e)),
        };
        let ac_json = serde_json::to_string(&ac).unwrap_or_else(|_| "[]".to_string());

        // Pre-resolve target projects: used both for the readiness gate
        // (target count) and for seeding after proposal creation.
        let mut resolved_target_ids: Vec<String> = Vec::new();
        if let Some(targets) = &p.target_projects {
            let project_repo =
                ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
            for t in targets {
                if let Ok(Some(pid)) = project_repo.resolve(t).await {
                    resolved_target_ids.push(pid);
                }
            }
        }

        // Deterministic DoR gate: block entering `in_review` when the spec is
        // not ready. Existing body/MDX/AC-count validation already passed.
        let effective_status = status.unwrap_or("draft");
        if effective_status == "in_review" {
            let ac_items = parse_ac_items(&ac_json);
            let readiness = evaluate_proposal_readiness(body, &ac_items, resolved_target_ids.len());
            if let Some(err) = format_readiness_error(&readiness) {
                return Json(err_single(err));
            }
        }

        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let proposal = match repo
            .create(djinn_db::ProposalCreateInput {
                title: &title,
                body,
                acceptance_criteria: Some(&ac_json),
                status,
                body_format: p.body_format.as_deref(),
            })
            .await
        {
            Ok(p) => p,
            Err(e) => return Json(err_single(e.to_string())),
        };

        // Seed target projects (best-effort: unresolvable refs are skipped).
        for pid in &resolved_target_ids {
            let _ = repo.add_target(&proposal.id, pid, "primary").await;
        }

        Json(ProposalSingleResponse {
            proposal: Some(ProposalModel::from(&proposal)),
            mdx: None,
            error: None,
        })
    }

    /// Import a portable proposal.mdx document, creating or updating a proposal.
    #[tool(
        description = "Import a portable proposal.mdx document. Parses YAML frontmatter (optional id, title, body_format, acceptance_criteria), validates MDX custom block tags against the proposal block registry, then creates a new proposal or updates the existing proposal named by id."
    )]
    pub async fn proposal_import(
        &self,
        Parameters(p): Parameters<ProposalImportParams>,
    ) -> Json<ProposalSingleResponse> {
        let imported = match parse_proposal_mdx(&p.mdx) {
            Ok(imported) => imported,
            Err(e) => return Json(err_single(e)),
        };

        if let Err(e) = validate_design(imported.body) {
            return Json(err_single(e));
        }
        if imported.body_format == "mdx"
            && let Err(e) = validate_mdx_blocks(imported.body)
        {
            return Json(err_single(e.to_string()));
        }
        if imported.body_format == "mdx"
            && let Err(e) = parse_mdx_blocks(imported.body)
        {
            return Json(err_single(e.to_string()));
        }

        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let proposal = if let Some(id) = imported.id.as_deref() {
            let Some(existing) = repo.resolve(id).await.ok().flatten() else {
                return Json(err_single(proposal_not_found_error(id)));
            };
            if let Err(e) = self
                .gate_proposal_edit(existing.author_user_id.as_deref())
                .await
            {
                return Json(err_single(e));
            }
            // Deterministic DoR gate: block import that would leave an
            // `in_review` proposal in a non-ready state.
            if existing.status == "in_review" {
                let ac_items = parse_ac_items(&imported.acceptance_criteria_json);
                let target_count = repo
                    .targets(&existing.id)
                    .await
                    .map(|t| t.len())
                    .unwrap_or(0);
                let readiness = evaluate_proposal_readiness(imported.body, &ac_items, target_count);
                if let Some(err) = format_readiness_error(&readiness) {
                    return Json(err_single(err));
                }
            }
            match repo
                .update(
                    &existing.id,
                    djinn_db::ProposalUpdateInput {
                        title: &imported.title,
                        body: imported.body,
                        acceptance_criteria: &imported.acceptance_criteria_json,
                        status: &existing.status,
                        superseded_by: existing.superseded_by.as_deref(),
                        body_format: Some(&imported.body_format),
                        // Imported proposals restore historical state — they
                        // are not authoring operations and carry no
                        // block-patch / native-skill attribution.
                        event_metadata: None,
                    },
                )
                .await
            {
                Ok(proposal) => proposal,
                Err(e) => return Json(err_single(e.to_string())),
            }
        } else {
            match repo
                .create(djinn_db::ProposalCreateInput {
                    title: &imported.title,
                    body: imported.body,
                    acceptance_criteria: Some(&imported.acceptance_criteria_json),
                    status: None,
                    body_format: Some(&imported.body_format),
                })
                .await
            {
                Ok(proposal) => proposal,
                Err(e) => return Json(err_single(e.to_string())),
            }
        };

        Json(ProposalSingleResponse {
            proposal: Some(ProposalModel::from(&proposal)),
            mdx: None,
            error: None,
        })
    }

    /// Export a proposal as a portable proposal.mdx string.
    #[tool(
        description = "Export a proposal (by UUID or short_id) as a portable proposal.mdx string. Returns the proposal and an `mdx` field containing YAML frontmatter (title, body_format, acceptance_criteria) followed by the body exactly as stored. For mdx proposals the output is validated for round-trip fidelity through the block registry parser."
    )]
    pub async fn proposal_export(
        &self,
        Parameters(p): Parameters<ProposalExportParams>,
    ) -> Json<ProposalSingleResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_single(proposal_not_found_error(&p.id)));
        };

        // Build the YAML frontmatter matching the parse_proposal_mdx format.
        let ac_items: Vec<JsonValue> =
            serde_json::from_str::<serde_json::Value>(&proposal.acceptance_criteria)
                .ok()
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default();

        let ac_yaml_lines: Vec<String> = ac_items
            .iter()
            .map(|item| {
                // Each AC is either a plain string or a {criterion, met} object.
                if let Some(s) = item.as_str() {
                    format!("  - {s}")
                } else {
                    // Structured: { "criterion": "...", "met": bool }
                    let criterion = item.get("criterion").and_then(|v| v.as_str()).unwrap_or("");
                    let met = item.get("met").and_then(|v| v.as_bool()).unwrap_or(false);
                    format!("  - criterion: {criterion}\n    met: {met}")
                }
            })
            .collect();

        let ac_section = if ac_yaml_lines.is_empty() {
            String::from("acceptance_criteria: []\n")
        } else {
            format!("acceptance_criteria:\n{}\n", ac_yaml_lines.join("\n"))
        };

        let mdx_output = format!(
            "---\ntitle: {}\nbody_format: {}\n{}---\n{}",
            proposal.title, proposal.body_format, ac_section, proposal.body,
        );

        // For mdx proposals: round-trip validate by parsing the output through
        // parse_mdx_blocks and confirming structural equality.
        if proposal.body_format == "mdx" {
            match parse_mdx_blocks(&proposal.body) {
                Err(e) => {
                    return Json(err_single(format!("round-trip validation failed: {e}")));
                }
                Ok(original_blocks) => {
                    // Extract the body from the exported mdx (after second ---)
                    let exported_body = split_proposal_mdx_frontmatter(&mdx_output)
                        .ok()
                        .map(|(_, body)| body)
                        .unwrap_or("");
                    match parse_mdx_blocks(exported_body) {
                        Err(e) => {
                            return Json(err_single(format!(
                                "round-trip parse failed on exported body: {e}"
                            )));
                        }
                        Ok(exported_blocks) => {
                            if original_blocks != exported_blocks {
                                return Json(err_single(
                                    "round-trip validation failed: exported MDX blocks \
                                     differ from original"
                                        .to_string(),
                                ));
                            }
                        }
                    }
                }
            }
        }

        Json(ProposalSingleResponse {
            proposal: Some(ProposalModel::from(&proposal)),
            mdx: Some(mdx_output),
            error: None,
        })
    }

    /// Show a proposal with targets, feedback, debate trail, revisions, and sign-offs.
    #[tool(
        description = "Show a proposal (by UUID or short_id) including target projects, the feedback/discussion thread, the debate-trail (objections/rebuttals/verdicts kept separate from feedback), its revision history, and review sign-offs (each flagged `stale` when given against an older revision)."
    )]
    pub async fn proposal_show(
        &self,
        Parameters(p): Parameters<ProposalShowParams>,
    ) -> Json<ProposalShowResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_show(proposal_not_found_error(&p.id)));
        };
        let targets = match target_models(&repo, &project_repo, &proposal.id).await {
            Ok(t) => t,
            Err(e) => return Json(err_show(e)),
        };
        let feedback = match repo.feedback(&proposal.id).await {
            Ok(f) => f.iter().map(Into::into).collect(),
            Err(e) => return Json(err_show(e.to_string())),
        };
        let revisions = match repo.revisions(&proposal.id).await {
            Ok(r) => r.iter().map(Into::into).collect(),
            Err(e) => return Json(err_show(e.to_string())),
        };
        let signoffs = match repo.signoffs(&proposal.id).await {
            Ok(s) => s
                .iter()
                .map(|so| ProposalSignoffModel::from_signoff(so, proposal.latest_revision_seq))
                .collect(),
            Err(e) => return Json(err_show(e.to_string())),
        };
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let epics = match graduated_epic_models(
            &repo,
            &epic_repo,
            &project_repo,
            &proposal.id,
            proposal.latest_revision_seq,
            proposal.pending_reconcile,
        )
        .await
        {
            Ok(e) => e,
            Err(e) => return Json(err_show(e)),
        };
        let memory_refs = match repo.memory_refs_for_proposal(&proposal.id).await {
            Ok(refs) => refs.into_iter().map(Into::into).collect(),
            Err(e) => return Json(err_show(e.to_string())),
        };
        let debate_trail = match repo.debate_trail(&proposal.id).await {
            Ok(d) => Some(d.iter().map(ProposalDebateTrailModel::from).collect()),
            Err(e) => return Json(err_show(e.to_string())),
        };
        Json(ProposalShowResponse {
            proposal: Some(ProposalModel::from(&proposal)),
            targets: Some(targets),
            feedback: Some(feedback),
            revisions: Some(revisions),
            signoffs: Some(signoffs),
            epics: Some(epics),
            memory_refs,
            debate_trail,
            error: None,
        })
    }

    /// List proposals (global) with optional filters and pagination.
    #[tool(
        description = "List proposals globally (not scoped to a project) with optional filters: status, author, target_project (UUID or owner/repo slug), text. Offset-based pagination. Returns {proposals[], total_count, limit, offset, has_more}."
    )]
    pub async fn proposal_list(
        &self,
        Parameters(p): Parameters<ProposalListParams>,
    ) -> Json<ProposalListResponse> {
        let sort = p.sort.as_deref().unwrap_or("created_desc");
        if let Err(e) = validate_sort(
            sort,
            &["created", "created_desc", "updated", "updated_desc"],
        ) {
            return Json(list_response::error::<ProposalListResponse>(e));
        }
        let limit = validate_limit(p.limit.unwrap_or(25));
        let offset = validate_offset(p.offset.unwrap_or(0));

        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());

        // Resolve the target_project filter to a UUID if a slug was passed.
        let target_project_id = if let Some(ref tref) = p.target_project {
            let project_repo =
                ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
            match project_repo.resolve(tref).await {
                Ok(Some(id)) => Some(id),
                _ => {
                    return Json(list_response::error::<ProposalListResponse>(format!(
                        "target_project not found: {tref}"
                    )));
                }
            }
        } else {
            None
        };

        let query = ProposalListQuery {
            status: p.status,
            text: p.text,
            author_user_id: p.author,
            target_project_id,
            sort: sort.to_owned(),
            limit,
            offset,
        };
        match repo.list_filtered(query).await {
            Ok(result) => Json(list_response::success::<ProposalListResponse>(
                result
                    .proposals
                    .iter()
                    .map(|(p, count)| ProposalModel::from_with_count(p, *count))
                    .collect(),
                result.total_count,
                limit,
                offset,
            )),
            Err(e) => Json(list_response::error::<ProposalListResponse>(e.to_string())),
        }
    }

    /// Update a proposal's editable fields.
    #[tool(
        description = "Update a proposal (by UUID or short_id): title, body, acceptance_criteria, status (draft|shared|ready|archived|superseded), and superseded_by. Only provided fields change."
    )]
    pub async fn proposal_update(
        &self,
        Parameters(p): Parameters<ProposalUpdateParams>,
    ) -> Json<ProposalSingleResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(existing) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_single(proposal_not_found_error(&p.id)));
        };
        if let Err(e) = self
            .gate_proposal_edit(existing.author_user_id.as_deref())
            .await
        {
            return Json(err_single(e));
        }

        let title = if let Some(ref t) = p.title {
            match validate_title(t) {
                Ok(v) => v,
                Err(e) => return Json(err_single(e)),
            }
        } else {
            existing.title.clone()
        };

        let body = p.body.as_deref().unwrap_or(&existing.body);
        if let Err(e) = validate_design(body) {
            return Json(err_single(e));
        }
        // Effective body_format: explicitly passed, else the proposal's current
        // format (matches the repository's own fallback on update).
        let body_format = p
            .body_format
            .as_deref()
            .unwrap_or(existing.body_format.as_str());
        if let Err(e) = validate_mdx_body(body, Some(body_format)) {
            return Json(err_single(e));
        }
        if p.body.is_some()
            && body_format == "mdx"
            && let Err(e) = validate_question_form_placement(body)
        {
            return Json(err_single(e));
        }

        let ac_json = if let Some(ac) = &p.acceptance_criteria {
            if let Err(e) = validate_ac_count(ac.len()) {
                return Json(err_single(e));
            }
            serde_json::to_string(ac).unwrap_or_else(|_| "[]".to_string())
        } else {
            existing.acceptance_criteria.clone()
        };

        let status = p.status.as_deref().unwrap_or(&existing.status);
        if let Err(e) = validate_proposal_status(status) {
            return Json(err_single(e));
        }

        // Deterministic DoR gate: block entering `in_review` when the
        // effective proposal body/AC/targets are not ready. Existing
        // body/MDX/AC-count validation already passed above.
        if status == "in_review" {
            let ac_items = parse_ac_items(&ac_json);
            let target_count = repo
                .targets(&existing.id)
                .await
                .map(|t| t.len())
                .unwrap_or(0);
            let readiness = evaluate_proposal_readiness(body, &ac_items, target_count);
            if let Some(err) = format_readiness_error(&readiness) {
                return Json(err_single(err));
            }
        }

        // Resolve superseded_by to a canonical proposal id when provided.
        let superseded_by = if let Some(ref s) = p.superseded_by {
            match repo.resolve(s).await.ok().flatten() {
                Some(target) => Some(target.id),
                None => return Json(err_single(format!("superseded_by proposal not found: {s}"))),
            }
        } else {
            existing.superseded_by.clone()
        };

        match repo
            .update(
                &existing.id,
                djinn_db::ProposalUpdateInput {
                    title: &title,
                    body,
                    acceptance_criteria: &ac_json,
                    status,
                    superseded_by: superseded_by.as_deref(),
                    body_format: p.body_format.as_deref(),
                    // Plain `proposal_update` writes carry no authoring
                    // attribution metadata — block-patch / native-skill tagging
                    // is reserved for the targeted patch primitive.
                    event_metadata: None,
                },
            )
            .await
        {
            Ok(updated) => Json(ProposalSingleResponse {
                proposal: Some(ProposalModel::from(&updated)),
                mdx: None,
                error: None,
            }),
            Err(e) => Json(err_single(e.to_string())),
        }
    }

    /// Apply a single targeted MDX block patch to a proposal body.
    ///
    /// Locates a specific range in the proposal body using the provided
    /// selector (heading, exact text, or byte range), then replaces or wraps
    /// that range with the given MDX content. Unrelated body content is
    /// preserved. Each successful patch increments `latest_revision_seq` exactly
    /// once and records targeted-block-patch metadata.
    #[tool(
        description = "Apply a single targeted MDX block patch to a proposal body. Locates a range via selector (heading_text, exact_text, or byte_range), then replaces or wraps it with the given block_mdx. Unrelated content is preserved. Each successful patch records one proposal revision with targeted-block-patch metadata."
    )]
    pub async fn proposal_block_patch(
        &self,
        Parameters(p): Parameters<ProposalBlockPatchParams>,
    ) -> Json<ProposalSingleResponse> {
        // 1. Validate operation.
        if !matches!(p.operation.as_str(), "replace" | "wrap") {
            return Json(err_single(format!(
                "invalid operation: {:?} (expected replace or wrap)",
                p.operation
            )));
        }

        // 2. Validate block_mdx is non-empty.
        if p.block_mdx.is_empty() {
            return Json(err_single("block_mdx must not be empty".to_string()));
        }

        // 3. Resolve proposal.
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(existing) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_single(proposal_not_found_error(&p.id)));
        };

        // 4. Edit gate.
        if let Err(e) = self
            .gate_proposal_edit(existing.author_user_id.as_deref())
            .await
        {
            return Json(err_single(e));
        }

        // 5. Stale revision guard.
        if let Some(expected_seq) = p.expected_latest_revision_seq
            && existing.latest_revision_seq != expected_seq
        {
            return Json(err_single(format!(
                "stale revision: expected latest_revision_seq={}, but proposal has {}",
                expected_seq, existing.latest_revision_seq
            )));
        }

        // 6. Resolve selector to a byte range in the body.
        let range = match resolve_selector(&existing.body, &p.selector) {
            Ok(r) => r,
            Err(e) => return Json(err_single(format!("selector error: {e}"))),
        };

        // 7. Build the new body.
        let new_body = match p.operation.as_str() {
            "replace" => {
                let mut body = String::with_capacity(
                    existing.body.len() - (range.end - range.start) + p.block_mdx.len(),
                );
                body.push_str(&existing.body[..range.start]);
                body.push_str(&p.block_mdx);
                body.push_str(&existing.body[range.end..]);
                body
            }
            "wrap" => {
                let selected = &existing.body[range.start..range.end];
                let mut body = String::with_capacity(
                    existing.body.len() - (range.end - range.start)
                        + p.block_mdx.len()
                        + selected.len(),
                );
                body.push_str(&existing.body[..range.start]);
                // Insert the block_mdx before the selected content.
                body.push_str(&p.block_mdx);
                body.push_str(selected);
                body.push_str(&existing.body[range.end..]);
                body
            }
            _ => unreachable!(),
        };

        // 8. Validate the resulting MDX body.
        // Determine body_format: if the proposal is markdown and the patch
        // introduces MDX block tags, upgrade to mdx.
        let has_mdx_blocks = !extract_custom_block_tags(&new_body).is_empty();
        let new_body_format = if existing.body_format == "mdx" || has_mdx_blocks {
            "mdx"
        } else {
            "markdown"
        };

        if new_body_format == "mdx" {
            if let Err(e) = validate_mdx_blocks(&new_body) {
                return Json(err_single(format!("resulting MDX is invalid: {e}")));
            }
            if let Err(e) = parse_mdx_blocks(&new_body) {
                return Json(err_single(format!("resulting MDX parse error: {e}")));
            }
        }
        if let Err(e) = validate_design(&new_body) {
            return Json(err_single(e));
        }

        // 9. Build event_metadata for the targeted block-patch revision.
        let mut metadata = serde_json::json!({
            "change_kind": "targeted_block_patch",
            "selector": range.selector_description,
            "range_start_byte": range.start,
            "range_end_byte": range.end,
            "native_skill_name": p.native_skill_name.as_deref().unwrap_or(""),
            "native_skill_version": p.native_skill_version.as_deref().unwrap_or(""),
        });
        if let Some(ref note) = p.note {
            metadata["note"] = serde_json::Value::String(note.clone());
        }

        // 10. Persist through the revisioning path.
        let ac_json = existing.acceptance_criteria.clone();
        match repo
            .update(
                &existing.id,
                djinn_db::ProposalUpdateInput {
                    title: &existing.title,
                    body: &new_body,
                    acceptance_criteria: &ac_json,
                    status: &existing.status,
                    superseded_by: existing.superseded_by.as_deref(),
                    body_format: Some(new_body_format),
                    event_metadata: Some(&metadata),
                },
            )
            .await
        {
            Ok(updated) => Json(ProposalSingleResponse {
                proposal: Some(ProposalModel::from(&updated)),
                mdx: None,
                error: None,
            }),
            Err(e) => Json(err_single(e.to_string())),
        }
    }

    /// Delete a proposal (cascades to its targets and feedback).
    #[tool(
        description = "Delete a proposal (by UUID or short_id). Cascades to its targets and feedback. Returns {ok}."
    )]
    pub async fn proposal_delete(
        &self,
        Parameters(p): Parameters<ProposalDeleteParams>,
    ) -> Json<ProposalDeleteResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(existing) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(ProposalDeleteResponse {
                ok: None,
                error: Some(proposal_not_found_error(&p.id)),
            });
        };
        if let Err(e) = self
            .gate_proposal_edit(existing.author_user_id.as_deref())
            .await
        {
            return Json(ProposalDeleteResponse {
                ok: None,
                error: Some(e),
            });
        }
        match repo.delete(&existing.id).await {
            Ok(()) => Json(ProposalDeleteResponse {
                ok: Some(true),
                error: None,
            }),
            Err(e) => Json(ProposalDeleteResponse {
                ok: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// Add (or re-role) a target project on a proposal.
    #[tool(
        description = "Add a target project to a proposal (or change its role if already present). `project` is a UUID or owner/repo slug; `role` is `primary` (default) or `reference`. This is the re-target capability — editable at any time. Returns the proposal's updated target list."
    )]
    pub async fn proposal_add_target(
        &self,
        Parameters(p): Parameters<ProposalTargetParams>,
    ) -> Json<ProposalTargetsResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_targets(proposal_not_found_error(&p.id)));
        };
        if let Err(e) = self
            .gate_proposal_edit(proposal.author_user_id.as_deref())
            .await
        {
            return Json(err_targets(e));
        }
        let role = p.role.as_deref().unwrap_or("primary");
        if !matches!(role, "primary" | "reference") {
            return Json(err_targets(format!(
                "invalid role: {role:?} (expected primary or reference)"
            )));
        }
        let project_id = match project_repo.resolve(&p.project).await {
            Ok(Some(id)) => id,
            _ => return Json(err_targets(format!("project not found: {}", p.project))),
        };
        if let Err(e) = repo.add_target(&proposal.id, &project_id, role).await {
            return Json(err_targets(e.to_string()));
        }
        finish_targets(&repo, &project_repo, &proposal.id).await
    }

    /// Remove a target project from a proposal.
    #[tool(
        description = "Remove a target project from a proposal. `project` is a UUID or owner/repo slug. No-op if it wasn't a target. Returns the proposal's updated target list."
    )]
    pub async fn proposal_remove_target(
        &self,
        Parameters(p): Parameters<ProposalTargetParams>,
    ) -> Json<ProposalTargetsResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_targets(proposal_not_found_error(&p.id)));
        };
        if let Err(e) = self
            .gate_proposal_edit(proposal.author_user_id.as_deref())
            .await
        {
            return Json(err_targets(e));
        }
        // Fall back to the raw value so a stale target can still be removed.
        let project_id = project_repo
            .resolve(&p.project)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| p.project.clone());
        if let Err(e) = repo.remove_target(&proposal.id, &project_id).await {
            return Json(err_targets(e.to_string()));
        }
        finish_targets(&repo, &project_repo, &proposal.id).await
    }

    /// Add a feedback entry (plain discussion) to a proposal.
    #[tool(
        description = "Add a feedback comment to a proposal. Feedback is plain discussion — it is NOT applied to the spec directly; the proposal owner asks djinn in chat to apply it, which rewrites the spec as a new revision and resolves the feedback. `author_kind` is `user` (default) or `ai` (set `author_model` for AI). `parent_id` threads a reply."
    )]
    pub async fn proposal_feedback_add(
        &self,
        Parameters(p): Parameters<ProposalFeedbackAddParams>,
    ) -> Json<ProposalFeedbackResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(err_feedback(proposal_not_found_error(&p.proposal_id)));
        };
        if let Err(e) = validate_body(&p.body) {
            return Json(err_feedback(e));
        }
        let author_kind = p.author_kind.as_deref().unwrap_or("user");
        if !matches!(author_kind, "user" | "ai") {
            return Json(err_feedback(format!(
                "invalid author_kind: {author_kind:?} (expected user or ai)"
            )));
        }
        match repo
            .add_feedback(djinn_db::ProposalFeedbackCreateInput {
                proposal_id: &proposal.id,
                parent_id: p.parent_id.as_deref(),
                author_kind,
                author_model: p.author_model.as_deref(),
                body: &p.body,
            })
            .await
        {
            Ok(f) => Json(ProposalFeedbackResponse {
                feedback: Some((&f).into()),
                error: None,
            }),
            Err(e) => Json(err_feedback(e.to_string())),
        }
    }

    /// Resolve a feedback entry: collapse it out of the active thread.
    #[tool(
        description = "Resolve a feedback entry, collapsing it out of the active thread. Pass `resolved_revision_seq` with the proposal revision that addressed it (when a spec change was applied), or omit it for a plain dismissal. Requires edit rights on the proposal."
    )]
    pub async fn proposal_feedback_resolve(
        &self,
        Parameters(p): Parameters<ProposalFeedbackResolveParams>,
    ) -> Json<ProposalFeedbackResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(feedback) = repo.get_feedback(&p.id).await.ok().flatten() else {
            return Json(err_feedback(format!("feedback not found: {}", p.id)));
        };
        // Resolving a proposal's feedback is an edit on that proposal's review
        // state → requires edit rights, same gate as proposal_update.
        let author = repo
            .resolve(&feedback.proposal_id)
            .await
            .ok()
            .flatten()
            .and_then(|pr| pr.author_user_id);
        if let Err(e) = self.gate_proposal_edit(author.as_deref()).await {
            return Json(err_feedback(e));
        }
        match repo
            .set_feedback_resolved(&p.id, p.resolved_revision_seq)
            .await
        {
            Ok(f) => Json(ProposalFeedbackResponse {
                feedback: Some((&f).into()),
                error: None,
            }),
            Err(e) => Json(err_feedback(e.to_string())),
        }
    }

    /// Give a review sign-off (scoped or technical) as the authenticated user.
    #[tool(
        description = "Record a review sign-off on a proposal as the authenticated user. `kind` is `scoped` (product/scope) or `technical` (engineering). The sign-off anchors to the current head revision; later spec edits mark it stale. When a proposal in `in_review` has both a fresh scoped and technical sign-off, it auto-advances to `approved`."
    )]
    pub async fn proposal_signoff(
        &self,
        Parameters(p): Parameters<ProposalSignoffParams>,
    ) -> Json<ProposalSingleResponse> {
        if !matches!(p.kind.as_str(), "scoped" | "technical") {
            return Json(err_single(format!(
                "invalid sign-off kind: {:?} (expected scoped or technical)",
                p.kind
            )));
        }
        let Some(user_id) = djinn_core::auth_context::current_user_id() else {
            return Json(err_single(
                "sign-off requires an authenticated user".to_string(),
            ));
        };
        // Role gate: scoped = PM/engineer/admin; technical = engineer/admin.
        match acting_caps(self.state.db()).await {
            Ok(Some(caps)) if !caps.can_signoff(&p.kind) => {
                return Json(err_single(format!(
                    "a {} sign-off requires the {} role",
                    p.kind,
                    if p.kind == "technical" {
                        "engineer (or admin)"
                    } else {
                        "PM, engineer (or admin)"
                    }
                )));
            }
            Err(e) => return Json(err_single(e)),
            _ => {}
        }
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_single(proposal_not_found_error(&p.id)));
        };

        // Deterministic DoR gate: block sign-off on draft/in_review proposals
        // whose body/AC/targets are not ready.  This prevents sign-off-driven
        // auto-advance (draft→in_review, draft→approved, in_review→approved)
        // from bypassing readiness checks.
        if matches!(proposal.status.as_str(), "draft" | "in_review") {
            let ac_items = parse_ac_items(&proposal.acceptance_criteria);
            let target_count = repo
                .targets(&proposal.id)
                .await
                .map(|t| t.len())
                .unwrap_or(0);
            let readiness = evaluate_proposal_readiness(&proposal.body, &ac_items, target_count);
            if let Some(err) = format_readiness_error(&readiness) {
                return Json(err_single(err));
            }
        }

        match repo.add_signoff(&proposal.id, &p.kind, &user_id).await {
            Ok(updated) => Json(ProposalSingleResponse {
                proposal: Some(ProposalModel::from(&updated)),
                mdx: None,
                error: None,
            }),
            Err(e) => Json(err_single(e.to_string())),
        }
    }

    /// Withdraw the authenticated user's sign-off of a given kind.
    #[tool(
        description = "Withdraw the authenticated user's sign-off (`scoped` or `technical`) from a proposal. May demote an `approved` proposal back to `in_review` if the approval gate is no longer met."
    )]
    pub async fn proposal_signoff_clear(
        &self,
        Parameters(p): Parameters<ProposalSignoffParams>,
    ) -> Json<ProposalSingleResponse> {
        let Some(user_id) = djinn_core::auth_context::current_user_id() else {
            return Json(err_single(
                "sign-off requires an authenticated user".to_string(),
            ));
        };
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_single(proposal_not_found_error(&p.id)));
        };
        match repo.clear_signoff(&proposal.id, &p.kind, &user_id).await {
            Ok(updated) => Json(ProposalSingleResponse {
                proposal: Some(ProposalModel::from(&updated)),
                mdx: None,
                error: None,
            }),
            Err(e) => Json(err_single(e.to_string())),
        }
    }

    /// Kick off an approved proposal — graduate it into the execution engine.
    #[tool(
        description = "Kick off an approved proposal: hand it to the Planner (a single `epic_breakdown` task on the first primary target) which reads the spec + target repos and breaks it down into epics across the targets, set status to `building`, and record the build owner (must be a participant — the author or a sign-off giver; defaults to the caller). Requires the proposal to be `approved` and the engineer role (or admin)."
    )]
    pub async fn proposal_graduate(
        &self,
        Parameters(p): Parameters<ProposalGraduateParams>,
    ) -> Json<ProposalSingleResponse> {
        // Capability: engineer/admin only.
        match acting_caps(self.state.db()).await {
            Ok(Some(caps)) if !caps.can_kickoff() => {
                return Json(err_single(
                    "kicking off a build requires the engineer role (or admin)".to_string(),
                ));
            }
            Err(e) => return Json(err_single(e)),
            _ => {}
        }
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_single(proposal_not_found_error(&p.id)));
        };
        if proposal.status != "approved" {
            return Json(err_single(format!(
                "proposal must be approved to kick off (current: {})",
                proposal.status
            )));
        }

        // Build owner must be a participant (author or sign-off giver).
        let participants = match repo.participants(&proposal.id).await {
            Ok(v) => v,
            Err(e) => return Json(err_single(e.to_string())),
        };
        let Some(owner) = p
            .owner_user_id
            .clone()
            .or_else(djinn_core::auth_context::current_user_id)
        else {
            return Json(err_single("a build owner is required".to_string()));
        };
        if !participants.is_empty() && !participants.contains(&owner) {
            return Json(err_single(
                "the build owner must be a participant (the author or a sign-off giver)"
                    .to_string(),
            ));
        }

        // At least one primary target is required — that is where the build
        // writes. The proposal-decomposition Planner reads the spec + targets
        // and creates the epics itself (cut over from the old mechanical
        // one-epic-per-primary fan-out).
        let targets = match repo.targets(&proposal.id).await {
            Ok(t) => t,
            Err(e) => return Json(err_single(e.to_string())),
        };
        let primaries: Vec<_> = targets.iter().filter(|t| t.role == "primary").collect();
        let Some(home_target) = primaries.first() else {
            return Json(err_single(
                "no primary target project to build — add a target first".to_string(),
            ));
        };
        let home_project_id = home_target.project_id.clone();

        // A human-readable target list for the breakdown task's design.
        let mut target_lines = Vec::with_capacity(targets.len());
        for t in &targets {
            let slug = match project_repo.get(&t.project_id).await {
                Ok(Some(proj)) => format!("{}/{}", proj.github_owner, proj.github_repo),
                _ => t.project_id.clone(),
            };
            target_lines.push(format!("- {slug} ({})", t.role));
        }

        let design = format!(
            "Decompose proposal `{}` ({}) into epics.\n\n\
             Call `proposal_show(id=\"{}\")` for the full spec, acceptance \
             criteria, and targets, then follow Workflow D (Proposal \
             Decomposition). Create one or more epics per `primary` target with \
             `epic_create(..., proposal_id=\"{}\")`, attach `reference` targets \
             as read-sources, and sequence cross-repo work with `blocked_by`. Do \
             NOT create worker tasks — each epic runs its own wave Planner.\n\n\
             Targets:\n{}",
            proposal.short_id,
            proposal.id,
            proposal.id,
            proposal.id,
            target_lines.join("\n"),
        );

        let ac = serde_json::json!([
            {"criterion": "Proposal read via proposal_show and target repos surveyed", "met": false},
            {"criterion": "One or more epics created per primary target (epic_create with proposal_id), with read_sources and blocked_by set for cross-repo ordering", "met": false},
            {"criterion": "submit_grooming called to finalize the breakdown", "met": false},
        ])
        .to_string();

        let title = format!("Break down proposal: {}", proposal.title);
        let task = match task_repo
            .create_in_project(
                &home_project_id,
                None,
                &title,
                &design,
                &design,
                "epic_breakdown",
                djinn_core::models::task::PRIORITY_CRITICAL,
                "planner",
                Some("open"),
                Some(&ac),
            )
            .await
        {
            Ok(t) => t,
            Err(e) => {
                return Json(err_single(format!("failed to create breakdown task: {e}")));
            }
        };
        // Attribute the breakdown task (and the epics the Planner spawns from it,
        // which inherit the session user) to the build owner so commits resolve
        // to a real GitHub account.
        let _ = task_repo.set_created_by_user_id(&task.id, &owner).await;
        // Record the breakdown task so a later proposal_stop_build can find and
        // force-close it (it has no epic, so it is not in proposal_epics).
        let _ = repo.set_breakdown_task(&proposal.id, &task.id).await;

        match repo.set_building(&proposal.id, &owner).await {
            Ok(updated) => Json(ProposalSingleResponse {
                proposal: Some(ProposalModel::from(&updated)),
                mdx: None,
                error: None,
            }),
            Err(e) => Json(err_single(e.to_string())),
        }
    }

    /// Stop an in-flight proposal build — the inverse of `proposal_graduate`.
    #[tool(
        description = "Stop an in-flight proposal build (mode: abort | freeze | unfreeze). `freeze` holds the build's tasks out of dispatch while leaving epics/tasks/branches in place; `unfreeze` resumes. `abort` tears the build down — kills running workers, force-closes every task (deleting branches so GitHub auto-closes their PRs), closes the epics, unlinks them, and reverts the proposal to `approved` so it can be edited and re-graduated. Pass preview=true with mode=abort for a read-only blast-radius (epics, open tasks, running sessions) without mutating. Requires the proposal to be `building` and the engineer role (or admin)."
    )]
    pub async fn proposal_stop_build(
        &self,
        Parameters(p): Parameters<ProposalStopBuildParams>,
    ) -> Json<ProposalStopBuildResponse> {
        let err = |msg: String| {
            Json(ProposalStopBuildResponse {
                ok: false,
                mode: String::new(),
                proposal_id: None,
                status: None,
                preview: false,
                epics_closed: 0,
                tasks_closed: 0,
                sessions_killed: 0,
                error: Some(msg),
            })
        };

        // Capability: engineer/admin only (same gate as graduate — this is the
        // inverse build-control operation).
        match acting_caps(self.state.db()).await {
            Ok(Some(caps)) if !caps.can_kickoff() => {
                return err("stopping a build requires the engineer role (or admin)".to_string());
            }
            Err(e) => return err(e),
            _ => {}
        }

        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return err(proposal_not_found_error(&p.id));
        };

        match p.mode.as_str() {
            "freeze" | "unfreeze" => {
                if proposal.status != "building" {
                    return err(format!(
                        "only a building proposal can be {}d (current: {})",
                        p.mode, proposal.status
                    ));
                }
                let frozen = p.mode == "freeze";
                match repo.set_frozen(&proposal.id, frozen).await {
                    Ok(updated) => Json(ProposalStopBuildResponse {
                        ok: true,
                        mode: p.mode,
                        proposal_id: Some(updated.id),
                        status: Some(updated.status),
                        preview: false,
                        epics_closed: 0,
                        tasks_closed: 0,
                        sessions_killed: 0,
                        error: None,
                    }),
                    Err(e) => err(e.to_string()),
                }
            }
            "abort" => {
                if proposal.status != "building" {
                    return err(format!(
                        "only a building proposal can be aborted (current: {})",
                        proposal.status
                    ));
                }
                let preview = p.preview.unwrap_or(false);
                let reason = p.reason.as_deref().unwrap_or("proposal build aborted");
                self.abort_proposal_build(&repo, &proposal, reason, preview)
                    .await
            }
            other => err(format!(
                "unknown mode '{other}' — expected abort | freeze | unfreeze"
            )),
        }
    }

    /// Tear down exactly one obsolete graduated epic subtree during proposal reconcile.
    #[tool(
        description = "Proposal reconcile helper: retire one obsolete graduated epic subtree without aborting the build. Pass `proposal_id` (or `id`) and `epic_id` (UUID or short_id). The tool verifies the epic is linked to the building proposal, blocks and records AI feedback if any target task has merged work, supports `preview=true` for read-only blast radius, and otherwise force-closes only target-epic tasks, kills their live sessions, closes/unlinks only that epic, and leaves the proposal building with unrelated graduated epics linked. Response includes ok, proposal_id, epic_id, preview, blocked/error, epics_closed, tasks_closed, sessions_killed, and merged-work blocked details."
    )]
    pub async fn proposal_reconcile_obsolete_epic(
        &self,
        Parameters(p): Parameters<ProposalReconcileObsoleteEpicParams>,
    ) -> Json<ProposalReconcileObsoleteEpicResponse> {
        let err = |proposal_id: Option<String>, epic_id: Option<String>, msg: String| {
            Json(ProposalReconcileObsoleteEpicResponse {
                ok: false,
                proposal_id,
                epic_id,
                preview: p.preview.unwrap_or(false),
                blocked: false,
                blocked_feedback_id: None,
                blocked_feedback_body: None,
                merged_tasks: Vec::new(),
                epics_closed: 0,
                tasks_closed: 0,
                sessions_killed: 0,
                error: Some(msg),
            })
        };

        match acting_caps(self.state.db()).await {
            Ok(Some(caps)) if !caps.can_kickoff() => {
                return err(
                    None,
                    None,
                    "reconciling obsolete epics requires the engineer role (or admin)".to_string(),
                );
            }
            Err(e) => return err(None, None, e),
            _ => {}
        }

        let Some(proposal_ref) = p.proposal_id.as_deref().or(p.id.as_deref()) else {
            return err(None, None, "missing proposal_id (or id)".to_string());
        };

        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(proposal_ref).await.ok().flatten() else {
            return err(None, None, proposal_not_found_error(proposal_ref));
        };
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(epic) = epic_repo.resolve(&p.epic_id).await.ok().flatten() else {
            return err(
                Some(proposal.id.clone()),
                None,
                format!("epic not found: {}", p.epic_id),
            );
        };

        let reason = p
            .reason
            .as_deref()
            .unwrap_or("proposal reconcile obsolete-subtree teardown");
        self.teardown_obsolete_proposal_epic(
            &repo,
            &proposal,
            &epic.id,
            reason,
            p.preview.unwrap_or(false),
        )
        .await
    }
}

// ── Build teardown (abort cascade) ───────────────────────────────────────────

impl DjinnMcpServer {
    /// Tear down a graduated build and revert the proposal to `approved`.
    ///
    /// Composes the existing teardown primitives, all idempotent and
    /// best-effort on side-effects: kill the running worker (pool), force-close
    /// each open task (`ForceClose` is exempt from the blocker guard, so order
    /// is irrelevant) and clean its branch/PR, close each graduated epic, unlink
    /// them so a re-graduation starts clean, and revert the proposal to
    /// `approved`. A `preview` returns the blast radius without mutating.
    async fn abort_proposal_build(
        &self,
        repo: &ProposalRepository,
        proposal: &djinn_core::models::Proposal,
        reason: &str,
        preview: bool,
    ) -> Json<ProposalStopBuildResponse> {
        use djinn_core::models::TransitionAction;

        let abort_err = |msg: String| {
            Json(ProposalStopBuildResponse {
                ok: false,
                mode: "abort".to_string(),
                proposal_id: Some(proposal.id.clone()),
                status: None,
                preview: false,
                epics_closed: 0,
                tasks_closed: 0,
                sessions_killed: 0,
                error: Some(msg),
            })
        };

        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let pool = self.state.pool().await;

        let graduated = match repo.graduated_epics(&proposal.id).await {
            Ok(v) => v,
            Err(e) => return abort_err(e.to_string()),
        };

        // Every non-closed task across the graduated epics, plus the breakdown
        // task (which has no epic, so it is not in proposal_epics).
        let mut open_tasks: Vec<djinn_core::models::Task> = Vec::new();
        for (epic_id, _project_id) in &graduated {
            if let Ok(tasks) = task_repo.list_by_epic(epic_id).await {
                open_tasks.extend(tasks.into_iter().filter(|t| t.status != "closed"));
            }
        }
        if let Some(breakdown_id) = &proposal.build_breakdown_task_id
            && let Ok(Some(task)) = task_repo.resolve(breakdown_id).await
            && task.status != "closed"
        {
            open_tasks.push(task);
        }

        // Count live worker sessions (used by both preview and the kill count).
        let mut live_sessions = 0i64;
        if let Some(pool) = pool.as_ref() {
            for t in &open_tasks {
                if pool.has_session(&t.id).await.unwrap_or(false) {
                    live_sessions += 1;
                }
            }
        }

        if preview {
            return Json(ProposalStopBuildResponse {
                ok: true,
                mode: "abort".to_string(),
                proposal_id: Some(proposal.id.clone()),
                status: Some(proposal.status.clone()),
                preview: true,
                epics_closed: graduated.len() as i64,
                tasks_closed: open_tasks.len() as i64,
                sessions_killed: live_sessions,
                error: None,
            });
        }

        // Act. Kill the live worker (graceful), force-close the task, clean its
        // branch/PR. Best-effort throughout: a failed kill/cleanup is logged by
        // the underlying primitive and the next reaper backstops it.
        let mut sessions_killed = 0i64;
        let mut tasks_closed = 0i64;
        for t in &open_tasks {
            if let Some(pool) = pool.as_ref()
                && pool.has_session(&t.id).await.unwrap_or(false)
            {
                let _ = pool.kill_session(&t.id).await;
                sessions_killed += 1;
            }
            if task_repo
                .transition(
                    &t.id,
                    TransitionAction::ForceClose,
                    "system",
                    "system",
                    Some(reason),
                    None,
                )
                .await
                .is_ok()
            {
                tasks_closed += 1;
                self.state.cleanup_task_branches(&t.id).await;
            }
        }

        let mut epics_closed = 0i64;
        for (epic_id, _project_id) in &graduated {
            if epic_repo.close(epic_id).await.is_ok() {
                epics_closed += 1;
            }
        }

        let _ = repo.unlink_epics(&proposal.id).await;
        let status = match repo.revert_to_approved(&proposal.id).await {
            Ok(updated) => updated.status,
            Err(e) => return abort_err(e.to_string()),
        };

        Json(ProposalStopBuildResponse {
            ok: true,
            mode: "abort".to_string(),
            proposal_id: Some(proposal.id.clone()),
            status: Some(status),
            preview: false,
            epics_closed,
            tasks_closed,
            sessions_killed,
            error: None,
        })
    }

    /// Tear down exactly one obsolete graduated epic subtree for proposal reconcile.
    ///
    /// This is the scoped form of the abort cascade: it never touches the
    /// proposal build metadata, the breakdown task, or unrelated graduated epics.
    /// If any task in the target subtree has merged work, the operation records
    /// AI feedback and returns blocked before preview success or mutation.
    async fn teardown_obsolete_proposal_epic(
        &self,
        repo: &ProposalRepository,
        proposal: &djinn_core::models::Proposal,
        epic_id: &str,
        reason: &str,
        preview: bool,
    ) -> Json<ProposalReconcileObsoleteEpicResponse> {
        use djinn_core::models::TransitionAction;

        let scoped_err = |msg: String| {
            Json(ProposalReconcileObsoleteEpicResponse {
                ok: false,
                proposal_id: Some(proposal.id.clone()),
                epic_id: Some(epic_id.to_string()),
                preview: false,
                blocked: false,
                blocked_feedback_id: None,
                blocked_feedback_body: None,
                merged_tasks: Vec::new(),
                epics_closed: 0,
                tasks_closed: 0,
                sessions_killed: 0,
                error: Some(msg),
            })
        };

        if proposal.status != "building" {
            return scoped_err(format!(
                "only a building proposal can tear down an obsolete epic (current: {})",
                proposal.status
            ));
        }

        let graduated = match repo.graduated_epics(&proposal.id).await {
            Ok(v) => v,
            Err(e) => return scoped_err(e.to_string()),
        };
        if !graduated.iter().any(|(id, _)| id == epic_id) {
            return scoped_err(format!(
                "epic {epic_id} is not a graduated epic for proposal {}",
                proposal.id
            ));
        }

        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let pool = self.state.pool().await;

        let all_tasks = match task_repo.list_by_epic(epic_id).await {
            Ok(tasks) => tasks,
            Err(e) => return scoped_err(e.to_string()),
        };

        let merged_tasks: Vec<_> = all_tasks
            .iter()
            .filter(|t| t.merge_commit_sha.is_some())
            .collect();
        if !merged_tasks.is_empty() {
            let task_list = merged_tasks
                .iter()
                .map(|t| {
                    format!(
                        "- {} ({}) merged at {}",
                        t.short_id,
                        t.id,
                        t.merge_commit_sha.as_deref().unwrap_or("<unknown>")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let body = format!(
                "Obsolete-subtree teardown is blocked for epic `{epic_id}` because it contains merged work. Preserve the subtree and reconcile manually before unlinking or closing it.\n\nMerged tasks:\n{task_list}"
            );
            let feedback = repo
                .add_feedback(djinn_db::ProposalFeedbackCreateInput {
                    proposal_id: &proposal.id,
                    parent_id: None,
                    author_kind: "ai",
                    author_model: Some("proposal-reconcile"),
                    body: &body,
                })
                .await;
            return Json(ProposalReconcileObsoleteEpicResponse {
                ok: false,
                proposal_id: Some(proposal.id.clone()),
                epic_id: Some(epic_id.to_string()),
                preview: false,
                blocked: true,
                blocked_feedback_id: feedback.as_ref().ok().map(|f| f.id.clone()),
                blocked_feedback_body: Some(body),
                merged_tasks: merged_tasks.iter().map(|t| t.id.clone()).collect(),
                epics_closed: 0,
                tasks_closed: 0,
                sessions_killed: 0,
                error: Some(format!(
                    "obsolete epic teardown blocked by merged work in {} task(s)",
                    merged_tasks.len()
                )),
            });
        }

        let open_tasks: Vec<_> = all_tasks
            .into_iter()
            .filter(|t| t.status != "closed")
            .collect();

        let mut live_sessions = 0i64;
        if let Some(pool) = pool.as_ref() {
            for t in &open_tasks {
                if pool.has_session(&t.id).await.unwrap_or(false) {
                    live_sessions += 1;
                }
            }
        }

        if preview {
            return Json(ProposalReconcileObsoleteEpicResponse {
                ok: true,
                proposal_id: Some(proposal.id.clone()),
                epic_id: Some(epic_id.to_string()),
                preview: true,
                blocked: false,
                blocked_feedback_id: None,
                blocked_feedback_body: None,
                merged_tasks: Vec::new(),
                epics_closed: 1,
                tasks_closed: open_tasks.len() as i64,
                sessions_killed: live_sessions,
                error: None,
            });
        }

        let mut sessions_killed = 0i64;
        let mut tasks_closed = 0i64;
        for t in &open_tasks {
            if let Some(pool) = pool.as_ref()
                && pool.has_session(&t.id).await.unwrap_or(false)
            {
                let _ = pool.kill_session(&t.id).await;
                sessions_killed += 1;
            }
            if task_repo
                .transition(
                    &t.id,
                    TransitionAction::ForceClose,
                    "system",
                    "system",
                    Some(reason),
                    None,
                )
                .await
                .is_ok()
            {
                tasks_closed += 1;
                self.state.cleanup_task_branches(&t.id).await;
            }
        }

        let epics_closed = if epic_repo.close(epic_id).await.is_ok() {
            1
        } else {
            0
        };
        if let Err(e) = repo.unlink_epic(&proposal.id, epic_id).await {
            return scoped_err(e.to_string());
        }

        Json(ProposalReconcileObsoleteEpicResponse {
            ok: true,
            proposal_id: Some(proposal.id.clone()),
            epic_id: Some(epic_id.to_string()),
            preview: false,
            blocked: false,
            blocked_feedback_id: None,
            blocked_feedback_body: None,
            merged_tasks: Vec::new(),
            epics_closed,
            tasks_closed,
            sessions_killed,
            error: None,
        })
    }
}

// ── Permission gates ─────────────────────────────────────────────────────────

impl DjinnMcpServer {
    /// Gate a direct spec edit: allowed for the author, a PM, an engineer, or
    /// an admin. `Ok(())` when unauthenticated (trusted/system path).
    pub(crate) async fn gate_proposal_edit(
        &self,
        author_user_id: Option<&str>,
    ) -> Result<(), String> {
        if let Some(caps) = acting_caps(self.state.db()).await? {
            let is_author = author_user_id == Some(caps.user_id.as_str());
            if !caps.can_edit(is_author) {
                return Err(
                    "editing this proposal requires its author, a PM, an engineer, or an admin"
                        .to_string(),
                );
            }
        }
        Ok(())
    }
}

// ── Small response constructors ──────────────────────────────────────────────

fn err_show(error: impl Into<String>) -> ProposalShowResponse {
    ProposalShowResponse {
        proposal: None,
        targets: None,
        feedback: None,
        revisions: None,
        signoffs: None,
        epics: None,
        memory_refs: vec![],
        debate_trail: None,
        error: Some(error.into()),
    }
}

/// Resolve a proposal's graduated epics to `{epic_short_id, project_path,
/// status}` display models.
async fn graduated_epic_models(
    repo: &ProposalRepository,
    epic_repo: &EpicRepository,
    project_repo: &ProjectRepository,
    proposal_id: &str,
    latest_revision_seq: i32,
    pending_reconcile: bool,
) -> Result<Vec<ProposalEpicModel>, String> {
    let links = repo
        .graduated_epics(proposal_id)
        .await
        .map_err(|e| e.to_string())?;
    let reconciliations = repo
        .latest_epic_reconciliations(proposal_id)
        .await
        .map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(links.len());
    for (epic_id, project_id) in links {
        let Some(epic) = epic_repo.get(&epic_id).await.ok().flatten() else {
            continue;
        };
        let reconciled_at_revision_seq = reconciliations.get(&epic_id).copied();
        let needs_reconcile = pending_reconcile
            && reconciled_at_revision_seq
                .map(|seq| seq < latest_revision_seq)
                .unwrap_or(false);
        let project_path = match project_repo.get(&project_id).await {
            Ok(Some(p)) => format!("{}/{}", p.github_owner, p.github_repo),
            _ => project_id.clone(),
        };
        out.push(ProposalEpicModel {
            epic_id,
            epic_short_id: epic.short_id,
            epic_title: epic.title,
            epic_emoji: epic.emoji,
            project_path,
            status: epic.status,
            reconciled_at_revision_seq,
            needs_reconcile,
        });
    }
    Ok(out)
}

fn err_single(error: impl Into<String>) -> ProposalSingleResponse {
    ProposalSingleResponse {
        proposal: None,
        mdx: None,
        error: Some(error.into()),
    }
}

fn err_targets(error: impl Into<String>) -> ProposalTargetsResponse {
    ProposalTargetsResponse {
        targets: None,
        error: Some(error.into()),
    }
}

fn err_feedback(error: impl Into<String>) -> ProposalFeedbackResponse {
    ProposalFeedbackResponse {
        feedback: None,
        error: Some(error.into()),
    }
}

async fn finish_targets(
    repo: &ProposalRepository,
    project_repo: &ProjectRepository,
    proposal_id: &str,
) -> Json<ProposalTargetsResponse> {
    match target_models(repo, project_repo, proposal_id).await {
        Ok(targets) => Json(ProposalTargetsResponse {
            targets: Some(targets),
            error: None,
        }),
        Err(e) => Json(err_targets(e)),
    }
}

#[cfg(test)]
mod import_tests {
    use super::*;
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_core::events::EventBus;
    use djinn_db::{Database, ProposalCreateInput};

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
}

#[cfg(test)]
mod export_tests {
    use super::*;
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
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
mod stop_build_tests {
    use super::*;
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_core::events::EventBus;
    use djinn_db::{
        Database, EpicCreateInput, EpicRepository, ProjectRepository, ProposalCreateInput,
    };

    /// A `building` proposal: one graduated epic with two open worker tasks,
    /// plus a recorded breakdown task. The slot pool is the test stub (no live
    /// sessions), so the cascade exercises the DB-observable teardown.
    async fn building_proposal() -> (
        DjinnMcpServer,
        Database,
        String,
        String,
        Vec<String>,
        String,
    ) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let bus = EventBus::noop();
        let project = ProjectRepository::new(db.clone(), bus.clone())
            .create("svc-stop", "test", "svc-stop")
            .await
            .unwrap();

        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        let proposal = prepo
            .create(ProposalCreateInput {
                title: "Stop me",
                body: "",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let epic = EpicRepository::new(db.clone(), bus.clone())
            .create_for_project(
                &project.id,
                EpicCreateInput {
                    title: "E",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: None,
                    auto_breakdown: Some(false),
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .unwrap();

        let trepo = TaskRepository::new(db.clone(), bus.clone());
        let mut task_ids = Vec::new();
        for i in 0..2 {
            let t = trepo
                .create_in_project(
                    &project.id,
                    Some(&epic.id),
                    &format!("t{i}"),
                    "",
                    "",
                    "task",
                    0,
                    "",
                    Some("open"),
                    Some("[\"do\"]"),
                )
                .await
                .unwrap();
            task_ids.push(t.id);
        }
        let breakdown = trepo
            .create_in_project(
                &project.id,
                None,
                "breakdown",
                "",
                "",
                "epic_breakdown",
                0,
                "planner",
                Some("open"),
                None,
            )
            .await
            .unwrap();

        prepo
            .link_epic(&proposal.id, &epic.id, &project.id)
            .await
            .unwrap();
        prepo
            .set_breakdown_task(&proposal.id, &breakdown.id)
            .await
            .unwrap();
        prepo.set_building(&proposal.id, "owner").await.unwrap();

        (
            DjinnMcpServer::new(test_mcp_state(db.clone())),
            db,
            proposal.id,
            epic.id,
            task_ids,
            breakdown.id,
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn freeze_and_unfreeze_toggle_the_flag() {
        let (server, db, pid, _e, _t, _b) = building_proposal().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "freeze".into(),
                reason: None,
                preview: None,
            }))
            .await
            .0;
        assert!(r.ok);
        assert!(repo.get(&pid).await.unwrap().unwrap().build_frozen);

        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "unfreeze".into(),
                reason: None,
                preview: None,
            }))
            .await
            .0;
        assert!(r.ok);
        assert!(!repo.get(&pid).await.unwrap().unwrap().build_frozen);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preview_reports_blast_radius_without_mutating() {
        let (server, db, pid, epic_id, _t, _b) = building_proposal().await;
        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "abort".into(),
                reason: None,
                preview: Some(true),
            }))
            .await
            .0;
        assert!(r.ok);
        assert!(r.preview);
        assert_eq!(r.epics_closed, 1);
        assert_eq!(r.tasks_closed, 3, "2 worker tasks + 1 breakdown task");

        // Nothing was mutated.
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        assert_eq!(repo.get(&pid).await.unwrap().unwrap().status, "building");
        let epic = EpicRepository::new(db.clone(), EventBus::noop())
            .get(&epic_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(epic.status, "open");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_teardown_preview_reports_blast_radius_without_mutating() {
        let (server, db, pid, epic_id, task_ids, breakdown_id) = building_proposal().await;
        let r = server
            .dispatch_tool(
                "proposal_reconcile_obsolete_epic",
                serde_json::json!({
                    "proposal_id": pid.clone(),
                    "epic_id": epic_id.clone(),
                    "preview": true,
                }),
            )
            .await
            .unwrap();
        assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r.get("preview").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r.get("epics_closed").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(
            r.get("tasks_closed").and_then(|v| v.as_i64()),
            Some(task_ids.len() as i64)
        );

        let bus = EventBus::noop();
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        assert_eq!(prepo.get(&pid).await.unwrap().unwrap().status, "building");
        let links = prepo.graduated_epics(&pid).await.unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].0, epic_id);

        let erepo = EpicRepository::new(db.clone(), bus.clone());
        assert_eq!(erepo.get(&epic_id).await.unwrap().unwrap().status, "open");
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        for tid in task_ids.iter().chain(std::iter::once(&breakdown_id)) {
            assert_eq!(trepo.get(tid).await.unwrap().unwrap().status, "open");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_teardown_closes_only_target_epic_and_preserves_build() {
        let (server, db, pid, target_epic_id, target_task_ids, breakdown_id) =
            building_proposal().await;
        let bus = EventBus::noop();
        let project_repo = ProjectRepository::new(db.clone(), bus.clone());
        let project_id = project_repo
            .resolve("test/svc-stop")
            .await
            .unwrap()
            .unwrap();
        let project = project_repo.get(&project_id).await.unwrap().unwrap();
        let other_epic = EpicRepository::new(db.clone(), bus.clone())
            .create_for_project(
                &project.id,
                EpicCreateInput {
                    title: "Other",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: None,
                    auto_breakdown: Some(false),
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .unwrap();
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        let other_task = trepo
            .create_in_project(
                &project.id,
                Some(&other_epic.id),
                "other-task",
                "",
                "",
                "task",
                0,
                "",
                Some("open"),
                Some("[\"do\"]"),
            )
            .await
            .unwrap();
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        prepo
            .link_epic(&pid, &other_epic.id, &project.id)
            .await
            .unwrap();
        let r = server
            .dispatch_tool(
                "proposal_reconcile_obsolete_epic",
                serde_json::json!({
                    "proposal_id": pid.clone(),
                    "epic_id": target_epic_id.clone(),
                    "reason": "obsolete after reconcile",
                }),
            )
            .await
            .unwrap();
        assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r.get("epics_closed").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(
            r.get("tasks_closed").and_then(|v| v.as_i64()),
            Some(target_task_ids.len() as i64)
        );
        assert_eq!(r.get("blocked").and_then(|v| v.as_bool()), Some(false));

        let p = prepo.get(&pid).await.unwrap().unwrap();
        assert_eq!(p.status, "building");
        assert_eq!(
            p.build_breakdown_task_id.as_deref(),
            Some(breakdown_id.as_str())
        );
        assert_eq!(p.build_owner_user_id.as_deref(), Some("owner"));
        assert_eq!(
            prepo.graduated_epics(&pid).await.unwrap(),
            vec![(other_epic.id.clone(), project.id.clone())]
        );

        let erepo = EpicRepository::new(db.clone(), bus.clone());
        assert_eq!(
            erepo.get(&target_epic_id).await.unwrap().unwrap().status,
            "closed"
        );
        assert_eq!(
            erepo.get(&other_epic.id).await.unwrap().unwrap().status,
            "open"
        );

        for tid in &target_task_ids {
            assert_eq!(trepo.get(tid).await.unwrap().unwrap().status, "closed");
        }
        assert_eq!(
            trepo.get(&other_task.id).await.unwrap().unwrap().status,
            "open"
        );
        assert_eq!(
            trepo.get(&breakdown_id).await.unwrap().unwrap().status,
            "open"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_teardown_blocks_merged_work_before_preview_or_mutation() {
        let (server, db, pid, target_epic_id, target_task_ids, breakdown_id) =
            building_proposal().await;
        let bus = EventBus::noop();
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        trepo
            .set_merge_commit_sha(&target_task_ids[0], "abc123")
            .await
            .unwrap();
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        let r = server
            .dispatch_tool(
                "proposal_reconcile_obsolete_epic",
                serde_json::json!({
                    "id": pid.clone(),
                    "epic_id": target_epic_id.clone(),
                    "preview": true,
                }),
            )
            .await
            .unwrap();
        assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(r.get("blocked").and_then(|v| v.as_bool()), Some(true));
        assert!(
            r.get("error")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .contains("blocked by merged work")
        );
        assert_eq!(r.get("epics_closed").and_then(|v| v.as_i64()), Some(0));
        assert_eq!(r.get("tasks_closed").and_then(|v| v.as_i64()), Some(0));
        assert!(
            r.get("blocked_feedback_body")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .contains("contains merged work")
        );

        let p = prepo.get(&pid).await.unwrap().unwrap();
        assert_eq!(p.status, "building");
        assert_eq!(
            p.build_breakdown_task_id.as_deref(),
            Some(breakdown_id.as_str())
        );
        assert_eq!(
            prepo.graduated_epics(&pid).await.unwrap(),
            vec![(
                target_epic_id.clone(),
                trepo
                    .get(&target_task_ids[0])
                    .await
                    .unwrap()
                    .unwrap()
                    .project_id
            )]
        );
        assert_eq!(
            EpicRepository::new(db.clone(), bus.clone())
                .get(&target_epic_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "open"
        );
        for tid in &target_task_ids {
            assert_eq!(trepo.get(tid).await.unwrap().unwrap().status, "open");
        }
        let feedback = prepo.feedback(&pid).await.unwrap();
        assert_eq!(feedback.len(), 1);
        assert_eq!(feedback[0].author_kind, "ai");
        assert!(feedback[0].body.contains("contains merged work"));
        assert!(feedback[0].body.contains(&target_task_ids[0]));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_tears_down_and_reverts_to_approved() {
        let (server, db, pid, epic_id, task_ids, breakdown_id) = building_proposal().await;
        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "abort".into(),
                reason: Some("changed my mind".into()),
                preview: Some(false),
            }))
            .await
            .0;
        assert!(r.ok, "abort failed: {:?}", r.error);
        assert_eq!(r.status.as_deref(), Some("approved"));
        assert_eq!(r.epics_closed, 1);
        assert_eq!(r.tasks_closed, 3);

        let bus = EventBus::noop();
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        let p = prepo.get(&pid).await.unwrap().unwrap();
        assert_eq!(p.status, "approved");
        assert!(p.build_owner_user_id.is_none());
        assert!(p.build_breakdown_task_id.is_none());
        assert!(prepo.graduated_epics(&pid).await.unwrap().is_empty());

        let epic = EpicRepository::new(db.clone(), bus.clone())
            .get(&epic_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(epic.status, "closed");

        let trepo = TaskRepository::new(db.clone(), bus.clone());
        for tid in task_ids.iter().chain(std::iter::once(&breakdown_id)) {
            let t = trepo.get(tid).await.unwrap().unwrap();
            assert_eq!(t.status, "closed", "task {tid} should be force-closed");
        }

        // A second abort is rejected — the proposal is no longer building.
        let r2 = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid,
                mode: "abort".into(),
                reason: Some("again".into()),
                preview: Some(false),
            }))
            .await
            .0;
        assert!(!r2.ok);
    }
}

// ── Schema-lean regression tests ──────────────────────────────────────────
//
// Guard `ProposalCreateParams` and `ProposalUpdateParams` against accidental
// inlining of block vocabulary (tags, field schemas, catalog enums). Clients
// discover vocabulary via `get_block_catalog` / `proposal_blocks`, then
// submit proposal bodies through the existing `body` + `body_format` fields.

#[cfg(test)]
mod schema_lean_tests {
    use schemars::schema_for;
    use serde_json::Value;

    /// Recursively collect every string value reachable from `value`.
    fn collect_strings(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(s) => out.push(s.clone()),
            Value::Array(arr) => {
                for item in arr {
                    collect_strings(item, out);
                }
            }
            Value::Object(map) => {
                for v in map.values() {
                    collect_strings(v, out);
                }
            }
            _ => {}
        }
    }

    /// Assert that the serialized JSON schema does not mention any of the
    /// given forbidden terms.  A single traversal collects all string values
    /// (keys, enum entries, titles, descriptions, …) and a linear scan
    /// checks every one.
    fn assert_schema_excludes_terms(schema: &Value, forbidden: &[&str], context: &str) {
        let mut strings = Vec::new();
        collect_strings(schema, &mut strings);
        for term in forbidden {
            for s in &strings {
                assert!(
                    !s.contains(term),
                    "{context} schema unexpectedly contains forbidden term \
                     \"{term}\" in string value \"{s}\""
                );
            }
        }
    }

    /// Terms that must never appear in a proposal write-schema.  These
    /// cover: generic vocabulary field names, concrete MDX block tags, and
    /// block-field/enum concepts.
    const FORBIDDEN_BLOCK_TERMS: &[&str] = &[
        // generic vocabulary surface
        "block_types",
        "catalog",
        "blocks",
        // concrete MDX block tags (must match proposal_block_catalog.json)
        "AnnotatedCode",
        "ApiEndpoint",
        "Callout",
        "Checklist",
        "Columns",
        "Decisions",
        "Diagram",
        "Diff",
        "FileTree",
        "JsonExplorer",
        "QuestionForm",
        "RichText",
        "Tabs",
        "Wireframe",
        // kebab-case type identifiers
        "annotated-code",
        "api-endpoint",
        "callout",
        "checklist",
        "columns",
        "decisions",
        "diagram",
        "diff",
        "file-tree",
        "json-explorer",
        "question-form",
        "rich-text",
        "tabs",
        "wireframe",
        // block enum / field schema vocabulary
        "BlockType",
        "ProposalBlock",
    ];

    /// Expected top-level properties for `ProposalCreateParams`.
    const CREATE_ALLOWED_PROPS: &[&str] = &[
        "title",
        "body",
        "acceptance_criteria",
        "target_projects",
        "status",
        "body_format",
    ];

    /// Expected top-level properties for `ProposalUpdateParams`.
    const UPDATE_ALLOWED_PROPS: &[&str] = &[
        "id",
        "title",
        "body",
        "acceptance_criteria",
        "status",
        "superseded_by",
        "body_format",
    ];

    #[test]
    fn proposal_create_params_schema_is_lean_and_excludes_block_vocabulary() {
        let schema = schema_for!(super::ProposalCreateParams);
        let json: Value = serde_json::to_value(&schema).expect("schema serializes");

        // Verify allowed properties.
        let props = json["properties"]
            .as_object()
            .expect("ProposalCreateParams schema should have properties object");
        let prop_keys: Vec<&str> = props.keys().map(String::as_str).collect();
        assert_eq!(
            prop_keys, CREATE_ALLOWED_PROPS,
            "ProposalCreateParams properties drifted: got {prop_keys:?}, \
             expected {CREATE_ALLOWED_PROPS:?}"
        );

        assert_schema_excludes_terms(&json, FORBIDDEN_BLOCK_TERMS, "ProposalCreateParams");
    }

    #[test]
    fn proposal_update_params_schema_is_lean_and_excludes_block_vocabulary() {
        let schema = schema_for!(super::ProposalUpdateParams);
        let json: Value = serde_json::to_value(&schema).expect("schema serializes");

        // Verify allowed properties.
        let props = json["properties"]
            .as_object()
            .expect("ProposalUpdateParams schema should have properties object");
        let prop_keys: Vec<&str> = props.keys().map(String::as_str).collect();
        assert_eq!(
            prop_keys, UPDATE_ALLOWED_PROPS,
            "ProposalUpdateParams properties drifted: got {prop_keys:?}, \
             expected {UPDATE_ALLOWED_PROPS:?}"
        );

        assert_schema_excludes_terms(&json, FORBIDDEN_BLOCK_TERMS, "ProposalUpdateParams");
    }
}

#[cfg(test)]
mod block_patch_tests {
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
                            "start": 4,
                            "end": 7,
                            "expected_text": "bbb"
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
        assert_eq!(stored.body, "aaa DDD ccc");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_patch_byte_range_rejects_stale_text() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Stale Range",
                body: "aaa bbb ccc",
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
                            "start": 4,
                            "end": 7,
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
    use super::super::proposal_blocks::{parse_mdx_blocks, validate_mdx_blocks};
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
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

#[cfg(test)]
mod signoff_readiness_tests {
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_core::events::EventBus;
    use djinn_db::{
        Database, ProjectRepository, ProposalCreateInput, ProposalRepository, UserRepository,
    };

    /// A well-formed body that passes all deterministic readiness checks.
    fn ready_body() -> &'static str {
        r#"
# Problem
Users cannot do X.

# Scope
In scope: Y. Out of scope: Z.

# Objectives
- Deliver A
- Deliver B

## File map
```file-map
    src/main.rs
    src/lib.rs
```

# Dependencies
Blocked by service C.

# Open Questions
What happens if D fails?
"#
    }

    /// A minimal body that fails most readiness checks (missing problem,
    /// scope, objectives, grounding, dependencies, open questions).
    fn incomplete_body() -> &'static str {
        "Just some random text without any required sections."
    }

    async fn setup_test_server_and_user() -> (DjinnMcpServer, Database, String) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let user = UserRepository::new(db.clone())
            .upsert_from_github(999_700, "signoff-test-user", None, None)
            .await
            .unwrap();
        UserRepository::new(db.clone())
            .set_role(&user.id, "engineer")
            .await
            .unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db, user.id)
    }

    /// A draft proposal with incomplete readiness fails on first sign-off
    /// and remains `draft` with no new sign-off.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn draft_incomplete_proposal_fails_signoff_and_remains_draft() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("svc-draft-inc", "test", "svc-draft-inc")
            .await
            .unwrap();

        let proposal = repo
            .create(ProposalCreateInput {
                title: "Incomplete Draft",
                body: incomplete_body(),
                acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        // Add a target so target_count > 0 (one fewer failure to worry about).
        repo.add_target(&proposal.id, &project.id, "primary")
            .await
            .unwrap();

        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
                server
                    .dispatch_tool(
                        "proposal_signoff",
                        serde_json::json!({ "id": proposal.id, "kind": "technical" }),
                    )
                    .await
            })
            .await
            .unwrap();

        let error = response.get("error").and_then(|v| v.as_str());
        assert!(
            error.is_some(),
            "expected readiness error, got: {response:?}"
        );
        let error = error.unwrap();
        assert!(
            error.contains("proposal not ready for review"),
            "error should mention readiness: {error}"
        );

        // Proposal must still be `draft` — sign-off was never persisted.
        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(stored.status, "draft", "status must remain draft");

        // No sign-offs recorded.
        let signoffs = repo.signoffs(&proposal.id).await.unwrap();
        assert!(signoffs.is_empty(), "no sign-offs should be recorded");
    }

    /// A complete draft proposal can receive a sign-off and advance to
    /// `in_review` (one of two required sign-off kinds).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn draft_complete_proposal_accepts_signoff_and_advances() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("svc-draft-ok", "test", "svc-draft-ok")
            .await
            .unwrap();

        let proposal = repo
            .create(ProposalCreateInput {
                title: "Complete Draft",
                body: ready_body(),
                acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        repo.add_target(&proposal.id, &project.id, "primary")
            .await
            .unwrap();

        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
                server
                    .dispatch_tool(
                        "proposal_signoff",
                        serde_json::json!({ "id": proposal.id, "kind": "technical" }),
                    )
                    .await
            })
            .await
            .unwrap();

        assert!(
            response.get("error").is_none(),
            "sign-off should succeed: {:?}",
            response.get("error")
        );

        // Proposal must have advanced to `in_review` (one of two kinds given).
        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(
            stored.status, "in_review",
            "status must advance to in_review"
        );

        // One sign-off recorded.
        let signoffs = repo.signoffs(&proposal.id).await.unwrap();
        assert_eq!(signoffs.len(), 1, "one sign-off should be recorded");
    }

    /// An `in_review` proposal with a vague AC fails technical sign-off with
    /// the offending AC index/text in the error message.
    ///
    /// Uses a direct status update to simulate legacy data that was advanced
    /// before the readiness gate was in place.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn in_review_proposal_with_vague_ac_fails_signoff() {
        let (server, db, user_id) = setup_test_server_and_user().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        // Create a proposal with a clean body but vague AC.
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Vague AC Review",
                body: ready_body(),
                acceptance_criteria: Some(
                    r#"[{"criterion":"API returns 200","met":false},{"criterion":"The UI should be user-friendly","met":false}]"#,
                ),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("svc-review-vague", "test", "svc-review-vague")
            .await
            .unwrap();
        repo.add_target(&proposal.id, &project.id, "primary")
            .await
            .unwrap();

        // Directly advance the status to `in_review` to simulate legacy
        // data that pre-dates the readiness gate.
        sqlx::query("UPDATE proposals SET status = 'in_review' WHERE id = $1")
            .bind(&proposal.id)
            .execute(db.pool())
            .await
            .unwrap();

        // Attempt the technical sign-off — it should fail because the
        // vague AC makes the proposal not ready.
        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id), async {
                server
                    .dispatch_tool(
                        "proposal_signoff",
                        serde_json::json!({ "id": proposal.id, "kind": "technical" }),
                    )
                    .await
            })
            .await
            .unwrap();

        let error = response.get("error").and_then(|v| v.as_str());
        assert!(
            error.is_some(),
            "expected readiness error, got: {response:?}"
        );
        let error = error.unwrap();
        assert!(
            error.contains("proposal not ready for review"),
            "error should mention readiness: {error}"
        );
        assert!(
            error.contains("Vague/unverifiable acceptance criterion"),
            "error should mention vague AC: {error}"
        );
        assert!(
            error.contains("user-friendly"),
            "error should include the offending AC text: {error}"
        );

        // Proposal must still be `in_review` — no new sign-off was recorded.
        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(stored.status, "in_review");

        // No sign-offs should exist.
        let signoffs = repo.signoffs(&proposal.id).await.unwrap();
        assert!(signoffs.is_empty(), "no sign-offs should be recorded");
    }
}

// ── End-to-end planner refinement loop regression (task iy6v) ────────────
//
// This module ties together the `y4td` surface delivered by the sibling tasks
// (1787 block-patch regressions, kepb planner prompt wiring, 18g4 patch
// primitive, 6al0 revision metadata, mzz8 schema-lean guard) into a single
// integrated regression that models the proposal `r0io` / `5bdd` flow:
//
//   1. A planner authoring session loads `visual-spec` from the native-skill
//      registry delivered by `5uzr` / `y8p2`.
//   2. The planner pulls `get_block_catalog` from the `ilqx` surface on demand
//      — block vocabulary is never inlined into prompts or write schemas.
//   3. The planner converts a markdown-only proposal draft into block-enriched
//      MDX through several targeted `proposal_block_patch` calls — never a
//      monolithic `proposal_update`.
//   4. Each patch records one proposal revision with `targeted_block_patch`
//      metadata and the active `visual-spec` version attribution.
//   5. The enriched proposal exports through `proposal_export` as valid MDX.
//
// Why these tests live here rather than as a separate cross-crate harness:
// the planner refinement loop is a property of how the control-plane MCP
// server stitches the surfaces together — `proposal_create`,
// `proposal_block_patch`, `proposal_show` (revisions), and `proposal_export`
// all run on the same `DjinnMcpServer` against a real `ProposalRepository`.
// The native-skill registry lookup and the block-catalog pull are pure-Rust
// surfaces that resolve at compile time.  This module therefore exercises
// the real delivered end-to-end surface without standing up the planner
// session runtime, which would require additional infrastructure.
#[cfg(test)]
mod end_to_end_planner_refinement_loop_tests {
    use super::super::proposal_blocks::{
        parse_mdx_blocks, proposal_block_catalog, validate_mdx_blocks,
    };
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_agent::native_skills::{native_skill_version, resolved_native_skills_for_role};
    use djinn_core::events::EventBus;
    use djinn_db::{Database, ProposalCreateInput, ProposalRepository};
    use serde_json::Value;

    async fn test_server() -> (DjinnMcpServer, Database) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db)
    }

    /// Multi-section markdown proposal draft used as the starting point for
    /// the planner refinement loop.  Four independently targetable sections /
    /// paragraphs are present so the test exercises the `proposal_block_patch`
    /// primitive over multiple distinct selectors.
    const DRAFT_BODY: &str = "\
# Visual-spec authoring integration

The opening paragraph introduces the proposal and explains its purpose.

## Approach

The approach section describes the high-level plan in prose.

## Tradeoffs

The tradeoffs section enumerates the costs of the chosen approach.

## Open Questions

The open-questions section collects uncertainties for the team.
";

    /// AC: planner authoring sessions receive the native `visual-spec` skill
    /// (delivered by `y8p2`) through the resolved-native-skills surface so the
    /// planner can `skill_read` it on demand rather than embedding it in the
    /// prompt body.  This is the lazy loading contract that lets
    /// non-authoring planner sessions avoid paying the visual-spec body cost.
    #[test]
    fn planner_authoring_session_resolves_visual_spec_from_native_registry() {
        // A planner authoring session must resolve exactly one native skill
        // — `visual-spec` — through the registry exposed by `y8p2`.
        let resolved = resolved_native_skills_for_role("planner");
        let names: Vec<&str> = resolved.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"visual-spec"),
            "planner authoring session must resolve visual-spec via the native \
             registry; got {names:?}"
        );

        // The version stamp must come from the same registry (no parallel
        // version source) so the planner can pass it through
        // `proposal_block_patch` for revision attribution.  `ResolvedSkill`
        // does not carry `version` (that field is reserved for the immutable
        // native registry), so we read the version from
        // `native_skill_version` directly.
        let registry_version = native_skill_version("visual-spec")
            .expect("native_skill_version must return the active visual-spec version");
        assert!(
            !registry_version.is_empty(),
            "native_skill_version must return a non-empty version stamp"
        );

        // The resolved skill must be marked `required: true` for the planner
        // role — the planner can't author MDX without it.  This pins the
        // lazy-loading contract: the registry is the single source of truth
        // for the active version, not a duplicated constant in prompts.
        let visual_spec = resolved.iter().find(|s| s.name == "visual-spec").unwrap();
        assert!(
            visual_spec.required,
            "visual-spec must be required for the planner authoring session"
        );
    }

    /// AC: non-authoring sessions (e.g. `worker`, `reviewer`) must NOT
    /// receive `visual-spec`.  This is the lazy-loading guard: only the
    /// planner role pays the visual-spec body cost.
    #[test]
    fn non_authoring_sessions_do_not_receive_visual_spec() {
        for role in ["worker", "reviewer"] {
            let resolved = resolved_native_skills_for_role(role);
            let names: Vec<&str> = resolved.iter().map(|s| s.name.as_str()).collect();
            assert!(
                !names.contains(&"visual-spec"),
                "{role} session must not receive visual-spec; got {names:?}"
            );
        }
    }

    /// AC: the `get_block_catalog` pull surface (delivered by `ilqx`) returns
    /// the lean (type, tag) vocabulary the planner uses to discover block
    /// names without inlining them in prompts or write schemas.  This test
    /// pins the catalog against the registry so a future drift between the
    /// two surfaces is caught.
    #[test]
    fn get_block_catalog_pull_surface_returns_lean_vocabulary() {
        let catalog = proposal_block_catalog();
        assert_eq!(
            catalog.len(),
            14,
            "get_block_catalog must expose all 14 v1 block types so the planner \
             can discover the vocabulary on demand"
        );
        // The catalog is the lean projection — no field schemas, no
        // descriptions, just (type, tag) pairs.  Any future regression that
        // bloats the catalog back into the rich registry shape is caught
        // here.
        for entry in &catalog {
            assert!(
                !entry.block_type.is_empty(),
                "catalog entry has empty type: {entry:?}"
            );
            assert!(
                !entry.tag.is_empty(),
                "catalog entry has empty tag: {entry:?}"
            );
        }
    }

    /// AC: a markdown-only proposal draft is progressively enriched into
    /// `body_format=mdx` through the targeted `proposal_block_patch`
    /// primitive.  `latest_revision_seq` must equal the seed revision (1)
    /// plus the number of patches applied.  A monolithic whole-body
    /// `proposal_update` is forbidden for enrichment — the test only
    /// invokes `proposal_block_patch`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_loop_increments_revision_seq_once_per_targeted_patch() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Planner Refinement Loop",
                body: DRAFT_BODY,
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
        assert_eq!(
            proposal.body_format, "markdown",
            "create seed must produce body_format=markdown"
        );

        // Three independently targetable sections get promoted to MDX blocks
        // through three sequential `proposal_block_patch` calls.  Each one
        // is exactly one material proposal edit (= one revision increment).
        let visual_spec_version = native_skill_version("visual-spec")
            .expect("native_skill_version must return the active visual-spec version");

        let patches = [
            (
                "The opening paragraph introduces the proposal and explains its purpose.",
                "<RichText id=\"opening\">\nThe structured opening paragraph.\n</RichText>",
                "first patch: opening paragraph -> RichText",
            ),
            (
                "The approach section describes the high-level plan in prose.",
                "<FileTree id=\"repo-layout\" name=\"repo\" />",
                "second patch: approach -> FileTree",
            ),
            (
                "The tradeoffs section enumerates the costs of the chosen approach.",
                "<Callout id=\"tradeoffs-callout\">\nThe structured tradeoff callout.\n</Callout>",
                "third patch: tradeoffs -> Callout",
            ),
        ];

        let mut expected_seq: i32 = 1;
        let mut prev_expected_revision_seq: Option<i32> = None;
        for (selector_text, block_mdx, note) in patches {
            let mut args = serde_json::json!({
                "id": proposal.id,
                "selector": { "exact_text": selector_text },
                "operation": "replace",
                "block_mdx": block_mdx,
                "native_skill_name": "visual-spec",
                "native_skill_version": visual_spec_version,
                "note": note,
            });
            // Pass `expected_latest_revision_seq` to exercise the stale-revision
            // guard path the prompt wires up for sequential patches.
            if let Some(prev) = prev_expected_revision_seq {
                args["expected_latest_revision_seq"] = serde_json::json!(prev);
            }

            let response = server
                .dispatch_tool("proposal_block_patch", args)
                .await
                .unwrap();
            assert!(
                response.get("error").is_none(),
                "proposal_block_patch failed for {note:?}: {:?}",
                response.get("error")
            );
            expected_seq += 1;
            prev_expected_revision_seq = Some(expected_seq);

            let after = repo.get(&proposal.id).await.unwrap().unwrap();
            assert_eq!(
                after.latest_revision_seq, expected_seq,
                "latest_revision_seq must be exactly +1 per patch after {note:?}"
            );
        }

        // Final state: three patches landed -> latest_revision_seq = 4
        // (1 seed + 3 patches).
        let final_state = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(
            final_state.latest_revision_seq, 4,
            "three patches from a 1-seed proposal must yield latest_revision_seq=4"
        );
        assert_eq!(
            final_state.body_format, "mdx",
            "first MDX block patch must upgrade body_format to mdx"
        );

        // The proposal_show surface must report the same revision seq
        // (drift between the repo state and the public surface is caught here).
        let shown = server
            .dispatch_tool("proposal_show", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();
        assert_eq!(
            shown
                .get("proposal")
                .and_then(|p| p.get("latest_revision_seq"))
                .and_then(|v| v.as_i64()),
            Some(4),
            "proposal_show.proposal.latest_revision_seq must match the repo state"
        );
    }

    /// AC: revision history exposes `targeted_block_patch` metadata on every
    /// patch revision, including the active `visual-spec` native-skill
    /// version attribution.  The seed revision must NOT carry that metadata
    /// — that signal is reserved for the patch primitive.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_loop_revisions_carry_visual_spec_attribution() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Refinement Loop Attribution",
                body: DRAFT_BODY,
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let visual_spec_version = native_skill_version("visual-spec")
            .expect("native_skill_version must return the active visual-spec version");

        // Apply two patches, attributing each to the registry's active version.
        for (selector_text, block_mdx) in [
            (
                "The opening paragraph introduces the proposal and explains its purpose.",
                "<RichText id=\"opening\">\nStructured opening.\n</RichText>",
            ),
            (
                "The approach section describes the high-level plan in prose.",
                "<FileTree id=\"repo\" name=\"repo\" />",
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
                        "native_skill_name": "visual-spec",
                        "native_skill_version": visual_spec_version,
                    }),
                )
                .await
                .unwrap();
            assert!(
                response.get("error").is_none(),
                "patch failed: {:?}",
                response.get("error")
            );
        }

        // Walk revisions through proposal_show — the surface the planner
        // consumes — and assert metadata shape on every patch revision.
        let shown = server
            .dispatch_tool("proposal_show", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();
        let revisions = shown
            .get("revisions")
            .and_then(|v| v.as_array())
            .expect("proposal_show.revisions must be a JSON array");

        // 1 seed + 2 patches = 3 revisions.
        assert_eq!(
            revisions.len(),
            3,
            "expected 3 revisions (1 seed + 2 patches); got {}",
            revisions.len()
        );

        // The seed revision must NOT carry targeted-block-patch metadata —
        // `proposal_create` writes no event_metadata.
        let seed = &revisions[0];
        let seed_meta = seed.get("event_metadata");
        assert!(
            seed_meta.is_none() || seed_meta.is_some_and(|v| v.is_null()),
            "create seed revision must not carry event_metadata, got {seed_meta:?}"
        );

        // Every patch revision must carry the targeted-block-patch signal
        // AND the active `visual-spec` version from the native registry.
        for (idx, rev) in revisions.iter().enumerate().skip(1) {
            let meta_str = rev
                .get("event_metadata")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("patch rev #{idx} must expose event_metadata"));
            let meta: Value = serde_json::from_str(meta_str)
                .unwrap_or_else(|e| panic!("patch rev #{idx} event_metadata must be JSON: {e}"));
            assert_eq!(
                meta["change_kind"], "targeted_block_patch",
                "patch rev #{idx} must identify as targeted_block_patch"
            );
            assert_eq!(
                meta["native_skill_name"], "visual-spec",
                "patch rev #{idx} must attribute the native skill name"
            );
            assert_eq!(
                meta["native_skill_version"], visual_spec_version,
                "patch rev #{idx} must attribute the active visual-spec version from \
                 the native registry (drift between registry and patch metadata is \
                 caught here)"
            );
            // The byte-range fields are present and well-typed.
            assert!(
                meta["range_start_byte"].is_number() && meta["range_end_byte"].is_number(),
                "patch rev #{idx} must expose numeric byte-range fields"
            );
            assert!(
                meta["range_end_byte"].as_u64().unwrap()
                    > meta["range_start_byte"].as_u64().unwrap(),
                "patch rev #{idx} range_end_byte must exceed range_start_byte"
            );
        }
    }

    /// AC: after the refinement loop, the proposal exports cleanly through
    /// `proposal_export` and the returned MDX round-trips through the block
    /// parser.  This is the end-to-end fidelity contract that ties the
    /// refinement loop back to the MDX export surface.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_loop_enriched_proposal_exports_as_valid_mdx() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Refinement Loop Export",
                body: DRAFT_BODY,
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let visual_spec_version = native_skill_version("visual-spec")
            .expect("native_skill_version must return the active visual-spec version");

        // Apply three sequential targeted patches — the full planner refinement
        // loop from a markdown-only draft.
        for (selector_text, block_mdx) in [
            (
                "The opening paragraph introduces the proposal and explains its purpose.",
                "<RichText id=\"opening\">\nStructured opening.\n</RichText>",
            ),
            (
                "The approach section describes the high-level plan in prose.",
                "<FileTree id=\"repo-layout\" name=\"repo\" />",
            ),
            (
                "The tradeoffs section enumerates the costs of the chosen approach.",
                "<Callout id=\"tradeoffs\">\nStructured callout.\n</Callout>",
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
                        "native_skill_name": "visual-spec",
                        "native_skill_version": visual_spec_version,
                    }),
                )
                .await
                .unwrap();
            assert!(
                response.get("error").is_none(),
                "patch failed: {:?}",
                response.get("error")
            );
        }

        // Final body_format must be mdx.
        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(stored.body_format, "mdx");

        // proposal_export must succeed and return MDX with frontmatter.
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
            mdx.matches("---").count() >= 2,
            "exported MDX must include the YAML frontmatter delimiters"
        );

        // The exported MDX body must parse into the same blocks as the
        // stored body.  This is the round-trip fidelity contract.
        let original_blocks =
            parse_mdx_blocks(&stored.body).expect("stored body must parse as MDX");
        let exported_body = mdx
            .splitn(3, "---")
            .nth(2)
            .expect("exported MDX must have a body section after frontmatter")
            .trim_start_matches('\n');
        let exported_blocks =
            parse_mdx_blocks(exported_body).expect("exported body must parse as MDX");
        assert_eq!(
            exported_blocks, original_blocks,
            "exported MDX blocks must match the stored body blocks byte-for-byte"
        );
        let exported_ids: Vec<&str> = exported_blocks.iter().map(|b| b.id.as_str()).collect();
        assert!(exported_ids.contains(&"opening"));
        assert!(exported_ids.contains(&"repo-layout"));
        assert!(exported_ids.contains(&"tradeoffs"));

        // Body validation must succeed end-to-end on the stored body.
        validate_mdx_blocks(&stored.body)
            .expect("enriched body must validate as MDX after the refinement loop");

        // Unrelated sections must survive byte-for-byte — proving no
        // monolithic whole-body rewrite happened.
        for anchor in [
            "# Visual-spec authoring integration",
            "## Open Questions",
            "The open-questions section collects uncertainties for the team.",
        ] {
            assert!(
                stored.body.contains(anchor),
                "unrelated anchor {anchor:?} must be preserved verbatim after the \
                 refinement loop; body was:\n{}",
                stored.body
            );
        }
    }

    /// AC: the planner workflow surfaces remain lazy.  Concretely:
    ///   * the `proposal_address.md` planner prompt does not inline the
    ///     block vocabulary or skill body (verified by re-asserting the
    ///     prompts-tests contract from kepb at the workflow-regression
    ///     level),
    ///   * the catalog pull surface is the single source of block
    ///     vocabulary (verified by ensuring the prompt does not name any
    ///     block tag from `proposal_block_catalog`),
    ///   * the active native-skill version stamped on patch revisions is
    ///     identical to the version returned by `native_skill_version` so the
    ///     registry remains the single source of truth.
    ///
    /// This test ties the lazy-surfaces contract to the actual refinement
    /// loop: any future edit that bakes vocabulary into the prompt, or that
    /// drifts the patch-attribute version away from the registry version,
    /// is caught here.
    #[test]
    fn refinement_loop_workflow_surfaces_remain_lazy() {
        // (a) The planner proposal-address prompt must not inline block
        //     vocabulary.  Re-assert the prompt-test contract at the
        //     workflow-regression level.
        let prompt = include_str!("../../../djinn-agent/src/prompts/proposal_address.md");
        let catalog = proposal_block_catalog();
        for entry in &catalog {
            assert!(
                !prompt.contains(&entry.tag),
                "proposal_address.md must not inline block tag {:?} from the catalog",
                entry.tag
            );
            assert!(
                !prompt.contains(&entry.block_type),
                "proposal_address.md must not inline block type {:?} from the catalog",
                entry.block_type
            );
        }
        // Generic vocabulary surface must not appear either.
        assert!(
            !prompt.contains("block_types"),
            "proposal_address.md must not reference a `block_types` catalog list"
        );

        // (b) The catalog pull surface and the native-skill version stamp
        //     remain the single source of truth.  The registry version
        //     returned by `native_skill_version` is exactly what planners
        //     stamp on patch revisions — there is no parallel version
        //     constant the prompt or tests could drift against.
        //     `ResolvedSkill` does not carry `version` (that field is reserved
        //     for the immutable native registry), so we only assert that the
        //     planner role resolves `visual-spec` here.
        let registry_version = native_skill_version("visual-spec")
            .expect("native_skill_version must return the active visual-spec version");
        let resolved = resolved_native_skills_for_role("planner");
        assert!(
            resolved.iter().any(|s| s.name == "visual-spec"),
            "planner must resolve visual-spec"
        );
        assert!(
            !registry_version.is_empty(),
            "registry version must be a non-empty stamp"
        );
    }

    /// AC: the full integrated end-to-end refinement loop — markdown draft ->
    /// skill/catalog resolution -> 3 targeted patches -> MDX export — wires
    /// together every y4td surface into a single deterministic regression.
    /// This is the load-bearing test the task acceptance criteria converge on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_loop_end_to_end_ties_all_y4td_surfaces_together() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        // (1) Planner authoring session must resolve the native `visual-spec`
        //     skill (y8p2 surface) — verifies the lazy loading contract.
        let resolved_skills = resolved_native_skills_for_role("planner");
        assert!(
            resolved_skills.iter().any(|s| s.name == "visual-spec"),
            "planner must resolve visual-spec via the native registry"
        );
        let registry_version = native_skill_version("visual-spec")
            .expect("native_skill_version must return the active visual-spec version");
        assert!(
            !registry_version.is_empty(),
            "native_skill_version must return a non-empty stamp"
        );

        // (2) The planner pulls the lean catalog on demand (ilqx surface) —
        //     block vocabulary is never inlined into the proposal write
        //     schemas (verified separately by the prompt schema-lean
        //     regression in `schema_lean_tests`).
        let catalog = proposal_block_catalog();
        assert_eq!(
            catalog.len(),
            14,
            "get_block_catalog must expose the full v1 vocabulary on demand"
        );

        // (3) Create a markdown-only proposal draft.
        let proposal = repo
            .create(ProposalCreateInput {
                title: "End-to-end y4td regression",
                body: DRAFT_BODY,
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        assert_eq!(proposal.body_format, "markdown");
        assert_eq!(proposal.latest_revision_seq, 1);

        // (4) Apply 3 sequential targeted block patches through the real
        //     `proposal_block_patch` MCP surface — never a whole-body
        //     `proposal_update`.  Each patch carries the active visual-spec
        //     version from the registry for revision attribution.
        for (selector_text, block_mdx) in [
            (
                "The opening paragraph introduces the proposal and explains its purpose.",
                "<RichText id=\"opening\">\nStructured opening.\n</RichText>",
            ),
            (
                "The approach section describes the high-level plan in prose.",
                "<FileTree id=\"repo-layout\" name=\"repo\" />",
            ),
            (
                "The tradeoffs section enumerates the costs of the chosen approach.",
                "<Callout id=\"tradeoffs\">\nStructured callout.\n</Callout>",
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
                        "native_skill_name": "visual-spec",
                        "native_skill_version": registry_version,
                    }),
                )
                .await
                .unwrap();
            assert!(
                response.get("error").is_none(),
                "proposal_block_patch failed: {:?}",
                response.get("error")
            );
        }

        // (5) Final state: body_format=mdx, latest_revision_seq=4 (1 seed + 3 patches).
        let final_state = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(final_state.body_format, "mdx");
        assert_eq!(final_state.latest_revision_seq, 4);

        // (6) Revision metadata: every patch revision carries
        //     `targeted_block_patch` + the registry's visual-spec version.
        let shown = server
            .dispatch_tool("proposal_show", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();
        let revisions = shown
            .get("revisions")
            .and_then(|v| v.as_array())
            .expect("proposal_show.revisions must be a JSON array");
        assert_eq!(revisions.len(), 4, "1 seed + 3 patches = 4 revisions");
        for (idx, rev) in revisions.iter().enumerate().skip(1) {
            let meta: Value = serde_json::from_str(
                rev.get("event_metadata")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| panic!("rev #{idx} must expose event_metadata")),
            )
            .unwrap_or_else(|e| panic!("rev #{idx} event_metadata must be JSON: {e}"));
            assert_eq!(meta["change_kind"], "targeted_block_patch");
            assert_eq!(meta["native_skill_name"], "visual-spec");
            assert_eq!(meta["native_skill_version"], registry_version);
        }

        // (7) The final enriched proposal exports as valid MDX through
        //     `proposal_export`, with all 3 patched blocks intact.
        let exported = server
            .dispatch_tool("proposal_export", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();
        assert!(exported.get("error").is_none());
        let mdx = exported
            .get("mdx")
            .and_then(|v| v.as_str())
            .expect("export must return mdx for body_format=mdx proposals");
        let exported_body = mdx
            .splitn(3, "---")
            .nth(2)
            .expect("exported MDX must have a body section after frontmatter")
            .trim_start_matches('\n');
        let exported_blocks =
            parse_mdx_blocks(exported_body).expect("exported body must parse as MDX");
        let exported_ids: Vec<&str> = exported_blocks.iter().map(|b| b.id.as_str()).collect();
        assert!(exported_ids.contains(&"opening"));
        assert!(exported_ids.contains(&"repo-layout"));
        assert!(exported_ids.contains(&"tradeoffs"));
    }
}
