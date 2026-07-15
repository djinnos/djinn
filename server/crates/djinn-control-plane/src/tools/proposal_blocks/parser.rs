//! MDX-AST parse / validate functions for proposal block bodies.
//!
//! These walk a real MDX abstract syntax tree (mdast) — produced by the
//! `markdown` crate with a JSX-only construct set — instead of a regex, and
//! recognize the PascalCase component tags published in the catalog registry.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use markdown::mdast::{AttributeContent, AttributeValue, Node};
use markdown::{Constructs, ParseOptions};

use super::catalog::{proposal_block_definition_for_tag, proposal_block_tags};
use super::types::{BlockError, ParsedProposalBlock};

/// Build the [`ParseOptions`] used for every proposal-MDX parse.
///
/// We start from the MDX construct set and then disable the MDX **expression**
/// (`{...}` flow/text) and **ESM** (`import`/`export`) constructs, keeping ONLY
/// the JSX element/attribute grammar (`mdx_jsx_flow` + `mdx_jsx_text`).
///
/// Why: proposal block CHILDREN routinely contain bare `{ ... }` — raw JSON in
/// `JsonExplorer` blocks, braces in code. With expression parsing on,
/// markdown-rs tries to parse those as JS and fails ("could not parse
/// expression"). Block children are opaque markdown that each block component
/// re-parses itself, so we want `{...}` inside content left as literal text —
/// exactly as the old regex did. We also do NOT install an
/// `mdx_expression_parse`/`mdx_esm_parse` JS hook: without one, markdown-rs
/// captures `{...}` JSX *attribute* values as RAW (brace-balanced) text, which
/// is precisely the behavior we want (store the raw expression string, e.g. for
/// a forward-compat `tabs={[...]}` attribute) and never evaluates JS.
fn proposal_parse_options() -> ParseOptions {
    let constructs = Constructs {
        mdx_expression_flow: false,
        mdx_expression_text: false,
        mdx_esm: false,
        ..Constructs::mdx()
    };
    ParseOptions {
        constructs,
        ..ParseOptions::mdx()
    }
}

/// Returns `true` when `tag` follows the canonical PascalCase component
/// convention (starts with uppercase, then alphanumeric). This mirrors the old
/// `<([A-Z][A-Za-z0-9]*)` regex used to distinguish registered block tags from
/// ordinary lowercase HTML elements.
fn is_pascal_case_tag(tag: &str) -> bool {
    let mut chars = tag.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_alphanumeric())
}

/// Offset just after the opening tag's closing `>` (or `/>`), scanning from the
/// element's start offset while skipping quoted attribute values and `{...}`
/// expression depth. This is the start of the block's children-markdown content
/// and mirrors exactly where the old regex's content group began — preserving
/// the leading `\n` after `>` that the previous parser captured.
fn open_tag_end_offset(source: &[u8], start: usize) -> usize {
    let mut i = start + 1; // skip the leading '<'
    let mut depth: i32 = 0;
    let mut quote: Option<u8> = None;
    while i < source.len() {
        let ch = source[i];
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match ch {
            b'"' | b'\'' => quote = Some(ch),
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'>' if depth == 0 => return i + 1,
            _ => {}
        }
        i += 1;
    }
    i
}

/// A JSX element node (block or inline flavor) decomposed into the fields the
/// block parser needs. Adjacent JSX elements not separated by a blank line are
/// parsed as the inline (`MdxJsxTextElement`) flavor inside a paragraph, so both
/// variants must be matched for parity with the old blank-line-agnostic regex.
struct JsxElementRef<'a> {
    name: &'a str,
    attributes: &'a [AttributeContent],
    children: &'a [Node],
    position: Option<&'a markdown::unist::Position>,
}

fn as_jsx_element(node: &Node) -> Option<JsxElementRef<'_>> {
    match node {
        Node::MdxJsxFlowElement(e) => Some(JsxElementRef {
            name: e.name.as_deref()?,
            attributes: &e.attributes,
            children: &e.children,
            position: e.position.as_ref(),
        }),
        Node::MdxJsxTextElement(e) => Some(JsxElementRef {
            name: e.name.as_deref()?,
            attributes: &e.attributes,
            children: &e.children,
            position: e.position.as_ref(),
        }),
        _ => None,
    }
}

/// Collect every top-level (non-nested) PascalCase JSX element in document
/// order. We never descend *into* a matched block — its children are opaque
/// markdown sliced from source, exactly as the old non-greedy regex captured
/// them. Lowercase HTML elements (`<div>`) are skipped but recursed through so a
/// PascalCase block wrapped in a `<div>` is still found.
fn collect_block_elements<'a>(node: &'a Node, out: &mut Vec<&'a Node>) {
    if let Some(el) = as_jsx_element(node)
        && is_pascal_case_tag(el.name)
    {
        out.push(node);
        return; // do not descend into block children
    }
    if let Some(children) = node.children() {
        for child in children {
            collect_block_elements(child, out);
        }
    }
}

