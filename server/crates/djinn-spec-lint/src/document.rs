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
}

/// The reusable source-positioned view of a successfully parsed MDX body.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DocumentAnalysis {
    pub registered_blocks: Vec<RegisteredBlockOccurrence>,
}

/// A markdown-rs MDX parse failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentError(pub String);

impl std::fmt::Display for DocumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
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
    Ok(DocumentAnalysis { registered_blocks })
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
            if name == "id" {
                return Utf8ByteSpan::new(body, name_start, index).ok();
            }
            continue;
        }
        index += 1;
        skip_whitespace(bytes, &mut index);
        let (start, end) = value_bounds(bytes, &mut index)?;
        if name == "id" {
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
            while bytes.get(*index) != Some(&quote) {
                *index += 1;
            }
            let end = *index;
            *index += 1;
            Some((start, end))
        }
        b'{' => {
            *index += 1;
            let start = *index;
            let mut depth = 1usize;
            while depth > 0 {
                match *bytes.get(*index)? {
                    b'{' => depth += 1,
                    b'}' => depth -= 1,
                    _ => {}
                }
                if depth > 0 {
                    *index += 1;
                }
            }
            let end = *index;
            *index += 1;
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
