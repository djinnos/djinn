use std::path::Path;

enum FuzzyResult {
    Unique(String),
    Ambiguous(usize),
}

/// Multi-layer fuzzy string replacement for the edit tool.
///
/// Tries matching strategies in order of strictness:
/// 1. Exact match
/// 2. Line-trimmed match (trailing whitespace stripped per line)
/// 3. Whitespace-normalized match (runs of whitespace collapsed to single space)
/// 4. Indentation-flexible match (leading whitespace stripped per line)
///
/// Returns `(new_content, optional_match_note)`.
pub(super) fn fuzzy_replace(
    content: &str,
    old_text: &str,
    new_text: &str,
    path: &Path,
) -> Result<(String, Option<String>), String> {
    let count = content.matches(old_text).count();
    if count == 1 {
        return Ok((content.replacen(old_text, new_text, 1), None));
    }
    if count > 1 {
        return Err(format!(
            "old_text appears {count} times in file (must be unique): {}",
            path.display()
        ));
    }

    if let Some(result) = try_line_trimmed_match(content, old_text, new_text) {
        return match result {
            FuzzyResult::Unique(new_content) => Ok((
                new_content,
                Some("(matched after trimming trailing whitespace)".to_string()),
            )),
            FuzzyResult::Ambiguous(n) => Err(format!(
                "old_text appears {n} times after trimming trailing whitespace \
                 (must be unique): {}",
                path.display()
            )),
        };
    }

    if let Some(result) = try_whitespace_normalized_match(content, old_text, new_text) {
        return match result {
            FuzzyResult::Unique(new_content) => Ok((
                new_content,
                Some("(matched with whitespace normalization)".to_string()),
            )),
            FuzzyResult::Ambiguous(n) => Err(format!(
                "old_text appears {n} times after whitespace normalization \
                 (must be unique): {}",
                path.display()
            )),
        };
    }

    if let Some(result) = try_indentation_flexible_match(content, old_text, new_text) {
        return match result {
            FuzzyResult::Unique(new_content) => Ok((
                new_content,
                Some("(matched with flexible indentation)".to_string()),
            )),
            FuzzyResult::Ambiguous(n) => Err(format!(
                "old_text appears {n} times after stripping indentation \
                 (must be unique): {}",
                path.display()
            )),
        };
    }

    Err(format!("old_text not found in file: {}", path.display()))
}

/// Trim trailing whitespace from each line, then find the match.
fn try_line_trimmed_match(content: &str, old_text: &str, new_text: &str) -> Option<FuzzyResult> {
    let trimmed_content: String = content
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed_old: String = old_text
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");

    let count = trimmed_content.matches(&trimmed_old as &str).count();
    if count == 0 {
        return None;
    }
    if count > 1 {
        return Some(FuzzyResult::Ambiguous(count));
    }

    let start = trimmed_content.find(&trimmed_old)?;
    let end = start + trimmed_old.len();

    let (orig_start, orig_end) = map_trimmed_to_original(content, &trimmed_content, start, end);
    let mut result = String::with_capacity(content.len());
    result.push_str(&content[..orig_start]);
    result.push_str(new_text);
    result.push_str(&content[orig_end..]);
    Some(FuzzyResult::Unique(result))
}