/// Resolve a JSX element's attributes into the flat `name -> string` map the
/// contract stores. String attributes keep their literal value; `{...}`
/// expression attributes store their raw expression text (forward-compat, e.g.
/// `tabs={[...]}`); bare/boolean attributes (`<Tag flag>`) become `"true"`.
/// `{...spread}` attributes are skipped (no name).
fn attributes_of(attributes: &[AttributeContent]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for attr in attributes {
        let AttributeContent::Property(prop) = attr else {
            continue; // skip {...spread}
        };
        let value = match &prop.value {
            None => "true".to_string(),
            Some(AttributeValue::Literal(s)) => s.clone(),
            Some(AttributeValue::Expression(expr)) => expr.value.trim().to_string(),
        };
        map.insert(prop.name.clone(), value);
    }
    map
}

/// Slice the children-markdown of a block element from the original `body`
/// between the end of the opening tag and the start of the closing tag, rather
/// than re-stringifying the mdast children (which would churn whitespace and
/// break the byte-identical export contract). Self-closing elements (no
/// children) yield an empty string.
fn block_raw_content(body: &str, el: &JsxElementRef<'_>) -> String {
    if el.children.is_empty() {
        return String::new();
    }
    let Some(pos) = el.position else {
        return String::new();
    };
    let start = pos.start.offset;
    let end = pos.end.offset;
    let content_start = open_tag_end_offset(body.as_bytes(), start);
    let close_start = end.saturating_sub(format!("</{}>", el.name).len());
    if close_start <= content_start {
        return String::new();
    }
    body[content_start..close_start].to_string()
}

/// Map a markdown-rs parse error to a [`BlockError`]. An end-tag mismatch for a
/// registered block surfaces as [`BlockError::UnclosedBlock`] (preserving the
/// old "no closing `</tag>`" semantics); anything else is a generic
/// [`BlockError::ParseError`].
fn mdx_error_to_block_error(message: &markdown::message::Message) -> BlockError {
    let reason = &message.reason;
    // markdown-rs phrases unclosed/mismatched JSX as:
    //   "Expected a closing tag for `<Diagram>` (…)" / "Unexpected closing tag …".
    if let Some(tag) = reason
        .split_once("for `<")
        .and_then(|(_, rest)| rest.split_once('>'))
        .map(|(tag, _)| tag.to_string())
        .filter(|t| proposal_block_definition_for_tag(t).is_some())
    {
        return BlockError::UnclosedBlock(tag);
    }
    BlockError::ParseError(reason.clone())
}

/// Extract registered proposal MDX blocks from a body string.
///
/// This walks a real MDX abstract syntax tree (mdast) — produced by the
/// `markdown` crate with the JSX-only construct set — instead of a regex. It
/// recognizes the PascalCase component tags published in the catalog registry
/// and skips unrelated HTML/MDX. `raw_content` is sliced verbatim from the
/// source `body` so it stays byte-identical to the old parser (and keeps
/// `proposal_export` round-tripping).
pub fn parse_mdx_blocks(body: &str) -> Result<Vec<ParsedProposalBlock>, BlockError> {
    if body.trim().is_empty() {
        return Ok(Vec::new());
    }
    let tree = markdown::to_mdast(body, &proposal_parse_options())
        .map_err(|e| mdx_error_to_block_error(&e))?;

    let mut nodes = Vec::new();
    collect_block_elements(&tree, &mut nodes);

    let mut blocks = Vec::new();
    for node in nodes {
        let el = as_jsx_element(node).expect("collected nodes are JSX elements");
        let Some(definition) = proposal_block_definition_for_tag(el.name) else {
            continue;
        };
        let attributes = attributes_of(el.attributes);
        let id = attributes.get("id").cloned().unwrap_or_default();
        let raw_content = block_raw_content(body, &el);

        blocks.push(ParsedProposalBlock {
            block_type: definition.block_type.to_string(),
            tag: el.name.to_string(),
            id,
            attributes,
            raw_content,
        });
    }

    Ok(blocks)
}

