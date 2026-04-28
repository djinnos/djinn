use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use protobuf::{Enum, Message};
use scip::types::{
    Descriptor, Document, Index, Metadata, Occurrence, Relationship, SymbolInformation,
    symbol_information,
};
use serde::{Deserialize, Serialize};

use crate::scip_indexer::ScipArtifact;

/// Normalized SCIP payload ready for graph construction without exposing protobuf details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedScipIndex {
    pub metadata: ScipMetadata,
    pub files: Vec<ScipFile>,
    pub external_symbols: Vec<ScipSymbol>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScipMetadata {
    pub project_root: Option<String>,
    pub tool_name: Option<String>,
    pub tool_version: Option<String>,
}

/// A source file and the structural symbol data SCIP reported for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScipFile {
    pub language: String,
    pub relative_path: PathBuf,
    pub definitions: Vec<ScipOccurrence>,
    pub references: Vec<ScipOccurrence>,
    pub occurrences: Vec<ScipOccurrence>,
    pub symbols: Vec<ScipSymbol>,
}

/// A normalized occurrence of a symbol in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScipOccurrence {
    pub symbol: String,
    pub range: ScipRange,
    pub enclosing_range: Option<ScipRange>,
    pub roles: BTreeSet<ScipSymbolRole>,
    pub syntax_kind: Option<String>,
    pub override_documentation: Vec<String>,
}

/// Expanded source range. SCIP stores 3- or 4-element packed ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScipRange {
    pub start_line: i32,
    pub start_character: i32,
    pub end_line: i32,
    pub end_character: i32,
}

/// A symbol defined or declared in a file, with outbound semantic relationships.
///
/// PR C1 added the `signature_parts` field. SCIP 0.7's
/// `signature_documentation` is a markdown blob (a `Document` proto with
/// only a `text` field), so for the indexers we ship today
/// `signature_parts` is `None`. The slot exists so downstream consumers
/// (e.g. `code_graph context`'s `MethodMeta` extraction) have a
/// uniform structured surface to read from when a future SCIP version
/// or indexer emits parameter / return-type fields. Per the plan
/// contract: NEVER regex the markdown — leave `signature_parts: None`
/// when structured proto fields are absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScipSymbol {
    pub symbol: String,
    pub kind: Option<ScipSymbolKind>,
    pub display_name: Option<String>,
    pub signature: Option<String>,
    pub documentation: Vec<String>,
    pub relationships: Vec<ScipRelationship>,
    pub visibility: Option<ScipVisibility>,
    /// Optional structured signature parsed from a richer `scip::Signature`
    /// proto. Populated only when the upstream indexer emits structured
    /// parameter / return-type fields. None for the vanilla SCIP 0.7
    /// schema's markdown-only `signature_documentation`.
    pub signature_parts: Option<ScipSignatureParts>,
}

/// PR C1: structured method signature lifted from the SCIP proto when
/// available. Mirrors the shape `MethodMeta` exposes through the
/// bridge: parameter names + types, optional return type, optional
/// type-parameter names, and pass-through visibility / async / annotation
/// hints.
///
/// These fields stay `Option` / `Vec` so partial population (e.g. an
/// indexer that emits only parameters) still surfaces what it has.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScipSignatureParts {
    pub parameters: Vec<ScipSignatureParam>,
    pub return_type: Option<String>,
    pub type_parameters: Vec<String>,
    pub visibility: Option<String>,
    pub is_async: Option<bool>,
    pub annotations: Vec<String>,
}

/// PR C1: a single structured parameter on a method/function signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScipSignatureParam {
    pub name: String,
    pub type_name: Option<String>,
    pub default_value: Option<String>,
}

impl Default for ScipSymbol {
    fn default() -> Self {
        ScipSymbol {
            symbol: String::new(),
            kind: None,
            display_name: None,
            signature: None,
            documentation: Vec::new(),
            relationships: Vec::new(),
            visibility: None,
            signature_parts: None,
        }
    }
}

