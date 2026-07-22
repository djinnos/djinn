//! Source-positioned MDX document analysis shared by lint rules.
//!
//! The markdown-rs AST supplies element offsets.  We use those offsets as the
//! anchor for a small opening-tag lexer solely to identify the source range of
//! an attribute value; it is not used to discover document structure.

use markdown::mdast::{AttributeContent, AttributeValue, Node};

use crate::Utf8ByteSpan;
use crate::catalog::proposal_block_definition_for_tag;
use crate::parser::proposal_parse_options;

/// A registered MDX component together with its exact source positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredBlockOccurrence {
    pub block_type: String,
    pub tag: String,
    pub id: String,
    pub element_span: Utf8ByteSpan,
    /// The source range of the `id` value, excluding its surrounding quotes or
    /// expression braces. `None` means the block has no `id` attribute.
    pub id_value_span: Option<Utf8ByteSpan>,
    /// Ranges for explicitly patchable code/template property values. The
    /// opening-tag lexer is anchored by this parsed MDX element's span.
    pub patchable_property_spans: Vec<Utf8ByteSpan>,
}

/// A source-positioned direct child of the parsed document root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopLevelNodeOccurrence {
    pub span: Utf8ByteSpan,
    pub heading_level: Option<u8>,
}

/// The reusable source-positioned view of a successfully parsed MDX body.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DocumentAnalysis {
    pub registered_blocks: Vec<RegisteredBlockOccurrence>,
    pub top_level_nodes: Vec<TopLevelNodeOccurrence>,
}

/// A markdown-rs MDX parse failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentError(pub String);

impl std::fmt::Display for DocumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Advance from the first byte after an opening quote to its unescaped closing
/// quote. The caller retains the closing-quote offset so value spans omit the
/// delimiters.
fn skip_quoted(bytes: &[u8], index: &mut usize, quote: u8) -> Option<()> {
    loop {
        match *bytes.get(*index)? {
            b'\\' => {
                *index += 1;
                bytes.get(*index)?;
                *index += 1;
            }
            byte if byte == quote => return Some(()),
            _ => *index += 1,
        }
    }
}

/// Skip one balanced JavaScript expression container. Strings and template
/// literals are opaque to brace balancing; `${...}` substitutions inside a
/// template recursively use the same balancing rule. This remains a small
/// opening-tag boundary lexer rather than a second document grammar.
fn skip_braced_expression(bytes: &[u8], index: &mut usize) -> Option<usize> {
    if bytes.get(*index) != Some(&b'{') {
        return None;
    }
    *index += 1;
    loop {
        match *bytes.get(*index)? {
            quote @ (b'\'' | b'"') => {
                *index += 1;
                skip_quoted(bytes, index, quote)?;
                *index += 1;
            }
            b'`' => skip_template_literal(bytes, index)?,
            b'/' if bytes.get(*index + 1) == Some(&b'/') => {
                *index += 2;
                while bytes.get(*index).is_some_and(|byte| *byte != b'\n') {
                    *index += 1;
                }
            }
            b'/' if bytes.get(*index + 1) == Some(&b'*') => {
                *index += 2;
                while !(bytes.get(*index) == Some(&b'*') && bytes.get(*index + 1) == Some(&b'/')) {
                    bytes.get(*index)?;
                    *index += 1;
                }
                *index += 2;
            }
            b'{' => {
                skip_braced_expression(bytes, index)?;
            }
            b'}' => {
                let closing = *index;
                *index += 1;
                return Some(closing);
            }
            _ => *index += 1,
        }
    }
}

fn skip_template_literal(bytes: &[u8], index: &mut usize) -> Option<()> {
    if bytes.get(*index) != Some(&b'`') {
        return None;
    }
    *index += 1;
    loop {
        match *bytes.get(*index)? {
            b'\\' => {
                *index += 1;
                bytes.get(*index)?;
                *index += 1;
            }
            b'`' => {
                *index += 1;
                return Some(());
            }
            b'$' if bytes.get(*index + 1) == Some(&b'{') => {
                *index += 1;
                skip_braced_expression(bytes, index)?;
            }
            _ => *index += 1,
        }
    }
}
impl std::error::Error for DocumentError {}