/// Recursively collect every PascalCase JSX element name in the AST, in
/// document order. Unlike [`collect_block_elements`] this DESCENDS into block
/// children so nested tags (known or unknown) are seen — required by the
/// nesting validation/extract semantics.
fn collect_jsx_names(node: &Node, out: &mut Vec<String>) {
    if let Some(el) = as_jsx_element(node) {
        if is_pascal_case_tag(el.name) {
            out.push(el.name.to_string());
        }
        for child in el.children {
            collect_jsx_names(child, out);
        }
        return;
    }
    if let Some(children) = node.children() {
        for child in children {
            collect_jsx_names(child, out);
        }
    }
}

/// All PascalCase JSX element names in `body`, in first-seen document order,
/// descending through nested blocks. Falls back to a lightweight opening-tag
/// scan when the body is not well-formed MDX (e.g. mismatched tags), preserving
/// the old regex's lenient behavior on malformed input.
fn pascal_case_tag_names(body: &str) -> Vec<String> {
    match markdown::to_mdast(body, &proposal_parse_options()) {
        Ok(tree) => {
            let mut names = Vec::new();
            collect_jsx_names(&tree, &mut names);
            names
        }
        Err(_) => scan_opening_tags(body),
    }
}

/// Lenient opening-tag scanner used only when MDX parsing fails. Mirrors the old
/// `<([A-Z][A-Za-z0-9]*)` regex: every opening PascalCase tag, in order, NOT
/// de-duplicated (callers dedupe as needed).
fn scan_opening_tags(body: &str) -> Vec<String> {
    static RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"<([A-Z][A-Za-z0-9]*)").expect("tag regex compiles"));
    RE.captures_iter(body)
        .map(|cap| cap[1].to_string())
        .collect()
}

/// Validate an MDX body against the block registry.
///
/// Walks the MDX AST collecting every PascalCase component name (descending
/// into nested blocks) and rejects the first unknown tag with
/// [`BlockError::UnknownBlock`]. Registered tags (including self-closing and
/// nested variants) pass silently. Empty or whitespace-only bodies are accepted.
pub fn validate_mdx_blocks(body: &str) -> Result<(), BlockError> {
    if body.trim().is_empty() {
        return Ok(());
    }
    let allowed = proposal_block_tags();
    for tag in pascal_case_tag_names(body) {
        if !allowed.contains(tag.as_str()) {
            return Err(BlockError::UnknownBlock(tag));
        }
    }
    // Don't-accept guard: a Diagram block must carry a source. An empty diagram
    // renders as a broken box for human reviewers, so reject it at authoring
    // time (block-patch / proposal_update) rather than letting it land.
    for block in parse_mdx_blocks(body)? {
        if block.block_type == "diagram" {
            let source_empty = block
                .attributes
                .get("source")
                .map(|s| s.trim().is_empty())
                .unwrap_or(true);
            if source_empty && block.raw_content.trim().is_empty() {
                return Err(BlockError::EmptyDiagram(block.id));
            }
        }
    }
    Ok(())
}

/// Enforce the MDX proposal question form placement contract.
///
/// The `question-form` block is OPTIONAL: an MDX proposal may contain zero or
/// more of them. Many proposals have no open questions, and requiring a
/// question form on every proposal made no sense.
///
/// No downstream flow reads the question-form block as a data source — it is a
/// read-only rendering construct (the UI simply renders each present block as an
/// "Open Questions" section, and renders nothing when none are present), so its
/// absence is safe. The refinement/debate/feedback flows operate on separate
/// concepts (e.g. the evidence-spike feasibility *question*), not this block.
///
/// The only remaining constraint is placement: when one or more question-form
/// blocks are present, the FINAL parsed proposal block must be a question-form,
/// so open questions render at the end of the proposal body.
pub fn validate_question_form_placement(body: &str) -> Result<(), String> {
    let blocks = parse_mdx_blocks(body).map_err(|e| e.to_string())?;
    let has_question_form = blocks
        .iter()
        .any(|block| block.block_type == "question-form");

    if has_question_form
        && !matches!(
            blocks.last(),
            Some(block) if block.block_type == "question-form"
        )
    {
        return Err(
            "The question-form block must be the last block in the proposal body".to_string(),
        );
    }

    Ok(())
}

/// Validate question-form placement for MDX bodies; markdown bodies intentionally
/// skip this block-level constraint.
pub fn validate_question_form_placement_for_format(
    body: &str,
    body_format: &str,
) -> Result<(), String> {
    if body_format == "mdx" {
        validate_question_form_placement(body)
    } else {
        Ok(())
    }
}