/// True for SCIP `local` identifiers (descriptor prefix `local `, e.g.
/// `local 0`, `local 42`).
///
/// SCIP scopes locals per-document, so an index that names a function-internal
/// variable as `local 0` in `dispatcher.go` and another as `local 0` in
/// `backfill.go` is referring to two distinct entities. Our graph keys symbols
/// by their raw SCIP id, which means those distinct entities collapse into a
/// single super-node and accumulate fan-in across the whole repository.
///
/// Locals carry no architectural signal at the project graph level, so the
/// parser drops them entirely (see [`parse_index`]) and the snapshot/ranking
/// tier never sees them. This helper centralizes the prefix check so callers
/// (visibility classification, the parse-time filter, downstream defenses)
/// stay in sync.
pub fn is_local_symbol(symbol: &str) -> bool {
    symbol.starts_with("local ")
}

/// Symbol visibility, derived from the SCIP symbol identifier shape.
///
/// SCIP 0.7 does not carry a dedicated visibility flag on `SymbolInformation`,
/// so we approximate it: identifiers prefixed with `local ` are document-local
/// (treated as `Private`); all other global identifiers are reachable across
/// documents and treated as `Public`. Anything we cannot classify falls back to
/// `Unknown`.
///
/// Locals are filtered out at parse time (see [`is_local_symbol`] and
/// [`parse_index`]), so in practice the `Private` arm is unreachable for
/// parser output today; the variant is kept because downstream code
/// (`mcp_bridge`) accepts user-provided visibility filters that include
/// `private`, and the API surface stays stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScipVisibility {
    Public,
    Private,
    Unknown,
}

