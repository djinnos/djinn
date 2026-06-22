//! Proposal MDX block registry and introspection tool.
//!
//! This module is the Rust source for the v1 proposal block contract: each
//! stable block type maps to the MDX tag clients should emit plus the field
//! schema workers can use when validating or generating proposal bodies.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

use markdown::mdast::{AttributeContent, AttributeValue, Node};
use markdown::{Constructs, ParseOptions};
use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;

/// Stable v1 proposal block types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockType {
    RichText,
    Diagram,
    AnnotatedCode,
    DataModel,
    ApiEndpoint,
    Decisions,
    FileTree,
    Diff,
    Callout,
    Table,
    Checklist,
    JsonExplorer,
    Html,
    OpenApiSpec,
    QuestionForm,
}

impl BlockType {
    /// Return the kebab-case block type identifier used in the registry.
    pub fn as_str(&self) -> &'static str {
        match self {
            BlockType::RichText => "rich-text",
            BlockType::Diagram => "diagram",
            BlockType::AnnotatedCode => "annotated-code",
            BlockType::DataModel => "data-model",
            BlockType::ApiEndpoint => "api-endpoint",
            BlockType::Decisions => "decisions",
            BlockType::FileTree => "file-tree",
            BlockType::Diff => "diff",
            BlockType::Callout => "callout",
            BlockType::Table => "table",
            BlockType::Checklist => "checklist",
            BlockType::JsonExplorer => "json-explorer",
            BlockType::Html => "html",
            BlockType::OpenApiSpec => "openapi-spec",
            BlockType::QuestionForm => "question-form",
        }
    }

    /// Return the MDX component tag name for this block type.
    pub fn tag(&self) -> &'static str {
        match self {
            BlockType::RichText => "RichText",
            BlockType::Diagram => "Diagram",
            BlockType::AnnotatedCode => "AnnotatedCode",
            BlockType::DataModel => "DataModel",
            BlockType::ApiEndpoint => "ApiEndpoint",
            BlockType::Decisions => "Decisions",
            BlockType::FileTree => "FileTree",
            BlockType::Diff => "Diff",
            BlockType::Callout => "Callout",
            BlockType::Table => "Table",
            BlockType::Checklist => "Checklist",
            BlockType::JsonExplorer => "JsonExplorer",
            BlockType::Html => "Html",
            BlockType::OpenApiSpec => "OpenApi",
            BlockType::QuestionForm => "QuestionForm",
        }
    }
}

/// Errors produced by the block registry parser and validator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockError {
    /// An unknown MDX block tag was encountered.
    UnknownBlock(String),
    /// A block is missing its required `id` attribute.
    MissingId(String),
    /// A block id is used more than once.
    DuplicateId(String),
    /// An opening tag has no matching closing tag.
    UnclosedBlock(String),
    /// A low-level regex or parsing failure.
    ParseError(String),
}

impl std::fmt::Display for BlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockError::UnknownBlock(tag) => write!(f, "Unknown MDX block tag: '{tag}'"),
            BlockError::MissingId(tag) => {
                write!(f, "{tag} block is missing a required `id` attribute")
            }
            BlockError::DuplicateId(id) => write!(f, "duplicate block id: `{id}`"),
            BlockError::UnclosedBlock(tag) => {
                write!(f, "unclosed <{tag}> block (no closing </{tag}> found)")
            }
            BlockError::ParseError(msg) => write!(f, "block parser error: {msg}"),
        }
    }
}

impl std::error::Error for BlockError {}

/// Registry mapping each [`BlockType`] to its MDX tag name and field schema.
#[derive(Debug, Clone)]
pub struct BlockRegistry {
    definitions: BTreeMap<&'static str, ProposalBlockDefinition>,
}

impl BlockRegistry {
    /// Create a new registry populated with the v1 block definitions.
    pub fn new() -> Self {
        Self {
            definitions: proposal_block_registry(),
        }
    }

    /// Look up a block definition by its MDX tag name.
    pub fn definition_for_tag(&self, tag: &str) -> Option<&ProposalBlockDefinition> {
        self.definitions.values().find(|def| def.tag == tag)
    }

    /// Return the set of known MDX tag names.
    pub fn tags(&self) -> HashSet<&str> {
        self.definitions.values().map(|def| def.tag).collect()
    }

    /// Return all registered block definitions.
    pub fn definitions(&self) -> &BTreeMap<&'static str, ProposalBlockDefinition> {
        &self.definitions
    }
}

