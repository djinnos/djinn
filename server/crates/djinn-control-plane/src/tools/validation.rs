// Shared input validation helpers for MCP tool parameters.
//
// Each function returns `Result<T, String>` where `Err` is a human-readable
// message suitable for returning as a JSON `{ "error": ... }` response.

use djinn_spec_lint::{
    BlockError as LeafBlockError, ParsedProposalBlock, extract_custom_block_tags, parse_mdx_blocks,
    proposal_block_tags, validate_block_content, validate_mdx_blocks,
};

use crate::tools::proposal_blocks::proposal_block_definition_for_tag;

/// Decode the handful of HTML entities that LLM-authored plain text routinely
/// over-escapes (e.g. a title arriving as `A &amp; B`). `&amp;` is decoded last
/// so `&amp;lt;` round-trips to `&lt;` rather than `<`.
fn decode_html_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Trim and validate a title: 1–200 chars. Titles are plain text, so HTML
/// entities (an LLM over-escaping artifact) are decoded to their literal form.
pub fn validate_title(s: &str) -> Result<String, String> {
    let trimmed = decode_html_entities(s.trim()).trim().to_owned();
    if trimmed.is_empty() {
        return Err("title must not be empty".into());
    }
    if trimmed.len() > 200 {
        return Err(format!("title exceeds 200 chars (got {})", trimmed.len()));
    }
    Ok(trimmed)
}

/// Validate description: max 10,000 chars.
pub fn validate_description(s: &str) -> Result<(), String> {
    if s.len() > 10_000 {
        return Err(format!(
            "description exceeds 10,000 chars (got {})",
            s.len()
        ));
    }
    Ok(())
}

/// Validate design field: max 50,000 chars.
pub fn validate_design(s: &str) -> Result<(), String> {
    if s.len() > 50_000 {
        return Err(format!("design exceeds 50,000 chars (got {})", s.len()));
    }
    Ok(())
}

/// Validate emoji: empty or a single emoji grapheme.
///
/// Uses char-range heuristics — no new crate dependency.
pub fn validate_emoji(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Ok(());
    }
    if s.len() > 32 {
        return Err("emoji exceeds 32 bytes".into());
    }
    // Must contain at least one emoji-range codepoint.
    let has_emoji = s.chars().any(is_emoji_char);
    if !has_emoji {
        return Err(format!("invalid emoji: {s:?}"));
    }
    Ok(())
}

/// Validate color: empty or `#` followed by 3, 4, 6, or 8 hex digits.
pub fn validate_color(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Ok(());
    }
    let Some(hex) = s.strip_prefix('#') else {
        return Err(format!("color must start with '#': {s:?}"));
    };
    let valid_len = matches!(hex.len(), 3 | 4 | 6 | 8);
    if !valid_len || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("invalid color: {s:?}"));
    }
    Ok(())
}

/// Validate priority: 0–99.
pub fn validate_priority(p: i64) -> Result<(), String> {
    if !(0..=99).contains(&p) {
        return Err(format!("priority must be 0–99 (got {p})"));
    }
    Ok(())
}

/// Validate issue_type: "task", "feature", "bug", "spike", "research", "planning", or "review".
/// Also accepts legacy "decomposition" (mapped to "planning" at the model layer).
pub fn validate_issue_type(s: &str) -> Result<(), String> {
    match s {
        "task" | "feature" | "bug" | "spike" | "research" | "planning" | "decomposition"
        | "review" => Ok(()),
        other => Err(format!(
            "invalid issue_type: {other:?} (expected task, feature, bug, spike, research, planning, or review)"
        )),
    }
}

/// Trim and validate a label: 1–50 chars.
pub fn validate_label(s: &str) -> Result<String, String> {
    let trimmed = s.trim().to_owned();
    if trimmed.is_empty() {
        return Err("label must not be empty".into());
    }
    if trimmed.len() > 50 {
        return Err(format!("label exceeds 50 chars (got {})", trimmed.len()));
    }
    Ok(trimmed)
}

/// Validate total label count: max 20.
pub fn validate_labels_count(n: usize) -> Result<(), String> {
    if n > 20 {
        return Err(format!("too many labels (max 20, got {n})"));
    }
    Ok(())
}

