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

use crate::repo_map::ScipArtifact;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScipSymbol {
    pub symbol: String,
    pub kind: Option<ScipSymbolKind>,
    pub display_name: Option<String>,
    pub signature: Option<String>,
    pub documentation: Vec<String>,
    pub relationships: Vec<ScipRelationship>,
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
    let files = index
        .documents
        .into_iter()
        .map(normalize_document)
        .collect::<Result<Vec<_>>>()?;
    let external_symbols = index
        .external_symbols
        .into_iter()
        .map(normalize_symbol)
        .collect::<Result<Vec<_>>>()?;

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

fn normalize_document(document: Document) -> Result<ScipFile> {
    if document.relative_path.is_empty() {
        return Err(anyhow!("SCIP document missing relative_path"));
    }

    let occurrences = document
        .occurrences
        .into_iter()
        .map(normalize_occurrence)
        .collect::<Result<Vec<_>>>()?;

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

    let symbols = document
        .symbols
        .into_iter()
        .map(normalize_symbol)
        .collect::<Result<Vec<_>>>()?;

    Ok(ScipFile {
        language: document.language,
        relative_path: PathBuf::from(document.relative_path),
        definitions,
        references,
        occurrences,
        symbols,
    })
}

fn normalize_occurrence(occurrence: Occurrence) -> Result<ScipOccurrence> {
    Ok(ScipOccurrence {
        symbol: occurrence.symbol,
        range: decode_range(&occurrence.range).ok_or_else(|| {
            anyhow!(
                "SCIP occurrence has malformed range: {:?}",
                occurrence.range
            )
        })?,
        enclosing_range: if occurrence.enclosing_range.is_empty() {
            None
        } else {
            Some(decode_range(&occurrence.enclosing_range).ok_or_else(|| {
                anyhow!(
                    "SCIP occurrence has malformed enclosing_range: {:?}",
                    occurrence.enclosing_range
                )
            })?)
        },
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

    let source_symbol = symbol.symbol.clone();
    let relationships = symbol
        .relationships
        .into_iter()
        .map(|relationship| normalize_relationship(&source_symbol, relationship))
        .collect();

    Ok(ScipSymbol {
        symbol: symbol.symbol,
        kind: map_symbol_kind(symbol.kind.enum_value().ok()),
        display_name,
        signature,
        documentation: symbol.documentation,
        relationships,
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
    fn partial_document_data_fails_gracefully() {
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
        let error = parse_scip_bytes(&bytes).expect_err("expected range error");
        assert!(error.to_string().contains("malformed range"));
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
}
