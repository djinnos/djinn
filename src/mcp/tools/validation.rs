// Shared input validation helpers for MCP tool parameters.
//
// Each function returns `Result<T, String>` where `Err` is a human-readable
// message suitable for returning as a JSON `{ "error": ... }` response.

/// Trim and validate a title: 1–200 chars.
pub fn validate_title(s: &str) -> Result<String, String> {
    let trimmed = s.trim().to_owned();
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

/// Validate issue_type: "task", "feature", or "bug".
pub fn validate_issue_type(s: &str) -> Result<(), String> {
    match s {
        "task" | "feature" | "bug" => Ok(()),
        other => Err(format!(
            "invalid issue_type: {other:?} (expected task, feature, or bug)"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_validation() {
        assert!(validate_title("").is_err());
        assert!(validate_title("   ").is_err());
        assert_eq!(validate_title("  hello  ").unwrap(), "hello");
        assert!(validate_title("x").is_ok());
        assert!(validate_title(&"x".repeat(200)).is_ok());
        assert!(validate_title(&"x".repeat(201)).is_err());
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
}
