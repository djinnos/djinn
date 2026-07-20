//! The V1 lint entry point and MDX structure rules.

use std::collections::HashSet;

use crate::parser::proposal_parse_options;
use crate::rules::duplicate_sections::lint_duplicate_sections;
use crate::{
    BodyFormat, Severity, SpecLintResultV1, Violation, analyze_mdx_document, validate_mdx_blocks,
};

/// Lint an exact proposal body using its effective stored format.
///
/// The caller supplies `checked_at` to keep results deterministic. Markdown is
/// deliberately not parsed as MDX; its result contains the V1 skipped-tier
/// record created by [`SpecLintResultV1::new`].
pub fn lint(
    body: &str,
    body_format: BodyFormat,
    checked_at: impl Into<String>,
) -> SpecLintResultV1 {
    let mut result = SpecLintResultV1::new(body, body_format, checked_at);

    // Keep the established registry parser and safety/round-trip behavior as
    // the source of truth. Lint has one stable structural failure code rather
    // than exposing parser implementation error variants to clients.
    if body_format == BodyFormat::Mdx {
        if let Err(error) = validate_mdx_blocks(body) {
            push_parse_error(&mut result, body, error.to_string());
            result.sort_violations();
            return result;
        }
    }

    let document = match analyze_mdx_document(body) {
        Ok(document) => document,
        Err(error) => {
            push_parse_error(&mut result, body, error.to_string());
            result.sort_violations();
            return result;
        }
    };

    let mut seen_ids = HashSet::new();
    for block in document.registered_blocks {
        if !block.id.is_empty() && !seen_ids.insert(block.id.clone()) {
            // An id comes from a parsed JSX attribute, so the anchored source
            // lexer must have found it. Falling back to the AST element span is
            // defensive and still preserves a valid source range.
            let span = block.id_value_span.unwrap_or(block.element_span);
            result.errors.push(
                Violation::new(
                    body,
                    "DUPLICATE_BLOCK_ID",
                    Severity::Error,
                    format!("duplicate registered block id: `{}`", block.id),
                    span.start,
                    span.end,
                )
                .expect("document analysis returns UTF-8 source spans"),
            );
        }
    }
    let tree = match markdown::to_mdast(body, &proposal_parse_options()) {
        Ok(tree) => tree,
        Err(error) => {
            push_parse_error(&mut result, body, error.reason);
            result.sort_violations();
            return result;
        }
    };
    for violation in lint_duplicate_sections(body, &tree) {
        match violation.severity {
            Severity::Error => result.errors.push(violation),
            Severity::Warning => result.warnings.push(violation),
        }
    }
    result.sort_violations();
    result
}

fn push_parse_error(result: &mut SpecLintResultV1, body: &str, detail: String) {
    result.errors.push(
        Violation::new(
            body,
            "MDX_PARSE_ERROR",
            Severity::Error,
            format!("MDX structure parsing failed: {detail}"),
            0,
            body.len(),
        )
        .expect("the complete body is always a UTF-8 range"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_skips_mdx_structure_without_parsing_it() {
        let body = "<Callout id=\"same\" />\n<Callout id=\"same\"";
        let result = lint(body, BodyFormat::Markdown, "fixed");
        assert!(result.errors.is_empty());
        assert_eq!(result.skipped_tiers.len(), 1);
        assert_eq!(result.skipped_tiers[0].tier, "mdx_structure");
        assert_eq!(result.skipped_tiers[0].reason, "BODY_FORMAT_MARKDOWN");
        assert_eq!(result.checked_at, "fixed");
    }

    #[test]
    fn malformed_mdx_becomes_a_stable_full_body_error() {
        let body = "before\n<Callout id=\"broken\">";
        let result = lint(body, BodyFormat::Mdx, "fixed");
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].code, "MDX_PARSE_ERROR");
        assert_eq!(result.errors[0].span.start, 0);
        assert_eq!(result.errors[0].span.end, body.len());
        result.validate_for_body(body).unwrap();
    }

    #[test]
    fn duplicate_ids_include_nested_third_and_unicode_offsets() {
        let body = concat!(
            "λ before\n",
            "<Callout id=\"duplicate\">\n",
            "  <Callout id=\"nested\">text</Callout>\n",
            "</Callout>\n",
            "<Callout id=\"duplicate\">second</Callout>\n",
            "<Callout id=\"duplicate\">third</Callout>"
        );
        let result = lint(body, BodyFormat::Mdx, "fixed");
        assert_eq!(result.errors.len(), 2);
        for violation in &result.errors {
            assert_eq!(violation.code, "DUPLICATE_BLOCK_ID");
            assert_eq!(&body[violation.span.start..violation.span.end], "duplicate");
        }
        let second = body.match_indices("id=\"duplicate\"").nth(1).unwrap().0 + 4;
        assert_eq!(result.errors[0].span.start, second);
        assert!(result.errors[0].span.start > "λ before\n".len());
        result.validate_for_body(body).unwrap();
    }

    #[test]
    fn duplicate_id_inside_mdx_fragment_is_detected() {
        let body = concat!(
            "<Callout id=\"same\">first</Callout>\n\n",
            "<>\n",
            "<Callout id=\"same\">second</Callout>\n",
            "</>"
        );
        let result = lint(body, BodyFormat::Mdx, "fixed");

        assert_eq!(result.errors.len(), 1);
        let violation = &result.errors[0];
        assert_eq!(violation.code, "DUPLICATE_BLOCK_ID");
        assert_eq!(&body[violation.span.start..violation.span.end], "same");
        let second = body.match_indices("id=\"same\"").nth(1).unwrap().0 + 4;
        assert_eq!(violation.span.start, second);
        result.validate_for_body(body).unwrap();
    }
}