/// Parse MDX and collect every registered block, including nested blocks, in
/// document order. All spans address byte offsets in the exact supplied body.
pub fn analyze_mdx_document(body: &str) -> Result<DocumentAnalysis, DocumentError> {
    let tree = markdown::to_mdast(body, &proposal_parse_options())
        .map_err(|error| DocumentError(error.reason))?;
    let mut registered_blocks = Vec::new();
    collect_registered_blocks(body, &tree, &mut registered_blocks);
    let mut top_level_nodes = Vec::new();
    if let Node::Root(root) = &tree {
        for child in &root.children {
            if let Some(position) = child.position() {
                let span = Utf8ByteSpan::new(body, position.start.offset, position.end.offset)
                    .expect("markdown-rs node positions address the original source");
                top_level_nodes.push(TopLevelNodeOccurrence {
                    span,
                    heading_level: match child {
                        Node::Heading(heading) => Some(heading.depth),
                        _ => None,
                    },
                });
            }
        }
    }
    Ok(DocumentAnalysis {
        registered_blocks,
        top_level_nodes,
    })
}

fn has_property(attributes: &[AttributeContent], wanted: &str) -> bool {
    attributes.iter().any(|attribute| {
        matches!(attribute, AttributeContent::Property(property) if property.name == wanted)
    })
}

fn collect_registered_blocks(
    body: &str,
    node: &Node,
    occurrences: &mut Vec<RegisteredBlockOccurrence>,
) {
    match node {
        Node::MdxJsxFlowElement(element) => {
            collect_element(
                body,
                &element.name,
                &element.attributes,
                &element.children,
                element.position.as_ref(),
                occurrences,
            );
        }
        Node::MdxJsxTextElement(element) => {
            collect_element(
                body,
                &element.name,
                &element.attributes,
                &element.children,
                element.position.as_ref(),
                occurrences,
            );
        }
        _ => {
            if let Some(children) = node.children() {
                for child in children {
                    collect_registered_blocks(body, child, occurrences);
                }
            }
        }
    }
}

fn collect_element(
    body: &str,
    name: &Option<String>,
    attributes: &[AttributeContent],
    children: &[Node],
    position: Option<&markdown::unist::Position>,
    occurrences: &mut Vec<RegisteredBlockOccurrence>,
) {
    let Some(tag) = name.as_deref() else {
        // `name: None` represents an MDX fragment (`<>...</>`). Fragments do
        // not themselves have registered-block metadata, but their descendants
        // remain part of the document-wide traversal.
        for child in children {
            collect_registered_blocks(body, child, occurrences);
        }
        return;
    };
    if let (Some(definition), Some(position)) = (proposal_block_definition_for_tag(tag), position) {
        let id = attribute_value(attributes, "id").unwrap_or_default();
        let element_span = Utf8ByteSpan::new(body, position.start.offset, position.end.offset)
            .expect("markdown-rs element positions address the original source");
        occurrences.push(RegisteredBlockOccurrence {
            block_type: definition.block_type.to_string(),
            tag: tag.to_string(),
            id,
            element_span,
            id_value_span: id_value_span(body, position.start.offset),
            patchable_property_spans: ["code", "template"]
                .into_iter()
                .filter(|name| has_property(attributes, name))
                .filter_map(|name| property_value_span(body, position.start.offset, name))
                .collect(),
        });
    }
    for child in children {
        collect_registered_blocks(body, child, occurrences);
    }
}

fn attribute_value(attributes: &[AttributeContent], wanted: &str) -> Option<String> {
    attributes.iter().find_map(|attribute| {
        let AttributeContent::Property(property) = attribute else {
            return None;
        };
        if property.name != wanted {
            return None;
        }
        Some(match &property.value {
            None => "true".to_string(),
            Some(AttributeValue::Literal(value)) => value.clone(),
            Some(AttributeValue::Expression(value)) => value.value.trim().to_string(),
        })
    })
}