pub(super) fn reindent_replacement(matched_block: &str, replacement: &str) -> String {
    let matched_lines: Vec<&str> = matched_block.split('\n').collect();
    let replacement_lines: Vec<&str> = replacement.split('\n').collect();

    if replacement_lines.is_empty() {
        return String::new();
    }

    let matched_base_indent = matched_lines
        .iter()
        .find(|line| !line.trim().is_empty())
        .map_or("", |line| leading_whitespace(line));

    let replacement_base_indent = replacement_lines
        .iter()
        .find(|line| !line.trim().is_empty())
        .map_or("", |line| leading_whitespace(line));

    replacement_lines
        .iter()
        .map(|line| {
            if line.is_empty() {
                return String::new();
            }

            let replacement_indent = leading_whitespace(line);
            let relative_indent = replacement_indent
                .strip_prefix(replacement_base_indent)
                .unwrap_or(replacement_indent);

            format!(
                "{matched_base_indent}{relative_indent}{}",
                &line[replacement_indent.len()..]
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn leading_whitespace(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// Map byte positions from a trimmed version back to the original content.
fn map_trimmed_to_original(
    original: &str,
    trimmed: &str,
    trimmed_start: usize,
    trimmed_end: usize,
) -> (usize, usize) {
    let orig_lines: Vec<&str> = original.split('\n').collect();
    let trimmed_lines: Vec<&str> = trimmed.split('\n').collect();

    let mut orig_offset = 0usize;
    let mut trimmed_offset = 0usize;
    let mut result_start = 0usize;
    let mut result_end = 0usize;
    let mut found_start = false;
    let mut found_end = false;

    for (i, (orig_line, trimmed_line)) in orig_lines.iter().zip(trimmed_lines.iter()).enumerate() {
        let newline: usize = usize::from(i < orig_lines.len() - 1);

        if !found_start && trimmed_start < trimmed_offset + trimmed_line.len() + newline {
            let offset_in_line = trimmed_start - trimmed_offset;
            result_start = orig_offset + offset_in_line;
            found_start = true;
        }

        if !found_end && trimmed_end <= trimmed_offset + trimmed_line.len() + newline {
            let offset_in_line = trimmed_end - trimmed_offset;
            let clamped = offset_in_line.min(orig_line.len() + newline);
            result_end = orig_offset + clamped;
            found_end = true;
        }

        orig_offset += orig_line.len() + newline;
        trimmed_offset += trimmed_line.len() + newline;

        if found_start && found_end {
            break;
        }
    }

    (result_start, result_end)
}

/// Collapse all runs of spaces/tabs to a single space, then find the match.
fn try_whitespace_normalized_match(
    content: &str,
    old_text: &str,
    new_text: &str,
) -> Option<FuzzyResult> {
    let (norm_content, content_map) = normalize_whitespace_with_map(content);
    let (norm_old, _) = normalize_whitespace_with_map(old_text);

    let count = norm_content.matches(&norm_old as &str).count();
    if count == 0 {
        return None;
    }
    if count > 1 {
        return Some(FuzzyResult::Ambiguous(count));
    }

    let norm_start = norm_content.find(&norm_old)?;
    let norm_end = norm_start + norm_old.len();

    let orig_start = content_map[norm_start];
    let orig_end = if norm_end >= content_map.len() {
        content.len()
    } else {
        content_map[norm_end]
    };

    let mut result = String::with_capacity(content.len());
    result.push_str(&content[..orig_start]);
    result.push_str(new_text);
    result.push_str(&content[orig_end..]);
    Some(FuzzyResult::Unique(result))
}

/// Normalize whitespace: collapse runs of spaces/tabs to a single space.
/// Returns (normalized_string, map from normalized byte index to original byte
/// index).
fn normalize_whitespace_with_map(s: &str) -> (String, Vec<usize>) {
    let mut normalized = String::with_capacity(s.len());
    let mut map: Vec<usize> = Vec::with_capacity(s.len());
    let mut in_ws = false;
    let bytes = s.as_bytes();

    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' || b == b'\r' {
            in_ws = false;
            normalized.push(b as char);
            map.push(i);
        } else if b == b' ' || b == b'\t' {
            if !in_ws {
                normalized.push(' ');
                map.push(i);
                in_ws = true;
            }
        } else {
            in_ws = false;
            normalized.push(b as char);
            map.push(i);
        }
    }

    (normalized, map)
}

/// Strip leading whitespace from each line, match, then apply edit preserving
/// the file's original indentation.
fn try_indentation_flexible_match(
    content: &str,
    old_text: &str,
    new_text: &str,
) -> Option<FuzzyResult> {
    let stripped_content: String = content
        .lines()
        .map(|l| l.trim_start())
        .collect::<Vec<_>>()
        .join("\n");
    let stripped_old: String = old_text
        .lines()
        .map(|l| l.trim_start())
        .collect::<Vec<_>>()
        .join("\n");

    if stripped_old.is_empty() {
        return None;
    }

    let count = stripped_content.matches(&stripped_old as &str).count();
    if count == 0 {
        return None;
    }
    if count > 1 {
        return Some(FuzzyResult::Ambiguous(count));
    }

    let stripped_start = stripped_content.find(&stripped_old)?;

    let match_start_line = stripped_content[..stripped_start]
        .chars()
        .filter(|&c| c == '\n')
        .count();
    let old_line_count = stripped_old.chars().filter(|&c| c == '\n').count() + 1;

    let content_lines: Vec<&str> = content.lines().collect();

    let mut orig_start = 0usize;
    for line in &content_lines[..match_start_line] {
        orig_start += line.len() + 1;
    }
    let mut orig_end = orig_start;
    for (i, line) in content_lines[match_start_line..]
        .iter()
        .enumerate()
        .take(old_line_count)
    {
        orig_end += line.len();
        if match_start_line + i + 1 < content_lines.len() {
            orig_end += 1;
        }
    }
    orig_end = orig_end.min(content.len());

    let matched_block = &content[orig_start..orig_end];
    let reindented = reindent_replacement(matched_block, new_text);

    let needs_trailing_newline = content[..orig_end].ends_with('\n') && !reindented.ends_with('\n');

    let mut result = String::with_capacity(content.len());
    result.push_str(&content[..orig_start]);
    result.push_str(&reindented);
    if needs_trailing_newline {
        result.push('\n');
    }
    result.push_str(&content[orig_end..]);
    Some(FuzzyResult::Unique(result))
}

#[cfg(test)]
mod tests {
    use super::{fuzzy_replace, reindent_replacement};
    use std::path::Path;

    #[test]
    fn rebases_multiline_replacement_using_matched_indentation() {
        let content = "fn main() {\n    match value {\n        Some(x) => {\n            process(x);\n        }\n    }\n}\n";
        let old_text = "match value {\n    Some(x) => {\n        process(x);\n    }\n}";
        let new_text = "match value {\n    Some(x) => {\n        if ready {\n            process(x);\n        }\n    }\n}";

        let (updated, note) = fuzzy_replace(content, old_text, new_text, Path::new("test.rs"))
            .expect("fuzzy replace should succeed");

        assert_eq!(note.as_deref(), Some("(matched with flexible indentation)"));
        assert!(updated.contains(
            "    match value {\n        Some(x) => {\n            if ready {\n                process(x);\n            }\n        }\n    }"
        ));
    }

    #[test]
    fn preserves_later_nested_indent_when_first_replacement_line_is_less_indented() {
        let content = "impl Example {\n        if condition {\n            run();\n        }\n}\n";
        let old_text = "if condition {\n    run();\n}";
        let new_text =
            "if condition {\n    let nested = || {\n        run();\n    };\n    nested();\n}";

        let (updated, note) = fuzzy_replace(content, old_text, new_text, Path::new("test.rs"))
            .expect("fuzzy replace should succeed");

        assert_eq!(note.as_deref(), Some("(matched with flexible indentation)"));
        assert!(updated.contains(
            "        if condition {\n            let nested = || {\n                run();\n            };\n            nested();\n        }"
        ));
    }

    #[test]
    fn reindent_replacement_preserves_internal_relative_indentation() {
        let matched_block = "        if ready {\n            execute();\n        }";
        let replacement =
            "if ready {\n    let nested = || {\n        execute();\n    };\n    nested();\n}";

        assert_eq!(
            reindent_replacement(matched_block, replacement),
            "        if ready {\n            let nested = || {\n                execute();\n            };\n            nested();\n        }"
        );
    }
}
