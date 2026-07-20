//! Source-positioned prose splice, delimiter, and local-reference checks.

use std::collections::HashSet;

use markdown::mdast::Node;

use crate::catalog::proposal_block_definition_for_tag;
use crate::{Severity, Utf8ByteSpan, Violation};

#[derive(Debug, Clone, Copy)]
struct SourceRange {
    start: usize,
    end: usize,
}

/// Run the deterministic V1 integrity checks using the parsed document as the
/// authority on which source text is prose. Code, destinations, JSX properties,
/// and registered-block payloads are never treated as prose.
pub(crate) fn lint_integrity(
    body: &str,
    tree: &Node,
    registered_block_spans: &[Utf8ByteSpan],
    registered_block_ids: &[String],
) -> Vec<Violation> {
    let mut violations = Vec::new();
    let mut anchors = HashSet::new();
    let mut text_nodes = Vec::new();
    let mut links = Vec::new();
    collect(tree, &mut anchors, &mut text_nodes, &mut links);
    anchors.extend(registered_block_ids.iter().map(|id| normalize_anchor(id)));

    for text in text_nodes {
        lint_text(body, text, &mut violations);
    }
    lint_fences(body, registered_block_spans, &mut violations);

    for link in links {
        if !link.url.starts_with('#') {
            continue;
        }
        let target = &link.url[1..];
        if anchors.contains(&normalize_anchor(target)) {
            continue;
        }
        let span = link_destination_span(body, link.span, &link.url).unwrap_or(link.span);
        violations.push(violation(
            body,
            "UNRESOLVED_LOCAL_REFERENCE",
            Severity::Warning,
            "local markdown reference does not resolve",
            span.start,
            span.end,
        ));
    }
    violations
}

#[derive(Debug)]
struct LinkOccurrence {
    url: String,
    span: SourceRange,
}

fn collect(
    node: &Node,
    anchors: &mut HashSet<String>,
    text_nodes: &mut Vec<SourceRange>,
    links: &mut Vec<LinkOccurrence>,
) {
    match node {
        Node::Heading(heading) => {
            let mut text = String::new();
            node_texts(&heading.children, &mut text);
            anchors.insert(normalize_anchor(&text));
            for child in &heading.children {
                collect(child, anchors, text_nodes, links);
            }
        }
        Node::Text(text) => {
            if let Some(position) = &text.position {
                text_nodes.push(SourceRange {
                    start: position.start.offset,
                    end: position.end.offset,
                });
            }
        }
        Node::Link(link) => {
            if let Some(position) = &link.position {
                links.push(LinkOccurrence {
                    url: link.url.clone(),
                    span: SourceRange {
                        start: position.start.offset,
                        end: position.end.offset,
                    },
                });
            }
            // Labels are prose, destinations are represented only by `url`.
            for child in &link.children {
                collect(child, anchors, text_nodes, links);
            }
        }
        Node::MdxJsxFlowElement(element) => collect_element(
            element.name.as_deref(),
            &element.children,
            anchors,
            text_nodes,
            links,
        ),
        Node::MdxJsxTextElement(element) => collect_element(
            element.name.as_deref(),
            &element.children,
            anchors,
            text_nodes,
            links,
        ),
        Node::Code(_)
        | Node::InlineCode(_)
        | Node::Html(_)
        | Node::Image(_)
        | Node::ImageReference(_) => {}
        _ => {
            if let Some(children) = node.children() {
                for child in children {
                    collect(child, anchors, text_nodes, links);
                }
            }
        }
    }
}

fn collect_element(
    name: Option<&str>,
    children: &[Node],
    anchors: &mut HashSet<String>,
    text_nodes: &mut Vec<SourceRange>,
    links: &mut Vec<LinkOccurrence>,
) {
    // A registered block's contents are raw component payload, not document prose.
    if name.is_some_and(|tag| proposal_block_definition_for_tag(tag).is_some()) {
        return;
    }
    // JSX attributes are deliberately not traversed: they are properties/code,
    // rather than rendered Markdown text. Children of ordinary JSX remain prose.
    for child in children {
        collect(child, anchors, text_nodes, links);
    }
}

fn node_texts(nodes: &[Node], output: &mut String) {
    for node in nodes {
        match node {
            Node::Text(text) => output.push_str(&text.value),
            Node::InlineCode(_) | Node::Code(_) => {}
            _ => {
                if let Some(children) = node.children() {
                    node_texts(children, output);
                }
            }
        }
    }
}

fn lint_text(body: &str, range: SourceRange, violations: &mut Vec<Violation>) {
    let text = &body[range.start..range.end];
    for (offset, character) in text.char_indices() {
        if !matches!(character, '.' | '!' | '?') || url_or_path_token(text, offset) {
            continue;
        }
        let next = offset + character.len_utf8();
        let previous = text[..offset].chars().next_back();
        if previous.is_some_and(|c| c.is_numeric())
            && text[next..].chars().next().is_some_and(|c| c.is_numeric())
        {
            continue; // decimals and semantic-version components
        }
        if text[next..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '#')
        {
            violations.push(violation(
                body,
                "GLUED_TERMINAL_TOKEN",
                Severity::Error,
                "terminal punctuation is glued to the following token",
                range.start + offset,
                range.start + next,
            ));
        }
    }

    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'`' {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && bytes[index] == b'`' {
            index += 1;
        }
        violations.push(violation(
            body,
            "UNBALANCED_INLINE_BACKTICK",
            Severity::Error,
            "unmatched inline backtick delimiter",
            range.start + start,
            range.start + index,
        ));
    }
}

