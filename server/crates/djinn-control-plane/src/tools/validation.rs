// Shared input validation helpers for MCP tool parameters.
//
// Each function returns `Result<T, String>` where `Err` is a human-readable
// message suitable for returning as a JSON `{ "error": ... }` response.

use crate::tools::proposal_blocks::{extract_custom_block_tags, proposal_block_tags};

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
    Ok(())
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
}