/// Locate an `id` attribute value inside the opening tag anchored by an AST
/// element start. Quoted, braced, and bare values are handled without applying
/// a document-wide regular expression.
fn id_value_span(body: &str, element_start: usize) -> Option<Utf8ByteSpan> {
    property_value_span(body, element_start, "id")
}

/// Locate a property value in an opening tag anchored by an AST element start.
/// This is intentionally not a document-wide selector grammar.
fn property_value_span(body: &str, element_start: usize, wanted: &str) -> Option<Utf8ByteSpan> {
    let bytes = body.as_bytes();
    let mut index = element_start + 1;
    while index < bytes.len() && !bytes[index].is_ascii_whitespace() && bytes[index] != b'>' {
        index += 1;
    }
    while index < bytes.len() {
        skip_whitespace(bytes, &mut index);
        match bytes.get(index)? {
            b'>' | b'/' => return None,
            b'{' => {
                skip_braced(bytes, &mut index)?;
                continue;
            }
            _ => {}
        }
        let name_start = index;
        while index < bytes.len()
            && !bytes[index].is_ascii_whitespace()
            && !matches!(bytes[index], b'=' | b'>' | b'/')
        {
            index += 1;
        }
        let name = &body[name_start..index];
        skip_whitespace(bytes, &mut index);
        if bytes.get(index) != Some(&b'=') {
            if name == wanted {
                return Utf8ByteSpan::new(body, name_start, index).ok();
            }
            continue;
        }
        index += 1;
        skip_whitespace(bytes, &mut index);
        let (start, end) = value_bounds(bytes, &mut index)?;
        if name == wanted {
            return Utf8ByteSpan::new(body, start, end).ok();
        }
    }
    None
}

fn skip_whitespace(bytes: &[u8], index: &mut usize) {
    while bytes.get(*index).is_some_and(u8::is_ascii_whitespace) {
        *index += 1;
    }
}

fn value_bounds(bytes: &[u8], index: &mut usize) -> Option<(usize, usize)> {
    match *bytes.get(*index)? {
        quote @ (b'\'' | b'"') => {
            *index += 1;
            let start = *index;
            skip_quoted(bytes, index, quote)?;
            let end = *index;
            *index += 1;
            Some((start, end))
        }
        b'{' => {
            let start = *index + 1;
            let end = skip_braced_expression(bytes, index)?;
            Some((start, end))
        }
        _ => {
            let start = *index;
            while bytes
                .get(*index)
                .is_some_and(|byte| !byte.is_ascii_whitespace() && !matches!(*byte, b'>' | b'/'))
            {
                *index += 1;
            }
            Some((start, *index))
        }
    }
}

fn skip_braced(bytes: &[u8], index: &mut usize) -> Option<()> {
    let (_, end) = value_bounds(bytes, index)?;
    *index = end + 1;
    Some(())
}

#[cfg(test)]
mod tests {
    use super::property_value_span;

    #[test]
    fn braced_property_span_ignores_braces_inside_strings_and_templates() {
        for (source, property, selected) in [
            (
                "<RichText id=\"r\" template={`Hello } later`} />",
                "template",
                "later",
            ),
            (
                "<AnnotatedCode id=\"a\" code={`const close = \"}\"; later();`} />",
                "code",
                "later()",
            ),
        ] {
            let span = property_value_span(source, 0, property).unwrap();
            let selected_start = source.find(selected).unwrap();
            assert!(span.start <= selected_start);
            assert!(span.end >= selected_start + selected.len());
        }
    }

    #[test]
    fn property_scan_skips_spoofing_text_in_complete_attribute_values() {
        let source = r#"<RichText label="escaped \" code={`quoted spoof`}" meta={{ nested: { text: "template={`braced spoof`}" } }} id="r" />"#;
        assert!(property_value_span(source, 0, "code").is_none());
        assert!(property_value_span(source, 0, "template").is_none());
    }
}