fn url_or_path_token(text: &str, offset: usize) -> bool {
    let start = text[..offset]
        .rfind(|c: char| c.is_whitespace())
        .map_or(0, |index| index + 1);
    let token = &text[start..];
    token.starts_with("http://")
        || token.starts_with("https://")
        || token.starts_with("www.")
        || token.contains('/')
}

fn lint_fences(body: &str, excluded: &[Utf8ByteSpan], violations: &mut Vec<Violation>) {
    let mut opening: Option<(u8, usize, usize)> = None;
    let mut line_start = 0;
    for line in body.split_inclusive('\n') {
        let line_end = line_start + line.len();
        if !excluded
            .iter()
            .any(|span| line_start >= span.start && line_start < span.end)
            && let Some((character, start, end, can_close)) = fence_run(line, line_start)
        {
            match opening {
                Some((open_character, open_start, open_end))
                    if can_close
                        && character == open_character
                        && end - start >= open_end - open_start =>
                {
                    opening = None;
                }
                None => opening = Some((character, start, end)),
                _ => {}
            }
        }
        line_start = line_end;
    }
    if let Some((_, start, end)) = opening {
        violations.push(violation(
            body,
            "UNBALANCED_CODE_FENCE",
            Severity::Error,
            "opening code fence has no compatible closing fence",
            start,
            end,
        ));
    }
}

fn fence_run(line: &str, absolute_start: usize) -> Option<(u8, usize, usize, bool)> {
    let bytes = line.as_bytes();
    let indent = bytes.iter().take_while(|byte| **byte == b' ').count();
    if indent > 3 {
        return None;
    }
    let character = *bytes.get(indent)?;
    if !matches!(character, b'`' | b'~') {
        return None;
    }
    let length = bytes[indent..]
        .iter()
        .take_while(|byte| **byte == character)
        .count();
    (length >= 3).then_some((
        character,
        absolute_start + indent,
        absolute_start + indent + length,
        bytes[indent + length..]
            .iter()
            .all(|byte| matches!(*byte, b' ' | b'\t' | b'\r' | b'\n')),
    ))
}

fn link_destination_span(body: &str, span: SourceRange, url: &str) -> Option<SourceRange> {
    let source = &body[span.start..span.end];
    let open = source.rfind('(')? + 1;
    let close = source[open..].find(')')? + open;
    let destination = &source[open..close];
    let start = destination.find(url)?;
    Some(SourceRange {
        start: span.start + open + start,
        end: span.start + open + start + url.len(),
    })
}

fn normalize_anchor(value: &str) -> String {
    let mut normalized = String::new();
    let mut hyphen = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() || character == '_' {
            normalized.push(character);
            hyphen = false;
        } else if !normalized.is_empty() {
            hyphen = true;
        }
        if hyphen && !normalized.ends_with('-') {
            normalized.push('-');
        }
    }
    normalized.trim_end_matches('-').to_string()
}

fn violation(
    body: &str,
    code: &str,
    severity: Severity,
    message: &str,
    start: usize,
    end: usize,
) -> Violation {
    Violation::new(body, code, severity, message, start, end)
        .expect("AST and source scanning return valid UTF-8 spans")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::proposal_parse_options;

    fn codes(body: &str) -> Vec<(String, String)> {
        let tree = markdown::to_mdast(body, &proposal_parse_options()).unwrap();
        lint_integrity(body, &tree, &[], &[])
            .into_iter()
            .map(|v| (v.code, body[v.span.start..v.span.end].to_string()))
            .collect()
    }

    #[test]
    fn eligible_prose_spans_unicode_delimiters_and_glued_tokens() {
        assert_eq!(
            codes("é.Thing and λ`"),
            vec![
                ("GLUED_TERMINAL_TOKEN".into(), ".".into()),
                ("UNBALANCED_INLINE_BACKTICK".into(), "`".into()),
            ]
        );
        assert_eq!(
            codes("é\n~~~\nraw"),
            vec![("UNBALANCED_CODE_FENCE".into(), "~~~".into())]
        );
    }

    #[test]
    fn excluded_and_non_splice_sources_are_clean() {
        for body in [
            "1.2.3 and `x.y`\n```\nx.y\n```",
            "https://example.test/a.b?x=y and path/to.a",
            "Retry after a timeout. then continue...",
            "[label](#missing)",
        ] {
            let found = codes(body);
            assert!(
                found
                    .iter()
                    .all(|(code, _)| code == "UNRESOLVED_LOCAL_REFERENCE")
            );
        }
    }
}