impl ScipVisibility {
    pub fn from_symbol_identifier(symbol: &str) -> Self {
        if symbol.is_empty() {
            ScipVisibility::Unknown
        } else if is_local_symbol(symbol) {
            ScipVisibility::Private
        } else {
            ScipVisibility::Public
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ScipVisibility::Public => "public",
            ScipVisibility::Private => "private",
            ScipVisibility::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScipRelationship {
    pub source_symbol: String,
    pub target_symbol: String,
    pub kinds: BTreeSet<ScipRelationshipKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScipRelationshipKind {
    Reference,
    Implementation,
    TypeDefinition,
    Definition,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScipSymbolRole {
    Definition,
    Import,
    WriteAccess,
    ReadAccess,
    Generated,
    Test,
    ForwardDefinition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScipSymbolKind {
    Package,
    Namespace,
    Type,
    Method,
    Property,
    Field,
    Constructor,
    Enum,
    Interface,
    Function,
    Variable,
    Constant,
    String,
    Number,
    Boolean,
    Array,
    Object,
    Key,
    Null,
    EnumMember,
    Struct,
    Event,
    Operator,
    Unknown(i32),
}

pub fn parse_scip_artifacts(artifacts: &[ScipArtifact]) -> Result<Vec<ParsedScipIndex>> {
    artifacts
        .iter()
        .map(|artifact| parse_scip_file(&artifact.path))
        .collect()
}

pub fn parse_scip_file(path: impl AsRef<Path>) -> Result<ParsedScipIndex> {
    let path = path.as_ref();
    let bytes = fs::read(path).with_context(|| format!("read SCIP file {}", path.display()))?;
    parse_scip_bytes(&bytes).with_context(|| format!("parse SCIP file {}", path.display()))
}

pub fn parse_scip_bytes(bytes: &[u8]) -> Result<ParsedScipIndex> {
    let index = Index::parse_from_bytes(bytes).context("decode SCIP protobuf payload")?;
    parse_index(index)
}

fn parse_index(index: Index) -> Result<ParsedScipIndex> {
    let metadata = normalize_metadata(index.metadata.as_ref());
    let mut dropped_locals: usize = 0;
    let files = index
        .documents
        .into_iter()
        .map(|doc| normalize_document(doc, &mut dropped_locals))
        .collect::<Result<Vec<_>>>()?;
    let external_symbols_raw = index
        .external_symbols
        .into_iter()
        .map(normalize_symbol)
        .collect::<Result<Vec<_>>>()?;
    // Drop any external symbols that are SCIP locals. Externals are
    // expected to be cross-package globals, but indexers occasionally
    // leak locals into the external set; filter defensively so the
    // downstream graph builder never sees `symbol:local 0` from any
    // path.
    let external_symbols: Vec<ScipSymbol> = external_symbols_raw
        .into_iter()
        .filter(|sym| {
            if is_local_symbol(&sym.symbol) {
                dropped_locals += 1;
                false
            } else {
                true
            }
        })
        .collect();

    if dropped_locals > 0 {
        tracing::info!(
            target: "djinn_graph::scip_parser",
            dropped_locals,
            "filtered SCIP local symbols from parsed index"
        );
    }

    Ok(ParsedScipIndex {
        metadata,
        files,
        external_symbols,
    })
}

fn normalize_metadata(metadata: Option<&Metadata>) -> ScipMetadata {
    let Some(metadata) = metadata else {
        return ScipMetadata::default();
    };

    ScipMetadata {
        project_root: (!metadata.project_root.is_empty()).then(|| metadata.project_root.clone()),
        tool_name: metadata
            .tool_info
            .as_ref()
            .and_then(|tool| (!tool.name.is_empty()).then(|| tool.name.clone())),
        tool_version: metadata
            .tool_info
            .as_ref()
            .and_then(|tool| (!tool.version.is_empty()).then(|| tool.version.clone())),
    }
}

fn normalize_document(document: Document, dropped_locals: &mut usize) -> Result<ScipFile> {
    if document.relative_path.is_empty() {
        return Err(anyhow!("SCIP document missing relative_path"));
    }

    // scip-go emits some occurrences with an empty range field — e.g.
    // synthetic references to generated code. Skip those rather than failing
    // the entire index parse, matching the tolerance scip-go itself shows.
    //
    // Drop SCIP `local` occurrences here too: they get scoped per-document
    // by the indexer, but our graph keys symbols by raw id, so identical
    // local indices would otherwise collapse across files (`local 0` in
    // dispatcher.go and `local 0` in backfill.go become a single super-node
    // with hundreds of inbound edges). See [`is_local_symbol`].
    let occurrences: Vec<_> = document
        .occurrences
        .into_iter()
        .filter_map(normalize_occurrence)
        .filter(|occurrence| {
            if is_local_symbol(&occurrence.symbol) {
                *dropped_locals += 1;
                false
            } else {
                true
            }
        })
        .collect();

    let definitions = occurrences
        .iter()
        .filter(|occurrence| occurrence.roles.contains(&ScipSymbolRole::Definition))
        .cloned()
        .collect();
    let references = occurrences
        .iter()
        .filter(|occurrence| {
            !occurrence.symbol.is_empty() && !occurrence.roles.contains(&ScipSymbolRole::Definition)
        })
        .cloned()
        .collect();

    // Filter local symbols out of the per-document symbol table. Also strip
    // any relationship whose target is a local — those would otherwise
    // generate dangling edges to a node that never gets created.
    let symbols_raw = document
        .symbols
        .into_iter()
        .map(normalize_symbol)
        .collect::<Result<Vec<_>>>()?;
    let symbols: Vec<ScipSymbol> = symbols_raw
        .into_iter()
        .filter_map(|mut sym| {
            if is_local_symbol(&sym.symbol) {
                *dropped_locals += 1;
                return None;
            }
            sym.relationships
                .retain(|rel| !is_local_symbol(&rel.target_symbol));
            Some(sym)
        })
        .collect();

    Ok(ScipFile {
        language: document.language,
        relative_path: PathBuf::from(document.relative_path),
        definitions,
        references,
        occurrences,
        symbols,
    })
}

fn normalize_occurrence(occurrence: Occurrence) -> Option<ScipOccurrence> {
    // A valid SCIP range is 3 (same-line) or 4 ints. scip-go has been seen
    // emitting 0-length ranges on synthetic occurrences; we drop those rather
    // than abort the whole parse. A malformed enclosing_range is also fatal
    // only for that one occurrence.
    let range = decode_range(&occurrence.range)?;
    let enclosing_range = if occurrence.enclosing_range.is_empty() {
        None
    } else {
        Some(decode_range(&occurrence.enclosing_range)?)
    };
    Some(ScipOccurrence {
        symbol: occurrence.symbol,
        range,
        enclosing_range,
        roles: decode_roles(occurrence.symbol_roles),
        syntax_kind: occurrence
            .syntax_kind
            .enum_value()
            .ok()
            .map(|kind| format!("{kind:?}")),
        override_documentation: occurrence.override_documentation,
    })
}

fn normalize_symbol(symbol: SymbolInformation) -> Result<ScipSymbol> {
    if symbol.symbol.is_empty() {
        return Err(anyhow!("SCIP symbol information missing symbol identifier"));
    }

    let display_name = if symbol.display_name.is_empty() {
        last_descriptor_name(&symbol.symbol)
    } else {
        Some(symbol.display_name.clone())
    };

    let signature = symbol
        .signature_documentation
        .as_ref()
        .and_then(|document| (!document.text.is_empty()).then(|| document.text.clone()));

    // PR C1: SCIP 0.7's `signature_documentation` is a markdown-only
    // `Document` proto (just a `text` field), so there are no
    // structured `parameters`/`return_type`/`type_parameters` to lift
    // here. We deliberately leave `signature_parts` as None — the plan
    // contract forbids regexing the markdown blob to fake structured
    // fields. When a future SCIP version or richer indexer emits a
    // proper `scip::Signature` message, this is the call-site to
    // populate the new fields.
    let signature_parts: Option<ScipSignatureParts> = None;

    let source_symbol = symbol.symbol.clone();
    let relationships = symbol
        .relationships
        .into_iter()
        .map(|relationship| normalize_relationship(&source_symbol, relationship))
        .collect();

    let visibility = ScipVisibility::from_symbol_identifier(&symbol.symbol);

    Ok(ScipSymbol {
        symbol: symbol.symbol,
        kind: map_symbol_kind(symbol.kind.enum_value().ok()),
        display_name,
        signature,
        documentation: symbol.documentation,
        relationships,
        visibility: Some(visibility),
        signature_parts,
    })
}

fn normalize_relationship(source_symbol: &str, relationship: Relationship) -> ScipRelationship {
    let mut kinds = BTreeSet::new();
    if relationship.is_reference {
        kinds.insert(ScipRelationshipKind::Reference);
    }
    if relationship.is_implementation {
        kinds.insert(ScipRelationshipKind::Implementation);
    }
    if relationship.is_type_definition {
        kinds.insert(ScipRelationshipKind::TypeDefinition);
    }
    if relationship.is_definition {
        kinds.insert(ScipRelationshipKind::Definition);
    }

    ScipRelationship {
        source_symbol: source_symbol.to_string(),
        target_symbol: relationship.symbol,
        kinds,
    }
}

fn decode_range(range: &[i32]) -> Option<ScipRange> {
    match range {
        [start_line, start_character, end_character] => Some(ScipRange {
            start_line: *start_line,
            start_character: *start_character,
            end_line: *start_line,
            end_character: *end_character,
        }),
        [start_line, start_character, end_line, end_character] => Some(ScipRange {
            start_line: *start_line,
            start_character: *start_character,
            end_line: *end_line,
            end_character: *end_character,
        }),
        _ => None,
    }
}

fn decode_roles(bitset: i32) -> BTreeSet<ScipSymbolRole> {
    let mut roles = BTreeSet::new();
    for (mask, role) in [
        (1, ScipSymbolRole::Definition),
        (2, ScipSymbolRole::Import),
        (4, ScipSymbolRole::WriteAccess),
        (8, ScipSymbolRole::ReadAccess),
        (16, ScipSymbolRole::Generated),
        (32, ScipSymbolRole::Test),
        (64, ScipSymbolRole::ForwardDefinition),
    ] {
        if bitset & mask != 0 {
            roles.insert(role);
        }
    }
    roles
}

fn last_descriptor_name(symbol: &str) -> Option<String> {
    let parsed = scip::symbol::parse_symbol(symbol).ok()?;
    let descriptors: Vec<Descriptor> = parsed.descriptors;
    descriptors
        .into_iter()
        .rev()
        .find(|descriptor| !descriptor.name.is_empty())
        .map(|descriptor| descriptor.name)
}

/// Best-effort SCIP-descriptor → human-readable label.
///
/// External / cross-package symbols that the parser cannot resolve to a
/// `display_name` flow through to the snapshot wire as raw SCIP descriptors
/// (e.g. `scip-go gomod github.com/golang/go/src . context/Context#`). The
/// UI renders these verbatim, drowning the canvas in 100-character URLs.
///
/// SCIP symbol grammar (best-effort):
/// ```text
/// <scheme> <manager> <package_name> <package_version> <descriptor>
/// ```
/// where `<descriptor>` uses `/` separators and ends with one of:
///
/// | Suffix       | Meaning                                  |
/// |--------------|------------------------------------------|
/// | `#`          | type                                     |
/// | `().`        | method                                   |
/// | `.`          | term / value                             |
/// | `[]` / `[…]` | typeparam                                |
///
/// We extract the trailing identifier of the descriptor (last `/`-separated
/// segment) and strip the suffix. Backticks are removed before splitting so
/// quoted package paths like `` `github.com/google/uuid`/UUID# `` collapse
/// to `UUID`.
///
/// Falls back to the original input on any parse mismatch — better to emit
/// something than nothing. Empty input passes through unchanged.
///
/// Mirrors the UI's `prettifyLabel` (in `ui/src/lib/codeGraphAdapter.ts`).
/// The UI keeps its copy as a defense-in-depth guard; the snapshot wire
/// should already carry pretty labels post-2026-04-28.
pub fn prettify_scip_descriptor(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    // Only engage on what looks like a SCIP descriptor — anything else is
    // likely already a display name and must pass through unchanged.
    if !is_scip_descriptor_prefix(raw) {
        return raw.to_string();
    }
    let stripped: String = raw.chars().filter(|c| *c != '`').collect();
    let tokens: Vec<&str> = stripped.split_whitespace().collect();
    let descriptor = match tokens.last() {
        Some(d) => *d,
        None => return raw.to_string(),
    };
    // Strip trailing suffix marker(s). Method `().` → `()`; type/term/typeparam
    // → bare identifier. Order matters: handle `().` before `.` so we don't
    // accidentally chew off the parens.
    let tail = if let Some(without_method) = descriptor.strip_suffix("().") {
        format!("{without_method}()")
    } else {
        let mut t = descriptor.to_string();
        while let Some(last) = t.chars().last() {
            if matches!(last, '#' | '.' | '[' | ']') {
                t.pop();
            } else {
                break;
            }
        }
        t
    };
    // SCIP descriptors nest with two separators: `/` between path-like
    // namespace segments (`crate/foo/Bar`) and `#` between a parent type
    // and its member (`Bar#baz()`). The visible label is the deepest leaf —
    // walk past both.
    let segments: Vec<&str> = tail
        .split(|c| c == '/' || c == '#')
        .filter(|s| !s.is_empty())
        .collect();
    match segments.last() {
        Some(seg) if !seg.is_empty() => seg.to_string(),
        _ => raw.to_string(),
    }
}

/// True when `raw` starts with a SCIP scheme token (`scip-` followed by one
/// or more identifier chars and a space). Pure-display names like
/// `internal/repository/jobs.go` slip through unchanged.
fn is_scip_descriptor_prefix(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    if !raw.starts_with("scip-") || bytes.len() < 6 {
        return false;
    }
    let mut saw_word_char = false;
    for (i, c) in raw.chars().enumerate().skip(5) {
        if c == ' ' {
            return saw_word_char && i > 5;
        }
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            saw_word_char = true;
            continue;
        }
        return false;
    }
    false
}

fn map_symbol_kind(kind: Option<symbol_information::Kind>) -> Option<ScipSymbolKind> {
    Some(match kind? {
        symbol_information::Kind::Package => ScipSymbolKind::Package,
        symbol_information::Kind::Namespace => ScipSymbolKind::Namespace,
        symbol_information::Kind::Type
        | symbol_information::Kind::Class
        | symbol_information::Kind::Trait
        | symbol_information::Kind::Protocol
        | symbol_information::Kind::Struct => ScipSymbolKind::Type,
        symbol_information::Kind::Method
        | symbol_information::Kind::AbstractMethod
        | symbol_information::Kind::StaticMethod
        | symbol_information::Kind::ProtocolMethod
        | symbol_information::Kind::TraitMethod
        | symbol_information::Kind::Constructor => ScipSymbolKind::Method,
        symbol_information::Kind::Property | symbol_information::Kind::StaticProperty => {
            ScipSymbolKind::Property
        }
        symbol_information::Kind::Field | symbol_information::Kind::StaticField => {
            ScipSymbolKind::Field
        }
        symbol_information::Kind::Enum => ScipSymbolKind::Enum,
        symbol_information::Kind::Interface => ScipSymbolKind::Interface,
        symbol_information::Kind::Function => ScipSymbolKind::Function,
        symbol_information::Kind::Variable | symbol_information::Kind::StaticVariable => {
            ScipSymbolKind::Variable
        }
        symbol_information::Kind::Constant => ScipSymbolKind::Constant,
        symbol_information::Kind::String => ScipSymbolKind::String,
        symbol_information::Kind::Number => ScipSymbolKind::Number,
        symbol_information::Kind::Boolean => ScipSymbolKind::Boolean,
        symbol_information::Kind::Array => ScipSymbolKind::Array,
        symbol_information::Kind::Object => ScipSymbolKind::Object,
        symbol_information::Kind::Key => ScipSymbolKind::Key,
        symbol_information::Kind::Null => ScipSymbolKind::Null,
        symbol_information::Kind::EnumMember => ScipSymbolKind::EnumMember,
        symbol_information::Kind::Event | symbol_information::Kind::StaticEvent => {
            ScipSymbolKind::Event
        }
        symbol_information::Kind::Operator => ScipSymbolKind::Operator,
        other => ScipSymbolKind::Unknown(other.value()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::{EnumOrUnknown, MessageField, SpecialFields};
    use scip::types::{ToolInfo, symbol_information};

    fn fixture_index_bytes() -> Vec<u8> {
        let mut index = Index::new();
        index.metadata = MessageField::some(Metadata {
            project_root: "file:///workspace/repo".to_string(),
            tool_info: MessageField::some(ToolInfo {
                name: "rust-analyzer".to_string(),
                version: "1.0.0".to_string(),
                arguments: vec![],
                special_fields: SpecialFields::new(),
            }),
            ..Metadata::new()
        });

        let mut document = Document::new();
        document.language = "rust".to_string();
        document.relative_path = "src/lib.rs".to_string();
        document.occurrences = vec![
            Occurrence {
                range: vec![1, 4, 10],
                symbol: "scip-rust . . . foo#".to_string(),
                symbol_roles: 1,
                ..Occurrence::new()
            },
            Occurrence {
                range: vec![3, 8, 3, 11],
                symbol: "scip-rust . . . bar().".to_string(),
                symbol_roles: 8,
                enclosing_range: vec![3, 0, 3, 20],
                ..Occurrence::new()
            },
        ];
        document.symbols = vec![SymbolInformation {
            symbol: "scip-rust . . . foo#".to_string(),
            display_name: "Foo".to_string(),
            documentation: vec!["docs".to_string()],
            relationships: vec![Relationship {
                symbol: "scip-rust . . . Trait#".to_string(),
                is_implementation: true,
                is_reference: true,
                ..Relationship::new()
            }],
            kind: EnumOrUnknown::new(symbol_information::Kind::Class),
            signature_documentation: MessageField::some(Document {
                language: "rust".to_string(),
                text: "struct Foo".to_string(),
                ..Document::new()
            }),
            ..SymbolInformation::new()
        }];
        index.documents.push(document);
        index.external_symbols.push(SymbolInformation {
            symbol: "scip-rust cargo deps 1.0 external#".to_string(),
            display_name: "External".to_string(),
            documentation: vec!["external docs".to_string()],
            kind: EnumOrUnknown::new(symbol_information::Kind::Class),
            ..SymbolInformation::new()
        });

        index.write_to_bytes().expect("encode fixture index")
    }

    #[test]
    fn parses_definitions_references_and_relationships() {
        let parsed = parse_scip_bytes(&fixture_index_bytes()).expect("parse fixture index");

        assert_eq!(
            parsed.metadata.project_root.as_deref(),
            Some("file:///workspace/repo")
        );
        assert_eq!(parsed.files.len(), 1);
        let file = &parsed.files[0];
        assert_eq!(file.relative_path, PathBuf::from("src/lib.rs"));
        assert_eq!(file.definitions.len(), 1);
        assert_eq!(file.references.len(), 1);
        assert_eq!(file.definitions[0].range.end_line, 1);
        assert!(
            file.definitions[0]
                .roles
                .contains(&ScipSymbolRole::Definition)
        );
        assert!(
            file.references[0]
                .roles
                .contains(&ScipSymbolRole::ReadAccess)
        );
        assert_eq!(
            file.references[0]
                .enclosing_range
                .as_ref()
                .unwrap()
                .start_character,
            0
        );

        let symbol = &file.symbols[0];
        assert_eq!(symbol.display_name.as_deref(), Some("Foo"));
        assert_eq!(symbol.signature.as_deref(), Some("struct Foo"));
        assert_eq!(symbol.kind, Some(ScipSymbolKind::Type));
        assert_eq!(symbol.relationships.len(), 1);
        assert!(
            symbol.relationships[0]
                .kinds
                .contains(&ScipRelationshipKind::Implementation)
        );
        assert!(
            symbol.relationships[0]
                .kinds
                .contains(&ScipRelationshipKind::Reference)
        );
        assert_eq!(parsed.external_symbols.len(), 1);
    }

    #[test]
    fn malformed_payload_returns_error() {
        let error = parse_scip_bytes(b"not-a-protobuf").expect_err("expected decode failure");
        assert!(error.to_string().contains("decode SCIP protobuf payload"));
    }

    #[test]
    fn partial_document_data_skips_malformed_occurrences() {
        // scip-go is known to emit occurrences with empty / single-element
        // ranges on synthetic references. Skip those occurrences rather than
        // abort the whole parse — otherwise one bad occurrence kills indexing
        // for the entire project. The document itself still parses, just
        // without the malformed occurrence.
        let mut index = Index::new();
        index.documents.push(Document {
            language: "rust".to_string(),
            relative_path: "src/lib.rs".to_string(),
            occurrences: vec![Occurrence {
                range: vec![7],
                symbol: "scip-rust . . . broken#".to_string(),
                ..Occurrence::new()
            }],
            ..Document::new()
        });

        let bytes = index.write_to_bytes().expect("encode index");
        let parsed = parse_scip_bytes(&bytes).expect("parse should succeed");
        assert_eq!(parsed.files.len(), 1);
        assert!(
            parsed.files[0].occurrences.is_empty(),
            "malformed occurrence should be dropped, got {:?}",
            parsed.files[0].occurrences
        );
    }

    #[test]
    fn parses_multiple_artifacts() {
        let dir = PathBuf::from("tmp/scip-parser-tests");
        let _ = fs::create_dir_all(&dir);
        let first = dir.join("one.scip");
        let second = dir.join("two.scip");
        fs::write(&first, fixture_index_bytes()).expect("write fixture one");
        fs::write(&second, fixture_index_bytes()).expect("write fixture two");

        let parsed = parse_scip_artifacts(&[
            ScipArtifact {
                path: first.clone(),
                indexer: None,
            },
            ScipArtifact {
                path: second.clone(),
                indexer: None,
            },
        ])
        .expect("parse artifacts");

        assert_eq!(parsed.len(), 2);
        let _ = fs::remove_file(first);
        let _ = fs::remove_file(second);
    }

    /// Synthetic SCIP index that mixes a global symbol with several
    /// per-document `local …` entries. The parser must drop every
    /// `local` symbol from `symbols`, `definitions`, `references`,
    /// `occurrences`, and `external_symbols`, AND must strip
    /// `local`-target relationships off the surviving global so the
    /// graph builder never sees a dangling local edge.
    #[test]
    fn parse_index_filters_scip_local_symbols() {
        let mut index = Index::new();
        let mut document = Document::new();
        document.language = "go".to_string();
        document.relative_path = "internal/dispatcher.go".to_string();
        document.occurrences = vec![
            // Global definition — must survive.
            Occurrence {
                range: vec![1, 0, 10],
                symbol: "scip-go gomod example.com/svc . dispatcher/Run().".to_string(),
                symbol_roles: 1,
                ..Occurrence::new()
            },
            // Local definition — must be filtered.
            Occurrence {
                range: vec![2, 4, 8],
                symbol: "local 0".to_string(),
                symbol_roles: 1,
                ..Occurrence::new()
            },
            // Local read — must be filtered.
            Occurrence {
                range: vec![3, 8, 11],
                symbol: "local 0".to_string(),
                symbol_roles: 8,
                ..Occurrence::new()
            },
            // Reference to a global — must survive.
            Occurrence {
                range: vec![4, 4, 9],
                symbol: "scip-go gomod example.com/svc . dispatcher/helper().".to_string(),
                symbol_roles: 8,
                ..Occurrence::new()
            },
        ];
        document.symbols = vec![
            // Global symbol with relationships — keep, but drop the
            // local-targeted relationship.
            SymbolInformation {
                symbol: "scip-go gomod example.com/svc . dispatcher/Run().".to_string(),
                display_name: "Run".to_string(),
                relationships: vec![
                    Relationship {
                        symbol: "local 5".to_string(),
                        is_reference: true,
                        ..Relationship::new()
                    },
                    Relationship {
                        symbol: "scip-go gomod example.com/svc . dispatcher/helper()."
                            .to_string(),
                        is_reference: true,
                        ..Relationship::new()
                    },
                ],
                kind: EnumOrUnknown::new(symbol_information::Kind::Function),
                ..SymbolInformation::new()
            },
            // Local — must be filtered out of the symbol table.
            SymbolInformation {
                symbol: "local 0".to_string(),
                display_name: "cmd".to_string(),
                kind: EnumOrUnknown::new(symbol_information::Kind::Variable),
                ..SymbolInformation::new()
            },
        ];
        index.documents.push(document);
        // Defensively check the external-symbol filter too: an indexer
        // that leaks a `local` into the external set must still be
        // sanitized.
        index.external_symbols.push(SymbolInformation {
            symbol: "local 999".to_string(),
            display_name: "ctx".to_string(),
            kind: EnumOrUnknown::new(symbol_information::Kind::Variable),
            ..SymbolInformation::new()
        });
        index.external_symbols.push(SymbolInformation {
            symbol: "scip-go gomod example.com/lib v1.0 lib/Helper#".to_string(),
            display_name: "Helper".to_string(),
            kind: EnumOrUnknown::new(symbol_information::Kind::Class),
            ..SymbolInformation::new()
        });

        let bytes = index.write_to_bytes().expect("encode fixture index");
        let parsed = parse_scip_bytes(&bytes).expect("parse synthetic index");

        assert_eq!(parsed.files.len(), 1);
        let file = &parsed.files[0];

        // No local occurrences anywhere in the file.
        for bucket_name in ["occurrences", "definitions", "references"] {
            let bucket: &[ScipOccurrence] = match bucket_name {
                "occurrences" => &file.occurrences,
                "definitions" => &file.definitions,
                "references" => &file.references,
                _ => unreachable!(),
            };
            assert!(
                bucket.iter().all(|o| !is_local_symbol(&o.symbol)),
                "{bucket_name} bucket leaked a local: {:?}",
                bucket
            );
        }

        // Symbols must contain only the global `Run` (the local was filtered).
        assert_eq!(file.symbols.len(), 1, "expected only the global symbol");
        assert_eq!(file.symbols[0].symbol, "scip-go gomod example.com/svc . dispatcher/Run().");
        // The local-targeted relationship was stripped — only the global one
        // survives.
        assert_eq!(
            file.symbols[0].relationships.len(),
            1,
            "expected exactly one relationship after local-target stripping"
        );
        assert_eq!(
            file.symbols[0].relationships[0].target_symbol,
            "scip-go gomod example.com/svc . dispatcher/helper()."
        );

        // External symbols must contain only the global `Helper`.
        assert_eq!(parsed.external_symbols.len(), 1);
        assert_eq!(parsed.external_symbols[0].display_name.as_deref(), Some("Helper"));
    }

    // ── prettify_scip_descriptor (Fix 2) ───────────────────────────────

    #[test]
    fn prettify_strips_type_descriptor_to_trailing_identifier() {
        assert_eq!(
            prettify_scip_descriptor("scip-go gomod github.com/golang/go/src . context/Context#"),
            "Context"
        );
    }

    #[test]
    fn prettify_strips_method_descriptor_keeping_parens() {
        assert_eq!(
            prettify_scip_descriptor("scip-go gomod github.com/golang/go/src . fmt/Errorf()."),
            "Errorf()"
        );
    }

    #[test]
    fn prettify_handles_backticked_package_paths() {
        assert_eq!(
            prettify_scip_descriptor(
                "scip-go gomod github.com/google/uuid v1.6.0 `github.com/google/uuid`/UUID#"
            ),
            "UUID"
        );
    }

    #[test]
    fn prettify_strips_term_descriptor_to_trailing_identifier() {
        assert_eq!(
            prettify_scip_descriptor("scip-rust . . . crate/foo/Bar#baz()."),
            "baz()"
        );
    }

    #[test]
    fn prettify_passes_through_non_scip_inputs() {
        assert_eq!(prettify_scip_descriptor(""), "");
        // Plain identifiers and file paths must round-trip unchanged.
        assert_eq!(prettify_scip_descriptor("Client"), "Client");
        assert_eq!(
            prettify_scip_descriptor("internal/repository/jobs.go"),
            "internal/repository/jobs.go"
        );
    }
}