impl Default for BlockRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ProposalBlockFieldSchema {
    /// Primitive schema kind: `string`, `boolean`, `object`, or `array`.
    #[serde(rename = "type")]
    pub field_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<&'static str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<BTreeMap<&'static str, ProposalBlockFieldSchema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<ProposalBlockFieldSchema>>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ProposalBlockDefinition {
    /// Stable proposal block type identifier, e.g. `annotated-code`.
    #[serde(rename = "type")]
    pub block_type: &'static str,
    /// MDX component tag, e.g. `AnnotatedCode`.
    pub tag: &'static str,
    /// Optional authoring guidance for the LLM: how to encode this block's
    /// children/attributes. Absent for blocks whose shape is self-evident.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<&'static str>,
    /// Field schema keyed by field name.
    pub fields: BTreeMap<&'static str, ProposalBlockFieldSchema>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalBlocksParams {}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ProposalBlocksResponse {
    pub blocks: BTreeMap<&'static str, ProposalBlockDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedProposalBlock {
    /// Stable proposal block type identifier, e.g. `annotated-code`.
    pub block_type: String,
    /// MDX component tag found in the body, e.g. `AnnotatedCode`.
    pub tag: String,
    /// Stable identifier for this block from the optional `id` attribute.
    pub id: String,
    /// All attributes from the opening tag, including `id` when present.
    pub attributes: HashMap<String, String>,
    /// Raw content between opening and closing tags.
    pub raw_content: String,
}

impl ParsedProposalBlock {
    pub fn block_id(&self) -> &str {
        &self.id
    }
}

fn string_field() -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "string",
        enum_values: None,
        fields: None,
        items: None,
    }
}

fn boolean_field() -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "boolean",
        enum_values: None,
        fields: None,
        items: None,
    }
}

fn enum_string_field(values: Vec<&'static str>) -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "string",
        enum_values: Some(values),
        fields: None,
        items: None,
    }
}

fn object_field(
    fields: BTreeMap<&'static str, ProposalBlockFieldSchema>,
) -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "object",
        enum_values: None,
        fields: Some(fields),
        items: None,
    }
}

fn array_field(items: ProposalBlockFieldSchema) -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "array",
        enum_values: None,
        fields: None,
        items: Some(Box::new(items)),
    }
}

fn fields(
    entries: Vec<(&'static str, ProposalBlockFieldSchema)>,
) -> BTreeMap<&'static str, ProposalBlockFieldSchema> {
    entries.into_iter().collect()
}

fn block(
    block_type: &'static str,
    tag: &'static str,
    fields: BTreeMap<&'static str, ProposalBlockFieldSchema>,
) -> ProposalBlockDefinition {
    ProposalBlockDefinition {
        block_type,
        tag,
        description: None,
        fields,
    }
}

fn block_with_description(
    block_type: &'static str,
    tag: &'static str,
    description: &'static str,
    fields: BTreeMap<&'static str, ProposalBlockFieldSchema>,
) -> ProposalBlockDefinition {
    ProposalBlockDefinition {
        block_type,
        tag,
        description: Some(description),
        fields,
    }
}