/// Extract all PascalCase component (JSX-like) tag names from an MDX body, in
/// first-seen order and de-duplicated. Unlike [`parse_mdx_blocks`], this does
/// NOT filter to registered tags — it returns every opening tag whose name
/// starts with an uppercase letter, so callers (e.g. body validation) can
/// detect unknown blocks. Lowercase HTML tags (`<div>`, `<span>`, …) and
/// closing tags (`</Tag>`) are intentionally ignored.
pub fn extract_custom_block_tags(body: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let mut seen = HashSet::new();
    for tag in pascal_case_tag_names(body) {
        if seen.insert(tag.clone()) {
            tags.push(tag);
        }
    }
    tags
}

/// The content-bearing ATTRIBUTE alternatives for a children-based block type.
///
/// `Some(&[...])` marks a block type as content-required: its visible content
/// comes from its CHILDREN, so a self-closing tag or blank children renders an
/// empty block. The slice lists attribute names that satisfy the content
/// requirement in lieu of children (an empty slice means children are the ONLY
/// content source). `None` means the block is not subject to this empty-content
/// check — either self-closing is legitimate, or the emptiness is guarded
/// elsewhere (e.g. `diagram` is covered by the empty-source guard in
/// [`validate_mdx_blocks`]).
///
/// The children-based set and the attribute alternatives are derived from the
/// catalog grammar in `proposal_blocks/catalog.rs`: `decisions`, `file-tree`,
/// `checklist`, `diff`, `json-explorer`, `wireframe`, and `callout` are pure
/// children blocks (`decisions`' `items=` / `file-tree`'s `entries=` prop forms
/// are NOT rendered by the UI, so children are mandatory); `rich-text` and
/// `annotated-code` accept a content attribute (`content` / `code`); `tabs` and
/// `columns` are self-closing containers whose content lives in the `tabs` /
/// `columns` attribute.
fn content_required_attrs(block_type: &str) -> Option<&'static [&'static str]> {
    match block_type {
        "rich-text" => Some(&["content"]),
        "annotated-code" => Some(&["code"]),
        "tabs" => Some(&["tabs"]),
        "columns" => Some(&["columns"]),
        "decisions" | "file-tree" | "checklist" | "diff" | "json-explorer" | "wireframe"
        | "callout" => Some(&[]),
        _ => None,
    }
}

/// Reject content-required blocks that arrive self-closing or with blank
/// children and no content-bearing attribute.
///
/// A children-based block (see [`content_required_attrs`]) that has neither
/// non-blank children nor a populated content attribute would validate as a
/// "known tag" yet render empty in the UI — the exact production failure where
/// `<Decisions id="x" decisions={[…]} />` (a children-based block written in the
/// unsupported attribute form) silently rendered nothing. The returned error
/// names the offending tag + id and states the expected grammar, reusing the
/// catalog description as the source of truth.
pub fn validate_block_content(blocks: &[ParsedProposalBlock]) -> Result<(), String> {
    for block in blocks {
        let Some(content_attrs) = content_required_attrs(&block.block_type) else {
            continue;
        };
        // Satisfied by non-blank children …
        if !block.raw_content.trim().is_empty() {
            continue;
        }
        // … or by a present, non-empty content-bearing attribute.
        let has_content_attr = content_attrs.iter().any(|name| {
            block
                .attributes
                .get(*name)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
        });
        if has_content_attr {
            continue;
        }
        return Err(empty_block_error(block, content_attrs));
    }
    Ok(())
}

/// Build an actionable error for an empty content-required block, naming the
/// tag/id, listing any attribute alternative, and quoting the catalog grammar.
fn empty_block_error(block: &ParsedProposalBlock, content_attrs: &[&str]) -> String {
    let id = if block.id.is_empty() {
        "<no id>".to_string()
    } else {
        format!("`{}`", block.id)
    };
    let mut msg = format!(
        "{} block {id} is empty: its content must be written as block children",
        block.tag
    );
    if !content_attrs.is_empty() {
        let attrs = content_attrs
            .iter()
            .map(|a| format!("`{a}`"))
            .collect::<Vec<_>>()
            .join(" or ");
        msg.push_str(&format!(" (or supplied via the {attrs} attribute)"));
    }
    msg.push_str(", but it arrived self-closing or with blank children and would render empty. ");
    if let Some(def) = proposal_block_definition_for_tag(&block.tag)
        && let Some(desc) = def.description
    {
        msg.push_str("Expected grammar — ");
        msg.push_str(desc);
    }
    msg
}

/// Ensure all parsed blocks have non-empty, unique `id` attributes.
pub fn validate_block_ids(blocks: &[ParsedProposalBlock]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for block in blocks {
        if block.id.is_empty() {
            return Err(format!(
                "{} block is missing a required `id` attribute",
                block.tag
            ));
        }
        if !seen.insert(block.id.as_str()) {
            return Err(format!("duplicate block id: `{}`", block.block_id()));
        }
    }
    Ok(())
}
