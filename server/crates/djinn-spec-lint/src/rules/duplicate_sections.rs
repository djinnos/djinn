//! Deterministic heading-section duplicate detection.

use std::collections::BTreeSet;

use caseless::default_case_fold_str;
use markdown::mdast::{AttributeContent, AttributeValue, Node};
use unicode_normalization::UnicodeNormalization;

use crate::catalog::proposal_block_definition_for_tag;
use crate::{Severity, Utf8ByteSpan, Violation};

#[derive(Debug, Clone)]
struct Heading {
    depth: u8,
    span: Utf8ByteSpan,
    content_start: usize,
    normalized_text: String,
}

#[derive(Debug, Clone)]
struct TextFragment {
    start: usize,
    text: String,
}

/// Emit duplicate-section diagnostics for a successfully parsed document.
pub(crate) fn lint_duplicate_sections(body: &str, tree: &Node) -> Vec<Violation> {
    let mut headings = Vec::new();
    let mut fragments = Vec::new();
    collect(tree, &mut headings, &mut fragments);
    headings.sort_by_key(|heading| heading.span.start);
    fragments.sort_by_key(|fragment| fragment.start);

    let mut sections = Vec::new();
    for (index, heading) in headings.iter().enumerate() {
        let end = headings[index + 1..]
            .iter()
            .find(|next| next.depth <= heading.depth)
            .map_or(body.len(), |next| next.span.start);
        let tokens = fragments
            .iter()
            .filter(|fragment| fragment.start >= heading.content_start && fragment.start < end)
            .flat_map(|fragment| normalize_tokens(&fragment.text))
            .collect::<Vec<_>>();
        sections.push((heading, end, shingles(&tokens)));
    }

    let mut violations = Vec::new();
    for later in 0..sections.len() {
        let (later_heading, later_end, later_shingles) = &sections[later];
        let mut has_matching_heading = false;
        let mut has_duplicate_content = false;
        for (first_heading, _, first_shingles) in sections.iter().take(later) {
            if first_heading.depth != later_heading.depth
                || first_heading.normalized_text != later_heading.normalized_text
            {
                continue;
            }
            has_matching_heading = true;
            let intersection = first_shingles.intersection(later_shingles).count();
            let union = first_shingles.union(later_shingles).count();
            if union == 0 || intersection * 100 >= union * 60 {
                has_duplicate_content = true;
                break;
            }
        }
        if has_matching_heading {
            let (code, severity, span) = if has_duplicate_content {
                (
                    "DUPLICATE_SECTION_CONTENT",
                    Severity::Error,
                    Utf8ByteSpan::new(body, later_heading.span.start, *later_end)
                        .expect("AST heading positions address the original source"),
                )
            } else {
                (
                    "REPEATED_SECTION_HEADING",
                    Severity::Warning,
                    later_heading.span,
                )
            };
            violations.push(
                Violation::new(
                    body,
                    code,
                    severity,
                    format!(
                        "repeated section heading: `{}`",
                        later_heading.normalized_text
                    ),
                    span.start,
                    span.end,
                )
                .expect("section spans address the original source"),
            );
        }
    }
    violations
}

fn collect(node: &Node, headings: &mut Vec<Heading>, fragments: &mut Vec<TextFragment>) {
    match node {
        Node::Heading(heading) => {
            let Some(position) = heading.position.as_ref() else {
                return;
            };
            let span = Utf8ByteSpan {
                start: position.start.offset,
                end: position.end.offset,
            };
            let mut heading_fragments = Vec::new();
            for child in &heading.children {
                collect(child, &mut Vec::new(), &mut heading_fragments);
            }
            heading_fragments.sort_by_key(|fragment| fragment.start);
            headings.push(Heading {
                depth: heading.depth,
                span,
                content_start: span.end,
                normalized_text: heading_fragments
                    .iter()
                    .flat_map(|fragment| normalize_tokens(&fragment.text))
                    .collect::<Vec<_>>()
                    .join(" "),
            });
        }
        Node::Text(text) => {
            if let Some(position) = text.position.as_ref() {
                fragments.push(TextFragment {
                    start: position.start.offset,
                    text: text.value.clone(),
                });
            }
        }
        Node::MdxJsxFlowElement(element) => collect_element(
            element.name.as_deref(),
            &element.attributes,
            &element.children,
            element
                .position
                .as_ref()
                .map(|position| position.start.offset),
            headings,
            fragments,
        ),
        Node::MdxJsxTextElement(element) => collect_element(
            element.name.as_deref(),
            &element.attributes,
            &element.children,
            element
                .position
                .as_ref()
                .map(|position| position.start.offset),
            headings,
            fragments,
        ),
        Node::Code(_)
        | Node::InlineCode(_)
        | Node::MdxFlowExpression(_)
        | Node::MdxTextExpression(_) => {}
        _ => {
            if let Some(children) = node.children() {
                for child in children {
                    collect(child, headings, fragments);
                }
            }
        }
    }
}