pub static PROPOSAL_BLOCK_REGISTRY: LazyLock<BTreeMap<&'static str, ProposalBlockDefinition>> =
    LazyLock::new(|| {
        BTreeMap::from([
            (
                "rich-text",
                block(
                    "rich-text",
                    "RichText",
                    fields(vec![("content", string_field())]),
                ),
            ),
            (
                "diagram",
                block(
                    "diagram",
                    "Diagram",
                    fields(vec![
                        (
                            "type",
                            enum_string_field(vec!["mermaid", "plantuml", "svg"]),
                        ),
                        ("source", string_field()),
                    ]),
                ),
            ),
            (
                "annotated-code",
                block(
                    "annotated-code",
                    "AnnotatedCode",
                    fields(vec![
                        ("language", string_field()),
                        ("code", string_field()),
                        (
                            "annotations",
                            array_field(object_field(fields(vec![
                                ("line", string_field()),
                                ("note", string_field()),
                            ]))),
                        ),
                    ]),
                ),
            ),
            (
                "data-model",
                block(
                    "data-model",
                    "DataModel",
                    fields(vec![
                        ("name", string_field()),
                        (
                            "fields",
                            array_field(object_field(fields(vec![
                                ("name", string_field()),
                                ("type", string_field()),
                                ("optional", boolean_field()),
                                ("description", string_field()),
                            ]))),
                        ),
                    ]),
                ),
            ),
            (
                "api-endpoint",
                block(
                    "api-endpoint",
                    "ApiEndpoint",
                    fields(vec![
                        ("method", string_field()),
                        ("path", string_field()),
                        ("description", string_field()),
                        ("request_schema", string_field()),
                        ("response_schema", string_field()),
                    ]),
                ),
            ),
            (
                "decisions",
                block_with_description(
                    "decisions",
                    "Decisions",
                    "ADR-style architecture decision records. The block CHILDREN are \
                     markdown: write ONE decision per `### Title` heading (`##` also \
                     accepted). DECLARE each decision's status with an explicit \
                     `Status:` line directly under the heading — one of exactly \
                     `proposed`, `accepted`, `rejected`, `deprecated`, `superseded` \
                     (or an inline `[accepted]` marker on the heading line). Status \
                     comes ONLY from this declared token; it is NEVER inferred from \
                     words in the prose. Omit the line for no status badge. A \
                     superseded record may name its replacement: \
                     `Status: superseded by #3`. Optionally structure the body with \
                     `Context`, `Decision`, and `Consequences` sub-section labels \
                     (plain label, `Context:`, `**Decision**`, or `#### Consequences` \
                     all work); otherwise the body renders as freeform markdown. \
                     Example: `<Decisions id=\"x\">\\n### Use JWT for stateless auth\\n\
                     Status: accepted\\n\\nContext\\nWe scale horizontally.\\n\\n\
                     Decision\\nAdopt short-lived JWTs.\\n</Decisions>`. A body with no \
                     `##`/`###` headings falls back to a plain markdown render.",
                    fields(vec![(
                        "items",
                        array_field(object_field(fields(vec![
                            ("decision", string_field()),
                            ("rationale", string_field()),
                            ("status", string_field()),
                        ]))),
                    )]),
                ),
            ),
            (
                "file-tree",
                block_with_description(
                    "file-tree",
                    "FileTree",
                    "A file & change tree. The block CHILDREN are the tree text: \
                     either an indented ASCII tree (folders end in `/`, files sit \
                     under a deeper indent) OR one slash-path per line. DECLARE each \
                     file's change status with a single leading token immediately \
                     followed by whitespace, then the path: `+` = added/new, `~` = \
                     modified, `-` = removed/deleted, `>` = renamed/moved; NO token \
                     means unchanged. Status comes ONLY from this token — words like \
                     `new`/`modified` are NOT inferred. A trailing note after the \
                     path (e.g. `~ src/x.rs — wire the route` or `src/y.ts: helper`) \
                     renders as a muted caption. Example: \
                     `<FileTree id=\"x\" root=\"src\">\\n+ src/added.rs\\n~ src/main.rs \
                     bump version\\n- src/old.rs\\n</FileTree>`. Unparseable bodies \
                     fall back to a raw monospace render.",
                    fields(vec![
                        ("root", string_field()),
                        (
                            "entries",
                            array_field(object_field(fields(vec![
                                ("path", string_field()),
                                ("kind", enum_string_field(vec!["file", "dir"])),
                            ]))),
                        ),
                    ]),
                ),
            ),
            (
                "diff",
                block_with_description(
                    "diff",
                    "Diff",
                    "A before/after code diff. The block CHILDREN are a git-style \
                     unified diff: optional `diff --git`/`---`/`+++`/`@@ -old,+new @@` \
                     headers, then body lines each prefixed with `+` (added line), \
                     `-` (removed line), or a single leading space (unchanged context \
                     line). Old and new line numbers are reconstructed from the `@@` \
                     hunk headers. Set the optional `filename` attribute to the file \
                     path (shown in the header) and the optional `lang` attribute to a \
                     language hint (e.g. `ts`, `rust`). Example: \
                     `<Diff id=\"x\" filename=\"src/add.ts\" lang=\"ts\">\\n@@ -1,2 +1,2 @@\\n \
                     const a = 1;\\n-const b = 2;\\n+const b = 3;\\n</Diff>`. If the \
                     children are not a valid unified diff the renderer falls back to a \
                     plain code block, so always emit real `+`/`-`/` ` prefixed lines.",
                    fields(vec![
                        ("filename", string_field()),
                        ("lang", string_field()),
                    ]),
                ),
            ),
            (
                "callout",
                block_with_description(
                    "callout",
                    "Callout",
                    "An emphasized admonition card with a `tone` and a markdown \
                     body. Set the optional `tone` attribute to one of `info` \
                     (default), `decision`, `risk`, `warning`, or `success`; it \
                     drives the card's color and icon. The block CHILDREN are the \
                     markdown body. Use it to flag a risk, a decision, or an \
                     important note so it stands out from surrounding prose. \
                     Example: `<Callout id=\"x\" tone=\"warning\">\\nThis runs on \
                     the hot path.\\n</Callout>`.",
                    fields(vec![(
                        "tone",
                        enum_string_field(vec![
                            "info", "decision", "risk", "warning", "success",
                        ]),
                    )]),
                ),
            ),
            (
                "table",
                block_with_description(
                    "table",
                    "Table",
                    "A clean styled data table. The block CHILDREN are a \
                     GitHub-flavoured markdown table: a header row, a `| --- | \
                     --- |` separator row, then one body row per line, each \
                     pipe-delimited (`| cell | cell |`). Cells may contain inline \
                     markdown. The renderer falls back to a plain markdown block \
                     if the children are not a valid pipe table, so always emit \
                     real `|`-delimited rows. Example: `<Table id=\"x\">\\n| Name | \
                     Type |\\n| --- | --- |\\n| id | uuid |\\n</Table>`.",
                    fields(vec![]),
                ),
            ),
            (
                "checklist",
                block_with_description(
                    "checklist",
                    "Checklist",
                    "A read-only checklist of items with their authored \
                     done-state. The block CHILDREN are GitHub task-list lines: \
                     `- [x] done item` (checked) or `- [ ] todo item` \
                     (unchecked); plain `- item` bullets render unchecked. Add an \
                     optional trailing note after ` — ` (em-dash) or two spaces. \
                     The boxes are NOT toggleable by viewers — they reflect the \
                     authored state only. Use it for acceptance criteria or \
                     done-criteria. Example: `<Checklist id=\"x\">\\n- [x] Schema \
                     written\\n- [ ] Docs updated — link the runbook\\n</Checklist>`.",
                    fields(vec![]),
                ),
            ),
            (
                "json-explorer",
                block_with_description(
                    "json-explorer",
                    "JsonExplorer",
                    "An interactive, collapsible typed JSON tree (browser-devtools \
                     / Postman style). The block CHILDREN are a single JSON \
                     document — an object, array, or primitive. The renderer \
                     `JSON.parse`s the children and walks the value: object/array \
                     nodes show a chevron + a one-line summary (`3 keys` / `5 \
                     items`) and expand/collapse, and leaf values are \
                     type-colored (string, number, boolean, null). It is \
                     read-only. If the children are not valid JSON the renderer \
                     falls back to a plain code block, so always emit a single \
                     valid JSON document. Set the optional `title` attribute to a \
                     heading shown in the block header. Example: \
                     `<JsonExplorer id=\"x\" title=\"Sample response\">\\n{\\n  \
                     \"id\": \"abc\",\\n  \"active\": true\\n}\\n</JsonExplorer>`.",
                    fields(vec![]),
                ),
            ),
            (
                "html",
                block_with_description(
                    "html",
                    "Html",
                    "Author/agent-supplied HTML rendered SAFELY. The block \
                     CHILDREN are the HTML document/fragment; it is sanitized \
                     (DOMPurify, stripping scripts, `on*=` handlers, and \
                     `javascript:` URLs) and rendered inside a fully sandboxed \
                     iframe (`sandbox=\"\"`, restrictive CSP, no scripts, no \
                     same-origin). Use it for rich static formatted markup or \
                     inline SVG that the other blocks can't express. It is NOT a \
                     scripting surface — `<script>`, event handlers, and \
                     `javascript:`/`data:text/html` URLs are rejected at \
                     validation time and stripped at render. Example: \
                     `<Html id=\"x\">\\n<div style=\\\"padding:8px\\\"><strong>Hi\
                     </strong></div>\\n</Html>`.",
                    fields(vec![]),
                ),
            ),
            (
                "openapi-spec",
                block_with_description(
                    "openapi-spec",
                    "OpenApi",
                    "A whole-document OpenAPI 3.x (or Swagger 2) API reference, \
                     rendered Redoc / Swagger-UI style. The block CHILDREN are a \
                     complete OpenAPI document as JSON (an `openapi`/`swagger` \
                     version, an `info` block, and a `paths` object; optional \
                     `tags`, `servers`, and `components`). The renderer \
                     `JSON.parse`s the children, resolves internal \
                     `$ref`s (e.g. `#/components/schemas/User`, cycle-guarded), \
                     groups operations by `tags` (operations with no tag fall into \
                     a `default` group), and renders each operation as a \
                     collapsible row: a colored METHOD pill + path + summary that \
                     expands to its parameters, request body, and per-status \
                     responses, with example bodies derived from the JSON schemas. \
                     It is read-only. YAML is NOT supported in v1 — emit JSON (a \
                     YAML or non-OpenAPI body falls back to a raw JSON render). \
                     Example: `<OpenApi id=\"x\">\\n{\\n  \"openapi\": \"3.0.0\",\\n  \
                     \"info\": { \"title\": \"My API\", \"version\": \"1.0.0\" },\\n  \
                     \"paths\": {}\\n}\\n</OpenApi>`.",
                    fields(vec![]),
                ),
            ),
            (
                "question-form",
                block(
                    "question-form",
                    "QuestionForm",
                    fields(vec![
                        ("title", string_field()),
                        (
                            "questions",
                            array_field(object_field(fields(vec![
                                ("question", string_field()),
                                ("kind", enum_string_field(vec!["text", "single", "multi"])),
                                ("options", array_field(string_field())),
                            ]))),
                        ),
                    ]),
                ),
            ),
        ])
    });

