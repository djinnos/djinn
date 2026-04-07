use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Clone, Default)]
pub struct SymbolQuery {
    pub depth: Option<usize>,
    pub kinds: Option<HashSet<u64>>,
    pub name_filter: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StablePosition {
    pub(crate) line: u32,
    pub(crate) character: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SymbolEntry {
    pub(crate) kind: u64,
    pub(crate) kind_name: &'static str,
    pub(crate) name: String,
    pub(crate) name_path: String,
    pub(crate) depth: usize,
    pub(crate) line: Option<u64>,
    pub(crate) location: Option<String>,
    pub(crate) child_count: usize,
    pub(crate) stable_position: Option<StablePosition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SymbolLookupQuery {
    raw: String,
    suffix_segments: Vec<String>,
    kind_hint: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedSymbol {
    pub(crate) name_path: String,
    pub(crate) kind_name: &'static str,
    pub(crate) location: String,
    pub(crate) position: StablePosition,
}

pub(crate) fn parse_symbol_tree(
    symbols: &[serde_json::Value],
    query: &SymbolQuery,
) -> Vec<SymbolEntry> {
    let normalized_name_filter = query.name_filter.as_ref().map(|value| value.to_lowercase());
    let mut entries = Vec::new();
    for symbol in symbols {
        collect_symbol_entries(
            symbol,
            0,
            &mut Vec::new(),
            &normalized_name_filter,
            &mut entries,
        );
    }
    filter_symbol_entries(entries, query)
}

pub(crate) fn collect_symbol_entries(
    symbol: &serde_json::Value,
    depth: usize,
    parent_path: &mut Vec<String>,
    normalized_name_filter: &Option<String>,
    entries: &mut Vec<SymbolEntry>,
) {
    let name = symbol
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or("?")
        .to_string();
    let kind = symbol
        .get("kind")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);

    parent_path.push(name.clone());
    let name_path = parent_path.join("/");

    let matches_name = normalized_name_filter.as_ref().is_none_or(|needle| {
        let lowered_name = name.to_lowercase();
        let lowered_path = name_path.to_lowercase();
        lowered_name.contains(needle) || lowered_path.contains(needle)
    });

    if matches_name {
        let line = symbol
            .get("range")
            .and_then(|range| range.get("start"))
            .and_then(|start| start.get("line"))
            .and_then(|line| line.as_u64())
            .map(|line| line + 1);
        let location = symbol_location(symbol);
        let child_count = symbol
            .get("children")
            .and_then(|value| value.as_array())
            .map(Vec::len)
            .unwrap_or(0);
        entries.push(SymbolEntry {
            kind,
            kind_name: symbol_kind_name(kind),
            name,
            name_path,
            depth,
            line,
            location,
            child_count,
            stable_position: symbol_stable_position(symbol),
        });
    }

    if let Some(children) = symbol.get("children").and_then(|value| value.as_array()) {
        for child in children {
            collect_symbol_entries(
                child,
                depth + 1,
                parent_path,
                normalized_name_filter,
                entries,
            );
        }
    }

    parent_path.pop();
}

pub(crate) fn filter_symbol_entries(
    entries: Vec<SymbolEntry>,
    query: &SymbolQuery,
) -> Vec<SymbolEntry> {
    entries
        .into_iter()
        .filter(|entry| query.depth.is_none_or(|max_depth| entry.depth <= max_depth))
        .filter(|entry| {
            query
                .kinds
                .as_ref()
                .is_none_or(|kinds| kinds.contains(&entry.kind))
        })
        .collect()
}

fn symbol_location(symbol: &serde_json::Value) -> Option<String> {
    symbol.get("location").map(format_location).or_else(|| {
        symbol_stable_position(symbol).map(|position| {
            if position.character == 0 {
                format!("line {}", position.line + 1)
            } else {
                format!("line {}:{}", position.line + 1, position.character + 1)
            }
        })
    })
}

fn symbol_stable_position(symbol: &serde_json::Value) -> Option<StablePosition> {
    let selection = symbol
        .get("selectionRange")
        .and_then(|value| value.as_object())
        .or_else(|| symbol.get("range").and_then(|value| value.as_object()))?;
    let start = selection.get("start")?.as_object()?;
    Some(StablePosition {
        line: start.get("line")?.as_u64()? as u32,
        character: start.get("character")?.as_u64()? as u32,
    })
}

pub(crate) fn parse_symbol_lookup_query(query: &str) -> Result<SymbolLookupQuery, String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err("symbol query must not be empty".to_string());
    }

    let (kind_hint, name_query) = if let Some((raw_kind, raw_name)) = trimmed.split_once(':') {
        if raw_kind.contains('/') || raw_name.trim().is_empty() {
            (None, trimmed)
        } else {
            (
                Some(parse_single_symbol_kind(raw_kind.trim())?),
                raw_name.trim(),
            )
        }
    } else {
        (None, trimmed)
    };

    let suffix_segments: Vec<String> = name_query
        .split('/')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if suffix_segments.is_empty() {
        return Err("symbol query must not be empty".to_string());
    }

    Ok(SymbolLookupQuery {
        raw: trimmed.to_string(),
        suffix_segments,
        kind_hint,
    })
}

pub(crate) fn resolve_symbol_entries(
    entries: &[SymbolEntry],
    query: &SymbolLookupQuery,
) -> Result<ResolvedSymbol, String> {
    let mut matches: Vec<&SymbolEntry> = entries
        .iter()
        .filter(|entry| query.kind_hint.is_none_or(|kind| entry.kind == kind))
        .filter(|entry| name_path_has_suffix(&entry.name_path, &query.suffix_segments))
        .collect();

    matches.sort_by(|a, b| {
        a.name_path
            .cmp(&b.name_path)
            .then(a.location.cmp(&b.location))
            .then(a.kind.cmp(&b.kind))
    });

    if matches.len() == 1 {
        let entry = matches[0];
        let position = entry.stable_position.clone().ok_or_else(|| {
            format!(
                "Symbol `{}` was found at {} but does not expose a stable position.",
                entry.name_path,
                entry.location.as_deref().unwrap_or("unknown location")
            )
        })?;
        return Ok(ResolvedSymbol {
            name_path: entry.name_path.clone(),
            kind_name: entry.kind_name,
            location: entry.location.clone().unwrap_or_else(|| {
                format!("line {}:{}", position.line + 1, position.character + 1)
            }),
            position,
        });
    }

    if matches.is_empty() {
        let needle = query
            .suffix_segments
            .last()
            .map(|segment| segment.to_lowercase())
            .unwrap_or_default();
        let mut suggestions: Vec<&SymbolEntry> = entries
            .iter()
            .filter(|entry| query.kind_hint.is_none_or(|kind| entry.kind == kind))
            .filter(|entry| {
                entry.name.to_lowercase().contains(&needle)
                    || entry.name_path.to_lowercase().contains(&needle)
            })
            .collect();
        suggestions.sort_by(|a, b| {
            a.name_path
                .cmp(&b.name_path)
                .then(a.location.cmp(&b.location))
        });
        suggestions.truncate(5);

        let mut message = format!(
            "No symbol found matching `{}`. Use `lsp symbols` to inspect available name paths.",
            query.raw
        );
        if !suggestions.is_empty() {
            let rendered = suggestions
                .into_iter()
                .map(|entry| {
                    format!(
                        "- {} ({}, {})",
                        entry.name_path,
                        entry.kind_name,
                        entry.location.as_deref().unwrap_or("unknown location")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            message.push_str("\nClosest matches:\n");
            message.push_str(&rendered);
        }
        return Err(message);
    }

    let rendered = matches
        .into_iter()
        .map(|entry| {
            format!(
                "- {} ({}, {})",
                entry.name_path,
                entry.kind_name,
                entry.location.as_deref().unwrap_or("unknown location")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Err(format!(
        "Symbol query `{}` is ambiguous. Matching candidates:\n{}",
        query.raw, rendered
    ))
}

fn name_path_has_suffix(name_path: &str, suffix_segments: &[String]) -> bool {
    let name_segments: Vec<&str> = name_path.split('/').collect();
    let suffix: Vec<&str> = suffix_segments.iter().map(String::as_str).collect();
    name_segments.ends_with(&suffix)
}

pub(crate) fn format_symbol_entries(entries: &[SymbolEntry]) -> String {
    if entries.is_empty() {
        return "No symbols found in this document.".to_string();
    }

    let full = render_grouped_symbols(entries, SymbolRenderMode::Full);
    if full.len() <= 8_000 {
        return full;
    }

    let compact = render_grouped_symbols(entries, SymbolRenderMode::ChildCounts);
    if compact.len() <= 8_000 {
        return format!("{compact}\n\n(output shortened: children collapsed to counts)");
    }

    format!(
        "{}\n\n(output shortened: showing kind counts only)",
        render_kind_counts(entries)
    )
}

#[derive(Clone, Copy)]
enum SymbolRenderMode {
    Full,
    ChildCounts,
}

fn render_grouped_symbols(entries: &[SymbolEntry], mode: SymbolRenderMode) -> String {
    let mut groups: BTreeMap<&'static str, Vec<&SymbolEntry>> = BTreeMap::new();
    for entry in entries {
        groups.entry(entry.kind_name).or_default().push(entry);
    }

    let mut sections = Vec::new();
    for (kind, mut group_entries) in groups {
        group_entries.sort_by(|a, b| a.name_path.cmp(&b.name_path).then(a.line.cmp(&b.line)));
        let mut lines = vec![format!("{kind} ({})", group_entries.len())];
        for entry in group_entries {
            let location = entry
                .location
                .clone()
                .or_else(|| entry.line.map(|line| format!("line {line}")))
                .unwrap_or_else(|| "line ?".to_string());
            let suffix = match mode {
                SymbolRenderMode::Full => String::new(),
                SymbolRenderMode::ChildCounts if entry.child_count > 0 => {
                    format!(" [children: {}]", entry.child_count)
                }
                SymbolRenderMode::ChildCounts => String::new(),
            };
            lines.push(format!("- {} ({location}){suffix}", entry.name_path));
        }
        sections.push(lines.join("\n"));
    }

    sections.join("\n\n")
}

fn render_kind_counts(entries: &[SymbolEntry]) -> String {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for entry in entries {
        *counts.entry(entry.kind_name).or_default() += 1;
    }

    let mut lines = vec!["Symbol kinds".to_string()];
    for (kind, count) in counts {
        lines.push(format!("- {kind}: {count}"));
    }
    lines.join("\n")
}

pub fn parse_symbol_kind_filter(value: &str) -> Result<HashSet<u64>, String> {
    let mut kinds = HashSet::new();
    for raw_kind in value.split(',') {
        let normalized = raw_kind.trim().to_lowercase();
        if normalized.is_empty() {
            continue;
        }
        let kind_num = parse_single_symbol_kind(&normalized)?;
        kinds.insert(kind_num);
    }

    if kinds.is_empty() {
        return Err("symbol kind filter must not be empty".to_string());
    }

    Ok(kinds)
}

pub(crate) fn parse_single_symbol_kind(value: &str) -> Result<u64, String> {
    match value.trim().to_lowercase().as_str() {
        "file" => Ok(1),
        "module" => Ok(2),
        "namespace" => Ok(3),
        "package" => Ok(4),
        "class" => Ok(5),
        "method" => Ok(6),
        "property" => Ok(7),
        "field" => Ok(8),
        "constructor" => Ok(9),
        "enum" => Ok(10),
        "interface" => Ok(11),
        "function" | "fn" => Ok(12),
        "variable" | "var" => Ok(13),
        "constant" | "const" => Ok(14),
        "string" => Ok(15),
        "number" => Ok(16),
        "boolean" | "bool" => Ok(17),
        "array" => Ok(18),
        "object" => Ok(19),
        "key" => Ok(20),
        "null" => Ok(21),
        "enummember" | "enum_member" | "enum-member" => Ok(22),
        "struct" => Ok(23),
        "event" => Ok(24),
        "operator" => Ok(25),
        "typeparameter" | "type_parameter" | "type-parameter" => Ok(26),
        other => Err(format!("unknown symbol kind filter: {other}")),
    }
}

pub(crate) fn symbol_kind_name(kind: u64) -> &'static str {
    match kind {
        1 => "File",
        2 => "Module",
        3 => "Namespace",
        4 => "Package",
        5 => "Class",
        6 => "Method",
        7 => "Property",
        8 => "Field",
        9 => "Constructor",
        10 => "Enum",
        11 => "Interface",
        12 => "Function",
        13 => "Variable",
        14 => "Constant",
        15 => "String",
        16 => "Number",
        17 => "Boolean",
        18 => "Array",
        19 => "Object",
        20 => "Key",
        21 => "Null",
        22 => "EnumMember",
        23 => "Struct",
        24 => "Event",
        25 => "Operator",
        26 => "TypeParameter",
        _ => "Unknown",
    }
}

pub(crate) fn format_location(loc: &serde_json::Value) -> String {
    let uri = loc
        .get("uri")
        .or_else(|| loc.get("targetUri"))
        .and_then(|u| u.as_str())
        .unwrap_or("?");
    let range = loc.get("range").or_else(|| loc.get("targetSelectionRange"));
    let (line, character) = match range {
        Some(r) => {
            let start = r.get("start").unwrap_or(r);
            let l = start.get("line").and_then(|v| v.as_u64()).unwrap_or(0) + 1;
            let c = start.get("character").and_then(|v| v.as_u64()).unwrap_or(0) + 1;
            (l, c)
        }
        None => (1, 1),
    };
    let file = uri.strip_prefix("file://").unwrap_or(uri);
    format!("{file}:{line}:{character}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn symbol_kind_names() {
        assert_eq!(symbol_kind_name(5), "Class");
        assert_eq!(symbol_kind_name(12), "Function");
        assert_eq!(symbol_kind_name(23), "Struct");
        assert_eq!(symbol_kind_name(99), "Unknown");
    }

    #[test]
    fn format_location_with_uri_and_range() {
        let loc = json!({
            "uri": "file:///foo/bar.rs",
            "range": {
                "start": { "line": 9, "character": 4 },
                "end": { "line": 9, "character": 10 }
            }
        });
        assert_eq!(format_location(&loc), "/foo/bar.rs:10:5");
    }

    #[test]
    fn format_location_with_target_uri() {
        let loc = json!({
            "targetUri": "file:///foo/bar.rs",
            "targetSelectionRange": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 5 }
            }
        });
        assert_eq!(format_location(&loc), "/foo/bar.rs:1:1");
    }

    fn sample_symbols() -> Vec<serde_json::Value> {
        vec![
            json!({
                "name": "Config",
                "kind": 23,
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 10, "character": 1 }
                },
                "selectionRange": {
                    "start": { "line": 0, "character": 7 },
                    "end": { "line": 0, "character": 13 }
                },
                "children": [
                    {
                        "name": "rank",
                        "kind": 8,
                        "range": {
                            "start": { "line": 1, "character": 4 },
                            "end": { "line": 1, "character": 14 }
                        },
                        "selectionRange": {
                            "start": { "line": 1, "character": 8 },
                            "end": { "line": 1, "character": 12 }
                        }
                    }
                ]
            }),
            json!({
                "name": "helpers",
                "kind": 2,
                "range": {
                    "start": { "line": 20, "character": 0 },
                    "end": { "line": 40, "character": 1 }
                },
                "selectionRange": {
                    "start": { "line": 20, "character": 4 },
                    "end": { "line": 20, "character": 11 }
                },
                "children": [
                    {
                        "name": "rank",
                        "kind": 12,
                        "range": {
                            "start": { "line": 22, "character": 0 },
                            "end": { "line": 24, "character": 1 }
                        },
                        "selectionRange": {
                            "start": { "line": 22, "character": 3 },
                            "end": { "line": 22, "character": 7 }
                        }
                    }
                ]
            }),
            json!({
                "name": "rank",
                "kind": 12,
                "location": {
                    "uri": "file:///tmp/example.rs",
                    "range": {
                        "start": { "line": 50, "character": 0 },
                        "end": { "line": 52, "character": 1 }
                    }
                },
                "range": {
                    "start": { "line": 50, "character": 0 },
                    "end": { "line": 52, "character": 1 }
                },
                "selectionRange": {
                    "start": { "line": 50, "character": 3 },
                    "end": { "line": 50, "character": 7 }
                }
            }),
        ]
    }

    #[test]
    fn resolves_unique_symbol_by_suffix() {
        let entries = parse_symbol_tree(&sample_symbols(), &SymbolQuery::default());
        let query = parse_symbol_lookup_query("helpers/rank").unwrap();

        let resolved = resolve_symbol_entries(&entries, &query).unwrap();

        assert_eq!(resolved.name_path, "helpers/rank");
        assert_eq!(resolved.kind_name, "Function");
        assert_eq!(
            resolved.position,
            StablePosition {
                line: 22,
                character: 3
            }
        );
    }

    #[test]
    fn resolves_kind_hint_to_narrow_matches() {
        let entries = parse_symbol_tree(&sample_symbols(), &SymbolQuery::default());
        let query = parse_symbol_lookup_query("struct:Config").unwrap();

        let resolved = resolve_symbol_entries(&entries, &query).unwrap();

        assert_eq!(resolved.name_path, "Config");
        assert_eq!(resolved.kind_name, "Struct");
        assert_eq!(
            resolved.position,
            StablePosition {
                line: 0,
                character: 7
            }
        );
    }

    #[test]
    fn ambiguous_symbol_lists_candidates_deterministically() {
        let entries = parse_symbol_tree(&sample_symbols(), &SymbolQuery::default());
        let query = parse_symbol_lookup_query("rank").unwrap();

        let error = resolve_symbol_entries(&entries, &query).unwrap_err();

        assert!(error.contains("Symbol query `rank` is ambiguous."));
        assert!(error.contains("- Config/rank (Field, line 2:9)"));
        assert!(error.contains("- helpers/rank (Function, line 23:4)"));
        assert!(error.contains("- rank (Function, /tmp/example.rs:51:1)"));
    }

    #[test]
    fn missing_symbol_suggests_close_matches() {
        let entries = parse_symbol_tree(&sample_symbols(), &SymbolQuery::default());
        let query = parse_symbol_lookup_query("ran").unwrap();

        let error = resolve_symbol_entries(&entries, &query).unwrap_err();

        assert!(error.contains("No symbol found matching `ran`."));
        assert!(error.contains("Use `lsp symbols` to inspect available name paths."));
        assert!(error.contains("Closest matches:"));
        assert!(error.contains("- Config/rank (Field, line 2:9)"));
    }

    #[test]
    fn parse_symbol_kind_filter_supports_aliases() {
        let kinds = parse_symbol_kind_filter("function,method,type_parameter").unwrap();
        assert!(kinds.contains(&12));
        assert!(kinds.contains(&6));
        assert!(kinds.contains(&26));
    }

    #[test]
    fn parse_symbol_tree_filters_and_formats_grouped_output() {
        let symbols = vec![json!({
            "name": "Config",
            "kind": 23,
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 10, "character": 0 } },
            "children": [
                {
                    "name": "new",
                    "kind": 12,
                    "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 3 } }
                },
                {
                    "name": "value",
                    "kind": 8,
                    "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 2, "character": 5 } }
                }
            ]
        })];

        let entries = parse_symbol_tree(
            &symbols,
            &SymbolQuery {
                depth: Some(1),
                kinds: Some(HashSet::from([23])),
                name_filter: Some("conf".to_string()),
            },
        );

        let output = format_symbol_entries(&entries);
        assert!(output.contains("Struct (1)"));
        assert!(output.contains("- Config (line 1)"));
        assert!(!output.contains("Field"));
        assert!(!output.contains("new"));
    }

    #[test]
    fn parse_symbol_tree_matches_nested_name_paths() {
        let symbols = vec![json!({
            "name": "Outer",
            "kind": 5,
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 5, "character": 0 } },
            "children": [
                {
                    "name": "target_method",
                    "kind": 6,
                    "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 5 } }
                }
            ]
        })];

        let entries = parse_symbol_tree(
            &symbols,
            &SymbolQuery {
                depth: None,
                kinds: Some(HashSet::from([6])),
                name_filter: Some("outer/target".to_string()),
            },
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name_path, "Outer/target_method");
    }

    #[test]
    fn format_symbol_entries_falls_back_for_large_outputs() {
        let entries: Vec<SymbolEntry> = (0..250)
            .map(|index| SymbolEntry {
                kind: 12,
                kind_name: "Function",
                name: format!("very_long_symbol_name_{index:03}_{}", "x".repeat(40)),
                name_path: format!("module/very_long_symbol_name_{index:03}_{}", "x".repeat(40)),
                depth: 1,
                line: Some(index + 1),
                location: None,
                child_count: 3,
                stable_position: None,
            })
            .collect();

        let output = format_symbol_entries(&entries);
        assert!(output.contains("output shortened"));
    }
}