/// Trim and validate an owner: max 100 chars.
pub fn validate_owner(s: &str) -> Result<String, String> {
    let trimmed = s.trim().to_owned();
    if trimmed.len() > 100 {
        return Err(format!("owner exceeds 100 chars (got {})", trimmed.len()));
    }
    Ok(trimmed)
}

/// Clamp limit to 1–200.
pub fn validate_limit(l: i64) -> i64 {
    l.clamp(1, 200)
}

/// Clamp offset to >= 0.
pub fn validate_offset(o: i64) -> i64 {
    o.max(0)
}

/// Validate sort key is in the allowed set.
pub fn validate_sort(s: &str, allowed: &[&str]) -> Result<(), String> {
    if allowed.contains(&s) {
        Ok(())
    } else {
        Err(format!(
            "invalid sort: {s:?} (allowed: {})",
            allowed.join(", ")
        ))
    }
}

/// Validate comment body: 1–10,000 chars.
pub fn validate_body(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("body must not be empty".into());
    }
    if s.len() > 10_000 {
        return Err(format!("body exceeds 10,000 chars (got {})", s.len()));
    }
    Ok(())
}

/// Validate a proposal body for MDX block tags.
///
/// When `body_format` is `"mdx"` and `body` is non-empty, extract every
/// PascalCase component (JSX-like) tag and check each against the shared block
/// registry. The first unknown tag produces an error naming the offending tag,
/// e.g. `Unknown MDX block tag: 'FooBar'`. For `"markdown"` bodies or empty
/// bodies validation is skipped (returns `Ok(())`).
pub fn validate_mdx_body(body: &str, body_format: Option<&str>) -> Result<(), String> {
    if body_format != Some("mdx") {
        return Ok(());
    }
    if body.trim().is_empty() {
        return Ok(());
    }
    let allowed = proposal_block_tags();
    for tag in extract_custom_block_tags(body) {
        if !allowed.contains(tag.as_str()) {
            return Err(format!("Unknown MDX block tag: '{tag}'"));
        }
    }
    // Parse through the canonical leaf implementation before checking the local
    // wireframe-only HTML-safety backstop. Structural parse failures are
    // intentionally left for the full validation stack below, preserving the
    // established error ordering and human-readable parse errors.
    if let Ok(blocks) = parse_mdx_blocks(body) {
        validate_html_block_safety(&blocks)?;
    }
    Ok(())
}

/// Resolve the effective `body_format` for a full-body proposal write and run
/// the full MDX block-validation stack on it.
///
/// This is the shared cutover that closes the "markdown body full of block tags"
/// hole: a `markdown` body that carries any custom (PascalCase) block tag is
/// upgraded to `mdx` and validated, exactly like [`apply_block_patch`] already
/// does for targeted patches. A body that is already `mdx`, or has no block
/// tags, keeps its declared format. When the effective format is `mdx` the body
/// must pass the whole stack: unknown-tag rejection + wireframe HTML-safety
/// ([`validate_mdx_body`]), the empty-diagram guard and unknown-tag walk
/// ([`validate_mdx_blocks`]), a structural parse ([`parse_mdx_blocks`]), and the
/// empty-children guard ([`validate_block_content`]).
///
/// All four full-body write paths — `proposal_create`, `proposal_update`,
/// `proposal_import`, and `apply_block_patch` (block-patch) — call this so they
/// resolve the persisted format and validate identically. It deliberately does
/// NOT enforce question-form placement: that is an authoring-gate concern layered
/// on top by `proposal_create` / `proposal_update` (and intentionally skipped by
/// the block-patch and import restore paths).
///
/// Returns the `&'static str` format to persist (`"markdown"` or `"mdx"`).
pub fn resolve_body_format_and_validate(
    body: &str,
    declared_format: Option<&str>,
) -> Result<&'static str, String> {
    let has_block_tags = !extract_custom_block_tags(body).is_empty();
    let effective = if declared_format == Some("mdx") || has_block_tags {
        "mdx"
    } else {
        "markdown"
    };

    if effective == "mdx" {
        // Unknown-tag rejection + wireframe active-markup backstop.
        validate_mdx_body(body, Some("mdx"))?;
        // Unknown-tag walk (descends nested blocks) + empty-diagram guard.
        validate_mdx_blocks(body).map_err(adapt_leaf_block_error)?;
        // Structural parse, then reject empty children-based blocks.
        let blocks = parse_mdx_blocks(body).map_err(adapt_leaf_block_error)?;
        validate_block_content(&blocks)
            .map_err(|error| adapt_leaf_content_error(error, &blocks))?;
    }

    Ok(effective)
}