pub fn proposal_block_registry() -> BTreeMap<&'static str, ProposalBlockDefinition> {
    PROPOSAL_BLOCK_REGISTRY.clone()
}

pub fn proposal_block_definition_for_tag(tag: &str) -> Option<&'static ProposalBlockDefinition> {
    PROPOSAL_BLOCK_REGISTRY
        .values()
        .find(|definition| definition.tag == tag)
}

pub fn proposal_block_tags() -> HashSet<&'static str> {
    PROPOSAL_BLOCK_REGISTRY
        .values()
        .map(|definition| definition.tag)
        .collect()
}

/// Build the [`ParseOptions`] used for every proposal-MDX parse.
///
/// We start from the MDX construct set and then disable the MDX **expression**
/// (`{...}` flow/text) and **ESM** (`import`/`export`) constructs, keeping ONLY
/// the JSX element/attribute grammar (`mdx_jsx_flow` + `mdx_jsx_text`).
///
/// Why: proposal block CHILDREN routinely contain bare `{ ... }` — raw JSON in
/// `JsonExplorer`/`OpenApi` blocks, braces in code. With expression parsing on,
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
/// recognizes the PascalCase component tags published in
/// [`PROPOSAL_BLOCK_REGISTRY`] and skips unrelated HTML/MDX. `raw_content` is
/// sliced verbatim from the source `body` so it stays byte-identical to the old
/// parser (and keeps `proposal_export` round-tripping).
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
    Ok(())
}