fn collect_element(
    name: Option<&str>,
    attributes: &[AttributeContent],
    children: &[Node],
    start: Option<usize>,
    headings: &mut Vec<Heading>,
    fragments: &mut Vec<TextFragment>,
) {
    // Registered proposal blocks own opaque raw payloads.
    if name.is_some_and(|tag| proposal_block_definition_for_tag(tag).is_some()) {
        return;
    }
    if let Some(start) = start {
        for attribute in attributes {
            let AttributeContent::Property(property) = attribute else {
                continue;
            };
            if matches!(property.name.as_str(), "code" | "template") {
                continue;
            }
            if let Some(AttributeValue::Literal(value)) = &property.value {
                fragments.push(TextFragment {
                    start,
                    text: value.clone(),
                });
            }
        }
    }
    for child in children {
        collect(child, headings, fragments);
    }
}

/// NFKC followed by Unicode default case folding, then maximal letter-or-number
/// token extraction.
fn normalize_tokens(text: &str) -> Vec<String> {
    let normalized = text.nfkc().collect::<String>();
    let folded = default_case_fold_str(&normalized);
    let mut tokens = Vec::new();
    let mut token = String::new();
    for character in folded.chars() {
        if character.is_alphanumeric() {
            token.push(character);
        } else if !token.is_empty() {
            tokens.push(std::mem::take(&mut token));
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

fn shingles(tokens: &[String]) -> BTreeSet<Vec<String>> {
    if tokens.is_empty() {
        return BTreeSet::new();
    }
    if tokens.len() < 5 {
        return std::iter::once(tokens.to_vec()).collect();
    }
    tokens.windows(5).map(|window| window.to_vec()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::proposal_parse_options;

    fn violations(body: &str) -> Vec<Violation> {
        let tree = markdown::to_mdast(body, &proposal_parse_options()).unwrap();
        lint_duplicate_sections(body, &tree)
    }

    #[test]
    fn nested_content_belongs_to_parent_once_and_child_is_independent() {
        let body = concat!(
            "# Repeat\nalpha beta gamma delta epsilon\n",
            "## Child\nchild one two three four\n",
            "# Repeat\nalpha beta gamma delta epsilon\n",
            "## Child\nother one two three four\n",
        );
        let found = violations(body);
        assert_eq!(found.len(), 2);
        // The parent contains its nested child exactly once, so the differing
        // child prose lowers the parent similarity; the child remains compared.
        assert_eq!(found[0].code, "REPEATED_SECTION_HEADING");
        assert_eq!(found[1].code, "REPEATED_SECTION_HEADING");
    }

    #[test]
    fn normalization_and_shingles_follow_the_specified_order() {
        assert_eq!(
            normalize_tokens("ＦＯＯ, Straße! Σς ᾳ ẞ"),
            ["foo", "strasse", "σσ", "αι", "ss"]
        );
        let tokens = normalize_tokens("one two three four five six");
        assert_eq!(shingles(&tokens).len(), 2);
        assert!(shingles(&tokens).contains(&normalize_tokens("one two three four five")));
        let mdx_values = violations(
            "# ẞ\n<Widget title=\"ＦＯＯ Straße Σς ᾳ\" />\n# ss\n<Widget title=\"foo strasse σσ αι\" />\n",
        );
        assert_eq!(mdx_values[0].code, "DUPLICATE_SECTION_CONTENT");
        assert_eq!(mdx_values[0].severity, Severity::Error);
    }

    #[test]
    fn exact_boundary_is_error_and_lower_similarity_is_warning() {
        let error_body = "# Same\na b c d e f g h\n# Same\na b c d e f g x\n";
        let error = violations(error_body);
        assert_eq!(error[0].code, "DUPLICATE_SECTION_CONTENT");
        assert_eq!(error[0].severity, Severity::Error);
        assert_eq!(
            error[0].span.start,
            error_body.match_indices("# Same").nth(1).unwrap().0
        );
        assert_eq!(error[0].span.end, error_body.len());
        let first = (0..63)
            .map(|number| format!("a{number}"))
            .collect::<Vec<_>>()
            .join(" ");
        let second = (0..63)
            .map(|number| format!("a{number}"))
            .chain((0..41).map(|number| format!("b{number}")))
            .collect::<Vec<_>>()
            .join(" ");
        let first_shingles = shingles(&normalize_tokens(&first));
        let second_shingles = shingles(&normalize_tokens(&second));
        assert_eq!(first_shingles.intersection(&second_shingles).count(), 59);
        assert_eq!(first_shingles.union(&second_shingles).count(), 100);
        let warning_body = format!("# Same\n{first}\n# Same\n{second}\n");
        let warning = violations(&warning_body);
        assert_eq!(warning[0].code, "REPEATED_SECTION_HEADING");
        assert_eq!(warning[0].severity, Severity::Warning);
        assert_eq!(
            warning[0].span.start,
            warning_body.match_indices("# Same").nth(1).unwrap().0
        );
    }

    #[test]
    fn duplicate_content_against_any_prior_matching_heading_dominates() {
        let body = concat!(
            "# Same\n",
            "a b c d e\n",
            "# Same\n",
            "x y z q r\n",
            "# Same\n",
            "x y z q r\n",
        );
        let found = violations(body);
        assert_eq!(found.len(), 2);

        let second_start = body.match_indices("# Same").nth(1).unwrap().0;
        assert_eq!(found[0].code, "REPEATED_SECTION_HEADING");
        assert_eq!(found[0].severity, Severity::Warning);
        assert_eq!(found[0].span.start, second_start);
        assert_eq!(found[0].span.end, second_start + "# Same".len());

        let third_start = body.match_indices("# Same").nth(2).unwrap().0;
        assert_eq!(found[1].code, "DUPLICATE_SECTION_CONTENT");
        assert_eq!(found[1].severity, Severity::Error);
        assert_eq!(found[1].span.start, third_start);
        assert_eq!(found[1].span.end, body.len());
    }

    #[test]
    fn short_empty_levels_and_excluded_nodes_are_handled() {
        let short = violations("# Same\na b c\n# Same\na b c\n");
        assert_eq!(short[0].code, "DUPLICATE_SECTION_CONTENT");
        let empty = violations("# Same\n# Same\n");
        assert_eq!(empty[0].code, "DUPLICATE_SECTION_CONTENT");
        assert!(violations("# Same\na b c d e\n## Same\na b c d e\n").is_empty());
        for excluded_source in [
            "`a b c d e`",
            "```text\na b c d e\n```",
            "<Callout id=\"x\">a b c d e</Callout>",
            "<Widget template=\"a b c d e\" />",
            "<Widget code=\"a b c d e\" />",
        ] {
            let body = format!("# Same\na b c d e\n# Same\n{excluded_source}\n");
            let excluded = violations(&body);
            assert_eq!(excluded[0].code, "REPEATED_SECTION_HEADING");
            assert_eq!(excluded[0].severity, Severity::Warning);
            assert_eq!(
                excluded[0].span.start,
                body.match_indices("# Same").nth(1).unwrap().0
            );
        }

        // The label is eligible prose. Including the valid link destination
        // would make these section token streams identical, so this warning
        // proves that the destination itself remains excluded.
        let link_body = concat!(
            "# Same\n",
            "different https a b c d e example\n",
            "# Same\n",
            "[different](https://a-b-c-d-e.example)\n",
        );
        let link = violations(link_body);
        assert_eq!(link[0].code, "REPEATED_SECTION_HEADING");
        assert_eq!(link[0].severity, Severity::Warning);
        assert_eq!(
            link[0].span.start,
            link_body.match_indices("# Same").nth(1).unwrap().0
        );
    }
}