/// Preserve the established control-plane wording where the leaf contract has
/// deliberately kept only the portable validation detail.
fn adapt_leaf_block_error(error: LeafBlockError) -> String {
    match error {
        LeafBlockError::EmptyDiagram(id) => format!(
            "Diagram block `{id}` has no source — provide a non-empty `source` \
             (e.g. `source={{`flowchart LR; A-->B`}}`) or block content"
        ),
        other => other.to_string(),
    }
}

/// The compatibility catalog owns verbose authoring grammar. The leaf has
/// already identified the invalid block; this only restores that established
/// control-plane context without re-running its duplicate validator.
fn adapt_leaf_content_error(error: String, blocks: &[ParsedProposalBlock]) -> String {
    let Some(block) = blocks
        .iter()
        .find(|block| error.starts_with(&format!("{} block ", block.tag)))
    else {
        return error;
    };
    let Some(description) =
        proposal_block_definition_for_tag(&block.tag).and_then(|definition| definition.description)
    else {
        return error;
    };
    format!("{error}Expected grammar — {description}")
}

/// Server-side backstop for the sandboxed `html` and `wireframe` blocks (defense
/// in depth).
///
/// The primary defenses are the client-side iframe `sandbox=""` + restrictive
/// CSP and the DOMPurify pass before render. This is a third layer that rejects
/// the most dangerous *active* markup in a sandboxed-HTML block's children
/// **before it is ever stored**, mirroring agent-native's server regex. It is
/// deliberately conservative — only `<script>` tags, `on*=` event-handler
/// attributes, and `javascript:` / `data:text/html` URIs are rejected, so benign
/// formatted HTML (and inline SVG) still passes. `wireframe` is the remaining
/// sandboxed-HTML surface, so its raw markup is gated here.
fn validate_html_block_safety(blocks: &[ParsedProposalBlock]) -> Result<(), String> {
    for block in blocks {
        if block.block_type != "wireframe" {
            continue;
        }
        if let Some(reason) = first_unsafe_html_reason(&block.raw_content) {
            return Err(format!(
                "{} block '{}' contains disallowed active markup ({reason}); \
                 the {} block renders sandboxed/sanitized static markup and \
                 is not a scripting surface",
                block.tag, block.id, block.block_type
            ));
        }
    }
    Ok(())
}

/// Return a short reason string if `content` carries dangerous active markup,
/// or `None` if it is safe. Case-insensitive; whitespace inside the
/// `javascript:` scheme is collapsed so obfuscated `java script:` is caught.
fn first_unsafe_html_reason(content: &str) -> Option<&'static str> {
    let lower = content.to_ascii_lowercase();
    if lower.contains("<script") {
        return Some("a <script> tag");
    }
    // `on…=` event handler attributes, e.g. `onerror=`, `onload =`.
    if has_event_handler_attribute(&lower) {
        return Some("an on*= event handler");
    }
    // `javascript:` / `vbscript:` / `data:text/html` URIs, tolerating
    // intra-scheme whitespace used to dodge naive string matches.
    let collapsed: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    if collapsed.contains("javascript:")
        || collapsed.contains("vbscript:")
        || collapsed.contains("data:text/html")
    {
        return Some("a javascript:/data:text/html URI");
    }
    None
}