/// Enforce the MDX proposal question form placement contract.
///
/// MDX proposals must contain exactly one registered `question-form` block, and
/// that block must be the final parsed proposal block in the body.
pub fn validate_question_form_placement(body: &str) -> Result<(), String> {
    let blocks = parse_mdx_blocks(body).map_err(|e| e.to_string())?;
    let question_form_count = blocks
        .iter()
        .filter(|block| block.block_type == "question-form")
        .count();

    if question_form_count != 1 {
        return Err(format!(
            "Exactly one question-form block is required in MDX proposals (found {question_form_count})"
        ));
    }

    if !matches!(
        blocks.last(),
        Some(block) if block.block_type == "question-form"
    ) {
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

#[tool_router(router = proposal_blocks_tool_router, vis = "pub")]
impl DjinnMcpServer {
    #[tool(
        description = "Return the v1 proposal MDX block registry, including stable block types, MDX tags, and field schemas.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn proposal_blocks(
        &self,
        Parameters(_): Parameters<ProposalBlocksParams>,
    ) -> Json<ProposalBlocksResponse> {
        Json(ProposalBlocksResponse {
            blocks: proposal_block_registry(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_v1_blocks() {
        let registry = proposal_block_registry();
        assert_eq!(registry.len(), 15);
        assert_eq!(registry["rich-text"].tag, "RichText");
        assert_eq!(registry["diagram"].tag, "Diagram");
        assert_eq!(registry["annotated-code"].tag, "AnnotatedCode");
        assert_eq!(registry["data-model"].tag, "DataModel");
        assert_eq!(registry["api-endpoint"].tag, "ApiEndpoint");
        assert_eq!(registry["decisions"].tag, "Decisions");
        assert_eq!(registry["file-tree"].tag, "FileTree");
        assert_eq!(registry["diff"].tag, "Diff");
        assert_eq!(registry["callout"].tag, "Callout");
        assert_eq!(registry["table"].tag, "Table");
        assert_eq!(registry["checklist"].tag, "Checklist");
        assert_eq!(registry["json-explorer"].tag, "JsonExplorer");
        assert_eq!(registry["html"].tag, "Html");
        assert_eq!(registry["openapi-spec"].tag, "OpenApi");
        assert_eq!(registry["question-form"].tag, "QuestionForm");
        // The html block ships authoring guidance noting it is sandboxed.
        assert!(
            registry["html"]
                .description
                .is_some_and(|d| d.contains("sandboxed iframe")),
            "html block must advertise its sandboxed/sanitized render"
        );
        // The openapi-spec block ships authoring guidance noting $ref resolution.
        assert!(
            registry["openapi-spec"]
                .description
                .is_some_and(|d| d.contains("OpenAPI") && d.contains("$ref")),
            "openapi-spec block must advertise OpenAPI/$ref authoring guidance"
        );
        // The diff block ships authoring guidance for the LLM.
        assert!(
            registry["diff"]
                .description
                .is_some_and(|d| d.contains("unified diff")),
            "diff block must advertise unified-diff authoring guidance"
        );
    }

    #[test]
    fn block_type_enum_covers_v1() {
        let types = vec![
            BlockType::RichText,
            BlockType::Diagram,
            BlockType::AnnotatedCode,
            BlockType::DataModel,
            BlockType::ApiEndpoint,
            BlockType::Decisions,
            BlockType::FileTree,
            BlockType::Diff,
            BlockType::Callout,
            BlockType::Table,
            BlockType::Checklist,
            BlockType::JsonExplorer,
            BlockType::Html,
            BlockType::OpenApiSpec,
            BlockType::QuestionForm,
        ];
        for bt in types {
            assert!(!bt.as_str().is_empty());
            assert!(!bt.tag().is_empty());
        }
    }

    #[test]
    fn block_registry_new_has_all_definitions() {
        let reg = BlockRegistry::new();
        assert_eq!(reg.definitions().len(), 15);
        assert!(reg.definition_for_tag("RichText").is_some());
        assert!(reg.definition_for_tag("UnknownTag").is_none());
        assert!(reg.tags().contains("RichText"));
        assert!(!reg.tags().contains("UnknownTag"));
    }

    #[test]
    fn registry_tags_match_canonical_v1_set() {
        // This test is the Rust-side parity guard for the TypeScript
        // CANONICAL_V1_TAGS array. If either side drifts, this assertion
        // (and the corresponding TS test) will fail.
        let expected: std::collections::HashSet<&str> = [
            "RichText",
            "Diagram",
            "AnnotatedCode",
            "DataModel",
            "ApiEndpoint",
            "Decisions",
            "FileTree",
            "Diff",
            "Callout",
            "Table",
            "Checklist",
            "JsonExplorer",
            "Html",
            "OpenApi",
            "QuestionForm",
        ]
        .into_iter()
        .collect();
        let actual: std::collections::HashSet<&str> = proposal_block_tags().into_iter().collect();
        assert_eq!(
            actual, expected,
            "Rust registry tags do not match the canonical v1 set"
        );
    }

    #[test]
    fn registry_contains_field_schemas() {
        let registry = proposal_block_registry();
        let diagram_type = registry["diagram"].fields["type"].clone();
        assert_eq!(diagram_type.field_type, "string");
        assert_eq!(
            diagram_type.enum_values.as_deref(),
            Some(["mermaid", "plantuml", "svg"].as_slice())
        );

        let question_kind = registry["question-form"].fields["questions"]
            .items
            .as_ref()
            .and_then(|items| items.fields.as_ref())
            .and_then(|fields| fields.get("kind"))
            .expect("question kind schema exists");
        assert_eq!(
            question_kind.enum_values.as_deref(),
            Some(["text", "single", "multi"].as_slice())
        );
    }

    #[test]
    fn parse_registered_mdx_blocks() {
        let body = r#"# Proposal

<RichText id="intro" content="Hello" />

<Diagram id='flow' type='mermaid'>
graph TD;
</Diagram>

<AnnotatedCode id="example" language="rust">
fn main() {}
</AnnotatedCode>"#;

        let blocks = parse_mdx_blocks(body).unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].block_type, "rich-text");
        assert_eq!(blocks[0].tag, "RichText");
        assert_eq!(blocks[0].id, "intro");
        assert!(blocks[0].raw_content.is_empty());
        assert_eq!(blocks[1].block_type, "diagram");
        assert_eq!(blocks[1].tag, "Diagram");
        assert_eq!(blocks[1].id, "flow");
        assert_eq!(
            blocks[1].attributes.get("type").map(String::as_str),
            Some("mermaid")
        );
        assert!(blocks[1].raw_content.contains("graph TD"));
        assert_eq!(blocks[2].block_type, "annotated-code");
    }

    #[test]
    fn parse_mdx_blocks_extracts_stable_ids() {
        let body = r#"
<DataModel id="user-schema" name="User" />
<ApiEndpoint id="create-user" method="POST" path="/users" />
"#;
        let blocks = parse_mdx_blocks(body).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].id, "user-schema");
        assert_eq!(blocks[1].id, "create-user");
    }

    #[test]
    fn parse_mdx_blocks_multiple_blocks_in_one_body() {
        let body = r#"# Proposal

<RichText id="intro" content="Hello" />
<Diagram id="flow" type="mermaid">
graph TD;
</Diagram>
<Decisions id="choices" />
<QuestionForm id="questions" title="Open questions" />
"#;
        let blocks = parse_mdx_blocks(body).unwrap();
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[0].tag, "RichText");
        assert_eq!(blocks[1].tag, "Diagram");
        assert_eq!(blocks[2].tag, "Decisions");
        assert_eq!(blocks[3].tag, "QuestionForm");
    }

    #[test]
    fn parse_mdx_blocks_empty_and_whitespace() {
        assert!(parse_mdx_blocks("").unwrap().is_empty());
        assert!(parse_mdx_blocks("   ").unwrap().is_empty());
        assert!(parse_mdx_blocks("\n\n  \n").unwrap().is_empty());
        assert!(
            parse_mdx_blocks("plain markdown with no blocks")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn validate_mdx_blocks_accepts_known_tags() {
        let body = r#"
<RichText id="intro" />
<Diagram id="flow" type="mermaid">
graph TD;
</Diagram>
"#;
        assert!(validate_mdx_blocks(body).is_ok());
    }

    #[test]
    fn validate_mdx_blocks_rejects_unknown_tag() {
        let body = r#"
<RichText id="intro" />
<FooBar id="bad" />
"#;
        let err = validate_mdx_blocks(body).unwrap_err();
        assert_eq!(err, BlockError::UnknownBlock("FooBar".to_string()));
        assert!(err.to_string().contains("Unknown MDX block tag: 'FooBar'"));
    }

    #[test]
    fn validate_mdx_blocks_rejects_first_unknown_of_many() {
        let body = r#"
<RichText id="a" />
<BogusOne id="b" />
<AlsoBad id="c" />
"#;
        let err = validate_mdx_blocks(body).unwrap_err();
        assert_eq!(err, BlockError::UnknownBlock("BogusOne".to_string()));
    }

    #[test]
    fn validate_mdx_blocks_empty_body() {
        assert!(validate_mdx_blocks("").is_ok());
        assert!(validate_mdx_blocks("   \n  ").is_ok());
    }

    #[test]
    fn validate_mdx_blocks_ignores_lowercase_html() {
        let body = "<div>\n  <span>plain html</span>\n</div>";
        assert!(validate_mdx_blocks(body).is_ok());
    }

    #[test]
    fn validate_mdx_blocks_nested_unknown_rejected() {
        let body = "<RichText>\n  <GhostBlock />\n</RichText>";
        let err = validate_mdx_blocks(body).unwrap_err();
        assert_eq!(err, BlockError::UnknownBlock("GhostBlock".to_string()));
    }

    #[test]
    fn parse_mdx_blocks_returns_unclosed_error() {
        let body = "<Diagram id=\"flow\" type=\"mermaid\">\ngraph TD;";
        let err = parse_mdx_blocks(body).unwrap_err();
        assert_eq!(err, BlockError::UnclosedBlock("Diagram".to_string()));
    }

    #[test]
    fn validate_ids_passes_with_unique_ids() {
        let blocks = vec![
            ParsedProposalBlock {
                block_type: "data-model".to_string(),
                tag: "DataModel".to_string(),
                id: "schema-a".to_string(),
                attributes: HashMap::new(),
                raw_content: String::new(),
            },
            ParsedProposalBlock {
                block_type: "decisions".to_string(),
                tag: "Decisions".to_string(),
                id: "decisions-1".to_string(),
                attributes: HashMap::new(),
                raw_content: String::new(),
            },
        ];
        assert!(validate_block_ids(&blocks).is_ok());
    }

    #[test]
    fn validate_ids_fails_on_empty() {
        let blocks = vec![ParsedProposalBlock {
            block_type: "data-model".to_string(),
            tag: "DataModel".to_string(),
            id: String::new(),
            attributes: HashMap::new(),
            raw_content: String::new(),
        }];
        let err = validate_block_ids(&blocks).unwrap_err();
        assert!(err.contains("missing a required `id`"));
    }

    #[test]
    fn validate_ids_fails_on_duplicate() {
        let blocks = vec![
            ParsedProposalBlock {
                block_type: "data-model".to_string(),
                tag: "DataModel".to_string(),
                id: "same-id".to_string(),
                attributes: HashMap::new(),
                raw_content: String::new(),
            },
            ParsedProposalBlock {
                block_type: "decisions".to_string(),
                tag: "Decisions".to_string(),
                id: "same-id".to_string(),
                attributes: HashMap::new(),
                raw_content: String::new(),
            },
        ];
        let err = validate_block_ids(&blocks).unwrap_err();
        assert!(err.contains("duplicate block id"));
    }

    #[test]
    fn test_validate_question_form_missing() {
        let body = r#"# Proposal

<DataModel id="schema" name="User" />"#;

        let err = validate_question_form_placement(body).unwrap_err();
        assert_eq!(
            err,
            "Exactly one question-form block is required in MDX proposals (found 0)"
        );
    }

    #[test]
    fn test_validate_question_form_multiple() {
        let body = r#"# Proposal

<QuestionForm id="questions-a" title="Open questions" />

<QuestionForm id="questions-b" title="More questions" />"#;

        let err = validate_question_form_placement(body).unwrap_err();
        assert_eq!(
            err,
            "Exactly one question-form block is required in MDX proposals (found 2)"
        );
    }

    #[test]
    fn test_validate_question_form_not_last() {
        let body = r#"# Proposal

<QuestionForm id="questions" title="Open questions" />

<Decisions id="decisions" />"#;

        let err = validate_question_form_placement(body).unwrap_err();
        assert_eq!(
            err,
            "The question-form block must be the last block in the proposal body"
        );
    }

    #[test]
    fn test_validate_question_form_valid() {
        let body = r#"# Proposal

<DataModel id="schema" name="User" />

<QuestionForm id="questions" title="Open questions" />"#;

        assert!(validate_question_form_placement(body).is_ok());
    }

    #[test]
    fn test_validate_question_form_markdown_skipped() {
        let body = "# Proposal\n\nPlain markdown proposal with no MDX blocks.";

        assert!(validate_question_form_placement_for_format(body, "markdown").is_ok());
    }

    #[test]
    fn extract_tags_returns_registered_and_unknown() {
        let body = r#"
# Proposal

<RichText id="intro" />
<UnknownBlock id="x" />
<Diagram id="flow">
graph TD;
</Diagram>
"#;
        let tags = extract_custom_block_tags(body);
        // PascalCase tags, first-seen order, de-duplicated.
        assert_eq!(tags, vec!["RichText", "UnknownBlock", "Diagram"]);
    }

    #[test]
    fn extract_tags_ignores_lowercase_and_closing_tags() {
        let body = "<div>\n<RichText />\n</RichText>\n</div>\n<span>hi</span>";
        let tags = extract_custom_block_tags(body);
        // `div`/`span` (lowercase) and `</RichText>` (closing) are ignored.
        assert_eq!(tags, vec!["RichText"]);
    }

    #[test]
    fn extract_tags_dedupes_repeated_tags() {
        let body = "<RichText />\n<RichText />\n<Diagram />";
        let tags = extract_custom_block_tags(body);
        assert_eq!(tags, vec!["RichText", "Diagram"]);
    }

    #[test]
    fn extract_tags_nested_blocks() {
        // A registered block nested inside another registered block — both
        // opening tags are extracted, enabling validation of nesting.
        let body = "<RichText>\n  <Diagram>\n    graph TD\n  </Diagram>\n</RichText>";
        let tags = extract_custom_block_tags(body);
        assert_eq!(tags, vec!["RichText", "Diagram"]);
    }

    #[test]
    fn extract_tags_empty_body() {
        assert!(extract_custom_block_tags("").is_empty());
        assert!(extract_custom_block_tags("plain markdown only").is_empty());
    }

    // ── AST-parser parity tests (regressions the old regex could not handle) ──

    #[test]
    fn parse_attribute_value_containing_gt() {
        // The old regex's `([^>]*)` attribute group stopped at the first `>`,
        // truncating both the `path` attribute and the raw_content. The AST
        // walker reads the full attribute and keeps the children intact.
        let body = r#"<ApiEndpoint id="x" path="/a?to=>b">body</ApiEndpoint>"#;
        let blocks = parse_mdx_blocks(body).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].tag, "ApiEndpoint");
        assert_eq!(blocks[0].id, "x");
        assert_eq!(
            blocks[0].attributes.get("path").map(String::as_str),
            Some("/a?to=>b"),
            "attribute value with a `>` must be captured in full"
        );
        assert_eq!(blocks[0].raw_content, "body");
    }

    #[test]
    fn parse_nested_same_tag_children_not_truncated() {
        // The old non-greedy `([\s\S]*?)</tag>` matched the FIRST close tag, so
        // a nested same-named block truncated the outer block's raw_content.
        // The AST slices the full children span between the OUTER open/close.
        let body = "<Callout id=\"outer\">\nbefore\n<Callout id=\"inner\">nested</Callout>\nafter\n</Callout>";
        let blocks = parse_mdx_blocks(body).unwrap();
        assert_eq!(blocks.len(), 1, "only the outer block is a top-level block");
        assert_eq!(blocks[0].id, "outer");
        assert_eq!(
            blocks[0].raw_content,
            "\nbefore\n<Callout id=\"inner\">nested</Callout>\nafter\n",
            "outer raw_content must contain the whole nested same-tag child"
        );
    }

    #[test]
    fn parse_json_expression_attribute_captured_as_raw_text() {
        // A `{...}` JSX attribute expression is stored as raw (brace-balanced)
        // text — never JS-evaluated — for forward-compat container blocks.
        let body = r#"<Diagram id="d" config={{ "a": 1 }} />"#;
        let blocks = parse_mdx_blocks(body).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].tag, "Diagram");
        assert_eq!(
            blocks[0].attributes.get("config").map(String::as_str),
            Some(r#"{ "a": 1 }"#),
            "expression attribute must store the raw inner expression text"
        );
        assert!(blocks[0].raw_content.is_empty());
    }

    #[test]
    fn parse_block_with_bare_json_children_no_error() {
        // JsonExplorer / OpenApi children are bare `{ ... }` JSON. The MDX
        // expression constructs are disabled, so this parses without error and
        // the raw_content bytes are preserved verbatim.
        let json = "\n{\n  \"id\": \"abc\",\n  \"nested\": { \"a\": [1, 2, 3] },\n  \"active\": true\n}\n";
        let body = format!("<JsonExplorer id=\"cfg\" title=\"Sample\">{json}</JsonExplorer>");
        let blocks = parse_mdx_blocks(&body).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_type, "json-explorer");
        assert_eq!(
            blocks[0].raw_content, json,
            "bare-brace JSON children must be preserved byte-for-byte"
        );

        // An OpenApi block with bare JSON children likewise parses cleanly.
        let oa = "\n{ \"openapi\": \"3.0.0\", \"paths\": {} }\n";
        let body = format!("<OpenApi id=\"api\">{oa}</OpenApi>");
        let blocks = parse_mdx_blocks(&body).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].raw_content, oa);
    }
}
