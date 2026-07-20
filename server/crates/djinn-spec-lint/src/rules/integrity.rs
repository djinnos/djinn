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
    let mut references = Vec::new();
    let mut definitions = Vec::new();
    let mut excluded = registered_block_spans
        .iter()
        .map(|span| SourceRange {
            start: span.start,
            end: span.end,
        })
        .collect();
    collect(
        body,
        tree,
        &mut anchors,
        &mut text_nodes,
        &mut links,
        &mut references,
        &mut definitions,
        &mut excluded,
    );
    anchors.extend(registered_block_ids.iter().map(|id| normalize_anchor(id)));

    for text in text_nodes {
        lint_text(body, text, &mut violations);
    }
    lint_fences(body, &excluded, &mut violations);

    for reference in references {
        if let Some(definition) = definitions.iter().find(|definition| {
            definition
                .identifier
                .eq_ignore_ascii_case(&reference.identifier)
        }) {
            links.push(LinkOccurrence {
                url: definition.url.clone(),
                span: definition.span,
            });
        }
    }

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

#[derive(Debug)]
struct ReferenceOccurrence {
    identifier: String,
}

#[derive(Debug)]
struct DefinitionOccurrence {
    identifier: String,
    url: String,
    span: SourceRange,
}

fn collect(
    body: &str,
    node: &Node,
    anchors: &mut HashSet<String>,
    text_nodes: &mut Vec<SourceRange>,
    links: &mut Vec<LinkOccurrence>,
    references: &mut Vec<ReferenceOccurrence>,
    definitions: &mut Vec<DefinitionOccurrence>,
    excluded: &mut Vec<SourceRange>,
) {
    match node {
        Node::Heading(heading) => {
            let mut text = String::new();
            node_texts(&heading.children, &mut text);
            anchors.insert(normalize_anchor(&text));
            for child in &heading.children {
                collect(
                    body,
                    child,
                    anchors,
                    text_nodes,
                    links,
                    references,
                    definitions,
                    excluded,
                );
            }
        }
        Node::Text(text) => {
            if let Some(position) = &text.position {
                text_nodes.push(position_range(position));
            }
        }
        Node::Link(link) => {
            if let Some(position) = &link.position {
                links.push(LinkOccurrence {
                    url: link.url.clone(),
                    span: position_range(position),
                });
            }
            for child in &link.children {
                collect(
                    body,
                    child,
                    anchors,
                    text_nodes,
                    links,
                    references,
                    definitions,
                    excluded,
                );
            }
        }
        Node::LinkReference(link) => {
            references.push(ReferenceOccurrence {
                identifier: link.identifier.clone(),
            });
            for child in &link.children {
                collect(
                    body,
                    child,
                    anchors,
                    text_nodes,
                    links,
                    references,
                    definitions,
                    excluded,
                );
            }
        }
        Node::Definition(definition) => {
            if let Some(position) = &definition.position {
                definitions.push(DefinitionOccurrence {
                    identifier: definition.identifier.clone(),
                    url: definition.url.clone(),
                    span: position_range(position),
                });
            }
        }
        Node::MdxJsxFlowElement(element) => collect_element(
            body,
            element.name.as_deref(),
            &element.children,
            element.position.as_ref().map(position_range),
            anchors,
            text_nodes,
            links,
            references,
            definitions,
            excluded,
        ),
        Node::MdxJsxTextElement(element) => collect_element(
            body,
            element.name.as_deref(),
            &element.children,
            element.position.as_ref().map(position_range),
            anchors,
            text_nodes,
            links,
            references,
            definitions,
            excluded,
        ),
        Node::MdxTextExpression(expression) => add_position(&expression.position, excluded),
        Node::MdxFlowExpression(expression) => add_position(&expression.position, excluded),
        Node::Html(html) => add_position(&html.position, excluded),
        Node::Code(_) | Node::InlineCode(_) | Node::Image(_) | Node::ImageReference(_) => {}
        _ => {
            if let Some(children) = node.children() {
                for child in children {
                    collect(
                        body,
                        child,
                        anchors,
                        text_nodes,
                        links,
                        references,
                        definitions,
                        excluded,
                    );
                }
            }
        }
    }
}

fn collect_element(
    body: &str,
    name: Option<&str>,
    children: &[Node],
    position: Option<SourceRange>,
    anchors: &mut HashSet<String>,
    text_nodes: &mut Vec<SourceRange>,
    links: &mut Vec<LinkOccurrence>,
    references: &mut Vec<ReferenceOccurrence>,
    definitions: &mut Vec<DefinitionOccurrence>,
    excluded: &mut Vec<SourceRange>,
) {
    if name.is_some_and(|tag| proposal_block_definition_for_tag(tag).is_some()) {
        return;
    }
    if let Some(range) = position.and_then(|range| opening_tag_span(body, range.start)) {
        excluded.push(range);
    }
    for child in children {
        collect(
            body,
            child,
            anchors,
            text_nodes,
            links,
            references,
            definitions,
            excluded,
        );
    }
}

fn position_range(position: &markdown::unist::Position) -> SourceRange {
    SourceRange {
        start: position.start.offset,
        end: position.end.offset,
    }
}

fn add_position(position: &Option<markdown::unist::Position>, excluded: &mut Vec<SourceRange>) {
    if let Some(position) = position {
        excluded.push(position_range(position));
    }
}

fn opening_tag_span(body: &str, start: usize) -> Option<SourceRange> {
    let bytes = body.as_bytes();
    let (mut index, mut quote, mut braces) = (start, None, 0usize);
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(delimiter) = quote {
            if byte == delimiter {
                quote = None;
            }
        } else if matches!(byte, b'\'' | b'"') {
            quote = Some(byte);
        } else if byte == b'{' {
            braces += 1;
        } else if byte == b'}' {
            braces = braces.saturating_sub(1);
        } else if byte == b'>' && braces == 0 {
            return Some(SourceRange {
                start,
                end: index + 1,
            });
        }
        index += 1;
    }
    None
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
    let end = text[offset..]
        .find(|c: char| c.is_whitespace())
        .map_or(text.len(), |index| offset + index);
    let token = &text[start..end];
    token.starts_with("http://")
        || token.starts_with("https://")
        || token.starts_with("www.")
        || token.contains('/')
}

fn lint_fences(body: &str, excluded: &[SourceRange], violations: &mut Vec<Violation>) {
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
    let start = if let Some(open) = source.rfind('(') {
        let open = open + 1;
        let close = source[open..].find(')')? + open;
        source[open..close].find(url).map(|start| open + start)
    } else {
        source.find(url)
    }?;
    Some(SourceRange {
        start: span.start + start,
        end: span.start + start + url.len(),
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
    fn splice_candidates_stop_at_the_next_whitespace_token_boundary() {
        assert_eq!(
            codes("Broken.Thing /docs"),
            vec![("GLUED_TERMINAL_TOKEN".into(), ".".into())]
        );
    }

    #[test]
    fn multiline_html_and_jsx_properties_are_not_fences() {
        for body in [
            "<div data-template=\"\n~~~\nvalue\n\"></div>",
            "<Widget template={`\n~~~\nvalue\n`} />",
        ] {
            assert!(codes(body).is_empty(), "{body:?}");
        }
    }

    #[test]
    fn reference_links_resolve_definition_destinations() {
        assert_eq!(
            codes("[jump][target]\n\n[target]: #missing"),
            vec![("UNRESOLVED_LOCAL_REFERENCE".into(), "#missing".into())]
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