/// Detect an HTML `on<event>=` attribute (e.g. `onclick=`, `onerror =`).
/// Looks for a word boundary, `on`, one or more ASCII-alpha event-name chars,
/// optional whitespace, then `=`. Conservative: an `on`-prefixed plain word
/// followed by `=` is rare in benign block markup.
fn has_event_handler_attribute(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut i = 0;
    while let Some(pos) = lower[i..].find("on") {
        let start = i + pos;
        i = start + 2;
        // Require a boundary before `on` (start of string or non-alnum), so
        // words like `button` / `iron` don't match.
        let boundary =
            start == 0 || !bytes[start - 1].is_ascii_alphanumeric() && bytes[start - 1] != b'_';
        if !boundary {
            continue;
        }
        let mut j = i;
        // event name: at least one alpha char.
        while j < bytes.len() && bytes[j].is_ascii_alphabetic() {
            j += 1;
        }
        if j == i {
            continue;
        }
        // skip optional whitespace before `=`.
        let mut k = j;
        while k < bytes.len() && bytes[k].is_ascii_whitespace() {
            k += 1;
        }
        if k < bytes.len() && bytes[k] == b'=' {
            return true;
        }
    }
    false
}

/// Validate reason: max 2,000 chars.
pub fn validate_reason(s: &str) -> Result<(), String> {
    if s.len() > 2_000 {
        return Err(format!("reason exceeds 2,000 chars (got {})", s.len()));
    }
    Ok(())
}

/// Validate actor_id: max 100 chars.
pub fn validate_actor_id(s: &str) -> Result<(), String> {
    if s.len() > 100 {
        return Err(format!("actor_id exceeds 100 chars (got {})", s.len()));
    }
    Ok(())
}

/// Validate actor_role: max 50 chars.
pub fn validate_actor_role(s: &str) -> Result<(), String> {
    if s.len() > 50 {
        return Err(format!("actor_role exceeds 50 chars (got {})", s.len()));
    }
    Ok(())
}

/// Validate acceptance_criteria count: max 50.
pub fn validate_ac_count(n: usize) -> Result<(), String> {
    if n > 50 {
        return Err(format!("too many acceptance_criteria (max 50, got {n})"));
    }
    Ok(())
}

/// Validate initial task status for task_create: only "open".
pub fn validate_task_create_status(status: Option<&str>) -> Result<Option<&str>, String> {
    match status {
        None => Ok(None),
        Some("open") => Ok(Some("open")),
        Some(other) => Err(format!("invalid status: {other:?} (expected open)")),
    }
}

/// Validate initial epic status for epic_create. Epics are `open` (default) →
/// `closed`; the old `drafting`/`proposed` staging states are gone (that
/// pre-execution flow lives in proposals now). Pass `auto_breakdown=false` to
/// create an epic without auto-dispatching the planner.
pub fn validate_epic_create_status(status: Option<&str>) -> Result<Option<&str>, String> {
    match status {
        None => Ok(None),
        Some("open") => Ok(Some("open")),
        Some(other) => Err(format!("invalid epic status: {other:?} (expected open)")),
    }
}

/// Valid proposal lifecycle statuses:
/// `draft` → `in_review` → `approved` → `building` → `done`, plus the
/// off-ramps `rejected` / `archived` / `superseded`.
pub const PROPOSAL_STATUSES: &[&str] = &[
    "triage",
    "draft",
    "in_review",
    "approved",
    "building",
    "done",
    "rejected",
    "archived",
    "superseded",
];

/// Validate a proposal lifecycle status.
pub fn validate_proposal_status(status: &str) -> Result<(), String> {
    if PROPOSAL_STATUSES.contains(&status) {
        Ok(())
    } else {
        Err(format!(
            "invalid proposal status: {status:?} (expected one of {})",
            PROPOSAL_STATUSES.join(", ")
        ))
    }
}

/// Validate an optional initial proposal status for `proposal_create`
/// (`None` defaults to `draft`). Only the early hand-authored states are
/// allowed at creation; `building`/`done` are reached via the lifecycle.
pub fn validate_proposal_create_status(status: Option<&str>) -> Result<Option<&str>, String> {
    match status {
        None => Ok(None),
        Some(s @ ("triage" | "draft" | "in_review")) => Ok(Some(s)),
        Some(other) => Err(format!(
            "invalid initial proposal status: {other:?} (expected triage, draft, or in_review)"
        )),
    }
}

// ── Emoji helpers ────────────────────────────────────────────────────────────

/// Heuristic: is this char in a common emoji range?
fn is_emoji_char(c: char) -> bool {
    let cp = c as u32;
    matches!(
        cp,
        0x2600..=0x27BF        // Misc Symbols, Dingbats
        | 0x2300..=0x23FF      // Misc Technical
        | 0x2B50..=0x2B55      // Stars, circles
        | 0xFE00..=0xFE0F      // Variation selectors
        | 0x1F000..=0x1FAFF    // Extended emoji blocks
        | 0x200D               // ZWJ
        | 0xE0020..=0xE007F    // Tags
    )
}

// ── Tests ────────────────────────────────────────────────────────────────────

mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn title_validation() {
        assert!(validate_title("").is_err());
        assert!(validate_title("   ").is_err());
        assert_eq!(validate_title("  hello  ").unwrap(), "hello");
        assert!(validate_title("x").is_ok());
        assert!(validate_title(&"x".repeat(200)).is_ok());
        assert!(validate_title(&"x".repeat(201)).is_err());
        // HTML entities from over-escaped LLM output are decoded.
        assert_eq!(
            validate_title("Read-consistency &amp; DB pool routing").unwrap(),
            "Read-consistency & DB pool routing"
        );
        assert_eq!(
            validate_title("a &lt;b&gt; &quot;c&quot;").unwrap(),
            "a <b> \"c\""
        );
        assert_eq!(validate_title("&amp;lt;").unwrap(), "&lt;");
    }

    #[test]
    fn description_validation() {
        assert!(validate_description("").is_ok());
        assert!(validate_description(&"x".repeat(10_000)).is_ok());
        assert!(validate_description(&"x".repeat(10_001)).is_err());
    }

    #[test]
    fn design_validation() {
        assert!(validate_design("").is_ok());
        assert!(validate_design(&"x".repeat(50_000)).is_ok());
        assert!(validate_design(&"x".repeat(50_001)).is_err());
    }

    #[test]
    fn emoji_validation() {
        assert!(validate_emoji("").is_ok());
        assert!(validate_emoji("🚀").is_ok());
        assert!(validate_emoji("🎯").is_ok());
        assert!(validate_emoji("abc").is_err());
        assert!(validate_emoji(&"🚀".repeat(10)).is_err()); // > 32 bytes
    }

    #[test]
    fn color_validation() {
        assert!(validate_color("").is_ok());
        assert!(validate_color("#fff").is_ok());
        assert!(validate_color("#FFAA00").is_ok());
        assert!(validate_color("#8b5cf6").is_ok());
        assert!(validate_color("#ffff").is_ok()); // 4-digit
        assert!(validate_color("#ff00ff00").is_ok()); // 8-digit
        assert!(validate_color("fff").is_err()); // no #
        assert!(validate_color("#gg").is_err()); // bad hex
        assert!(validate_color("#12345").is_err()); // 5 digits
    }

    #[test]
    fn priority_validation() {
        assert!(validate_priority(0).is_ok());
        assert!(validate_priority(99).is_ok());
        assert!(validate_priority(-1).is_err());
        assert!(validate_priority(100).is_err());
    }

    #[test]
    fn issue_type_validation() {
        assert!(validate_issue_type("task").is_ok());
        assert!(validate_issue_type("feature").is_ok());
        assert!(validate_issue_type("bug").is_ok());
        assert!(validate_issue_type("spike").is_ok());
        assert!(validate_issue_type("research").is_ok());
        assert!(validate_issue_type("planning").is_ok());
        assert!(validate_issue_type("decomposition").is_ok()); // legacy compat
        assert!(validate_issue_type("review").is_ok());
        assert!(validate_issue_type("epic").is_err());
        assert!(validate_issue_type("").is_err());
    }

    #[test]
    fn label_validation() {
        assert!(validate_label("").is_err());
        assert!(validate_label("  ").is_err());
        assert_eq!(validate_label(" tag ").unwrap(), "tag");
        assert!(validate_label(&"x".repeat(50)).is_ok());
        assert!(validate_label(&"x".repeat(51)).is_err());
    }

    #[test]
    fn labels_count_validation() {
        assert!(validate_labels_count(0).is_ok());
        assert!(validate_labels_count(20).is_ok());
        assert!(validate_labels_count(21).is_err());
    }

    #[test]
    fn owner_validation() {
        assert_eq!(validate_owner("  alice  ").unwrap(), "alice");
        assert!(validate_owner(&"x".repeat(100)).is_ok());
        assert!(validate_owner(&"x".repeat(101)).is_err());
    }

    #[test]
    fn limit_and_offset() {
        assert_eq!(validate_limit(0), 1);
        assert_eq!(validate_limit(50), 50);
        assert_eq!(validate_limit(999), 200);
        assert_eq!(validate_offset(-5), 0);
        assert_eq!(validate_offset(10), 10);
    }

    #[test]
    fn sort_validation() {
        let allowed = &["priority", "created", "created_desc"];
        assert!(validate_sort("priority", allowed).is_ok());
        assert!(validate_sort("nope", allowed).is_err());
    }

    #[test]
    fn body_validation() {
        assert!(validate_body("").is_err());
        assert!(validate_body("hello").is_ok());
        assert!(validate_body(&"x".repeat(10_000)).is_ok());
        assert!(validate_body(&"x".repeat(10_001)).is_err());
    }

    #[test]
    fn mdx_body_valid_known_blocks() {
        let body = r#"
# Proposal

<RichText id="intro" content="Hello" />

<Diagram id="flow" type="mermaid">
graph TD;
</Diagram>

<AnnotatedCode id="example" language="rust">
fn main() {}
</AnnotatedCode>
"#;
        assert!(validate_mdx_body(body, Some("mdx")).is_ok());
    }

    #[test]
    fn mdx_body_valid_all_canonical_blocks() {
        // This is the Rust-side counterpart to the canonical proposal.mdx fixture
        // used by the UI test suite. It proves that backend validation accepts
        // every v1 block type when they appear together in a single MDX body.
        let body = r#"# Canonical Proposal

<RichText id="intro">
Welcome to the proposal.
</RichText>

<Diagram id="arch-overview" type="mermaid">
graph TD
  A[Client] --> B[API]
</Diagram>

<AnnotatedCode id="handler" language="rust">
fn handle_request(req: Request) -> Response {}
</AnnotatedCode>

<ApiEndpoint id="get-users" method="GET" path="/api/users">
Returns a list of all users.
</ApiEndpoint>

<Decisions id="auth-choice">
We chose JWT over session cookies.
</Decisions>

<FileTree id="project-layout" root="src">
  src/
    main.rs
</FileTree>

<Diff id="add-fn" filename="src/add.ts" lang="ts">
@@ -1,3 +1,4 @@
 export function add(a: number, b: number) {
-  return a + b;
+  const sum = a + b;
+  return sum;
 }
</Diff>

<Callout id="perf-note" tone="warning">
This runs on the hot path.
</Callout>

<Checklist id="acceptance">
- [x] Schema migration written
- [ ] Backfill job verified
</Checklist>

<JsonExplorer id="config-sample">
{
  "enabled": true,
  "retries": 3,
  "labels": ["alpha", "beta"]
}
</JsonExplorer>

<Tabs id="views" tabs={[{ "label": "Overview", "body": "Some **markdown**." }, { "label": "Detail", "body": "More detail." }]} />

<Columns id="compare" columns={[{ "body": "Before: the old approach." }, { "body": "After: the new approach." }]} />

<Wireframe id="signin-screen" surface="browser">
<div style="display:flex;flex-direction:column;gap:10px;padding:16px;height:100%">
  <h1>Sign in</h1>
  <div class="wf-card">
    <label>Email<input value="jane@acme.co" /></label>
    <button class="primary"><span data-icon="mail"></span> Continue</button>
  </div>
</div>
</Wireframe>

<QuestionForm id="open-questions" title="Open Questions">
Should we use Redis or Memcached?
</QuestionForm>
"#;
        assert!(validate_mdx_body(body, Some("mdx")).is_ok());
    }

    #[test]
    fn mdx_body_rejects_single_unknown_block() {
        let body = r#"
<RichText id="intro" />
<FooBar id="bad" />
"#;
        let err = validate_mdx_body(body, Some("mdx")).unwrap_err();
        assert!(
            err.contains("Unknown MDX block tag: 'FooBar'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn mdx_body_rejects_first_unknown_of_many() {
        let body = r#"
<RichText id="a" />
<BogusOne id="b" />
<AlsoBad id="c" />
"#;
        let err = validate_mdx_body(body, Some("mdx")).unwrap_err();
        assert!(
            err.contains("Unknown MDX block tag: 'BogusOne'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn mdx_body_nested_blocks_validated() {
        // A registered block nested inside another — both known, passes.
        let body = "<RichText>\n  <Diagram>\n    graph TD\n  </Diagram>\n</RichText>";
        assert!(validate_mdx_body(body, Some("mdx")).is_ok());

        // Unknown block nested inside a known one — rejected.
        let body = "<RichText>\n  <GhostBlock />\n</RichText>";
        let err = validate_mdx_body(body, Some("mdx")).unwrap_err();
        assert!(
            err.contains("Unknown MDX block tag: 'GhostBlock'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn mdx_body_skips_markdown_format() {
        // A `markdown` body containing PascalCase-ish text is not validated.
        let body = "<NotARealBlock> still fine </NotARealBlock>";
        assert!(validate_mdx_body(body, Some("markdown")).is_ok());
        assert!(validate_mdx_body(body, None).is_ok());
    }

    #[test]
    fn mdx_body_skips_empty_body() {
        assert!(validate_mdx_body("", Some("mdx")).is_ok());
        assert!(validate_mdx_body("   \n  ", Some("mdx")).is_ok());
    }

    #[test]
    fn mdx_body_ignores_lowercase_html() {
        let body = "<div>\n  <span>plain html</span>\n</div>";
        assert!(validate_mdx_body(body, Some("mdx")).is_ok());
    }

    #[test]
    fn mdx_body_wireframe_block_allows_benign_markup() {
        // A benign wireframe (tokens + helper classes + a data-icon marker)
        // passes the active-markup backstop.
        let body = r#"
<Wireframe id="ok" surface="desktop">
<div style="display:flex;flex-direction:column;gap:10px;padding:16px">
  <h1>Dashboard</h1>
  <button class="primary"><span data-icon="plus"></span> New</button>
</div>
</Wireframe>
"#;
        assert!(validate_mdx_body(body, Some("mdx")).is_ok());
    }

    #[test]
    fn mdx_body_wireframe_block_rejects_script_tag() {
        let body = r#"
<Wireframe id="bad" surface="desktop">
<div>hi</div>
<script>alert(1)</script>
</Wireframe>
"#;
        let err = validate_mdx_body(body, Some("mdx")).unwrap_err();
        assert!(err.contains("<script>"), "unexpected error: {err}");
        assert!(
            err.contains("Wireframe block 'bad'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn mdx_body_wireframe_block_rejects_event_handler() {
        let body = r#"
<Wireframe id="bad" surface="mobile">
<img src="x" onerror="alert(1)" />
</Wireframe>
"#;
        let err = validate_mdx_body(body, Some("mdx")).unwrap_err();
        assert!(
            err.contains("on*= event handler"),
            "unexpected error: {err}"
        );
        assert!(
            err.contains("Wireframe block 'bad'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn html_safety_does_not_affect_other_blocks() {
        // An `onClick`-looking handler inside a non-html block (e.g. RichText
        // prose / code) is NOT subject to the html active-markup rejection.
        let body = r#"
<RichText id="r">
Bind it with `el.onclick = fn` in the handler.
</RichText>
"#;
        assert!(validate_mdx_body(body, Some("mdx")).is_ok());
    }

    #[test]
    fn first_unsafe_html_reason_detects_obfuscated_scheme() {
        // Intra-scheme whitespace is collapsed before matching.
        assert!(first_unsafe_html_reason("<a href=\"java\nscript:x\">").is_some());
        assert!(first_unsafe_html_reason("<div>plain text</div>").is_none());
    }

    #[test]
    fn event_handler_detection_avoids_false_positives() {
        // `iron` / `button=` style substrings must not trip the on*= matcher.
        assert!(!has_event_handler_attribute(
            "<button class=\"iron\">on the wall</button>"
        ));
        assert!(has_event_handler_attribute("<div onmouseover=\"x\">"));
    }

    #[test]
    fn reason_validation() {
        assert!(validate_reason("").is_ok());
        assert!(validate_reason(&"x".repeat(2_000)).is_ok());
        assert!(validate_reason(&"x".repeat(2_001)).is_err());
    }

    #[test]
    fn actor_id_validation() {
        assert!(validate_actor_id("").is_ok());
        assert!(validate_actor_id(&"x".repeat(100)).is_ok());
        assert!(validate_actor_id(&"x".repeat(101)).is_err());
    }

    #[test]
    fn actor_role_validation() {
        assert!(validate_actor_role("").is_ok());
        assert!(validate_actor_role(&"x".repeat(50)).is_ok());
        assert!(validate_actor_role(&"x".repeat(51)).is_err());
    }

    #[test]
    fn ac_count_validation() {
        assert!(validate_ac_count(0).is_ok());
        assert!(validate_ac_count(50).is_ok());
        assert!(validate_ac_count(51).is_err());
    }

    #[test]
    fn epic_create_status_validation() {
        assert_eq!(validate_epic_create_status(None).unwrap(), None);
        assert_eq!(
            validate_epic_create_status(Some("open")).unwrap(),
            Some("open")
        );
        assert!(validate_epic_create_status(Some("drafting")).is_err());
        assert!(validate_epic_create_status(Some("proposed")).is_err());
        assert!(validate_epic_create_status(Some("closed")).is_err());
    }

    // ── resolve_body_format_and_validate ─────────────────────────────────────

    #[test]
    fn resolve_plain_markdown_stays_markdown() {
        // No block tags → stays markdown, no validation applied.
        assert_eq!(
            resolve_body_format_and_validate("# Title\n\nPlain prose.", Some("markdown")).unwrap(),
            "markdown"
        );
        assert_eq!(
            resolve_body_format_and_validate("just text", None).unwrap(),
            "markdown"
        );
    }

    #[test]
    fn resolve_markdown_with_block_tags_upgrades_to_mdx() {
        // A markdown-declared body carrying a known block tag is upgraded and
        // validated — this is the core cutover that closes the storage hole.
        let body = "<Callout id=\"c\" tone=\"info\">\nNote.\n</Callout>";
        assert_eq!(
            resolve_body_format_and_validate(body, Some("markdown")).unwrap(),
            "mdx"
        );
        // Omitted format upgrades the same way.
        assert_eq!(resolve_body_format_and_validate(body, None).unwrap(), "mdx");
    }

    #[test]
    fn resolve_markdown_with_unknown_block_tag_is_rejected() {
        // Previously passed silently (markdown skipped all validation).
        let body = "<FooBar id=\"x\" />";
        let err = resolve_body_format_and_validate(body, Some("markdown")).unwrap_err();
        assert!(err.contains("FooBar"), "error was: {err}");
    }

    #[test]
    fn resolve_rejects_empty_children_based_block() {
        // The production failure: a self-closing children-based block.
        let body = r#"<Decisions id="d" decisions={[{"decision":"x"}]} />"#;
        let err = resolve_body_format_and_validate(body, Some("markdown")).unwrap_err();
        assert!(err.contains("Decisions block"), "error was: {err}");
        assert!(err.contains("###"), "error was: {err}");
    }

    #[test]
    fn resolve_preserves_leaf_validation_errors_and_wireframe_backstop() {
        let malformed =
            resolve_body_format_and_validate("<Diagram id=\"flow\">", Some("mdx")).unwrap_err();
        assert_eq!(
            malformed,
            "unclosed <Diagram> block (no closing </Diagram> found)"
        );

        let empty_diagram =
            resolve_body_format_and_validate(r#"<Diagram id="flow" source="" />"#, Some("mdx"))
                .unwrap_err();
        assert_eq!(
            empty_diagram,
            "Diagram block `flow` has no source — provide a non-empty `source` (e.g. `source={`flowchart LR; A-->B`}`) or block content"
        );

        let unsafe_wireframe = resolve_body_format_and_validate(
            r#"<Wireframe id="bad"><script>alert(1)</script></Wireframe>"#,
            Some("mdx"),
        )
        .unwrap_err();
        assert_eq!(
            unsafe_wireframe,
            "Wireframe block 'bad' contains disallowed active markup (a <script> tag); the wireframe block renders sandboxed/sanitized static markup and is not a scripting surface"
        );
    }

    #[test]
    fn resolve_mdx_declared_empty_body_ok() {
        assert_eq!(
            resolve_body_format_and_validate("", Some("mdx")).unwrap(),
            "mdx"
        );
    }
}
