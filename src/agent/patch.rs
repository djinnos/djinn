//! Custom LLM-friendly patch format parser and applicator.
//!
//! Inspired by OpenCode's approach, this format uses content-based context
//! matching instead of line numbers, making it resilient to LLM hallucinations.
//!
//! # Format
//!
//! ```text
//! *** Begin Patch
//! *** Update File: path/to/file.rs
//! @@ context_line_from_file @@
//!  context line (unchanged)
//! -old line to remove
//! +new line to add
//!  context line (unchanged)
//!
//! *** Add File: path/to/new_file.rs
//! +line 1
//! +line 2
//!
//! *** Delete File: path/to/old_file.rs
//! *** End Patch
//! ```

use std::path::{Path, PathBuf};

/// A parsed patch containing one or more file operations.
#[derive(Debug)]
pub(crate) struct Patch {
    pub(crate) operations: Vec<FileOp>,
}

/// A single file operation within a patch.
#[derive(Debug)]
pub(crate) enum FileOp {
    Update {
        path: String,
        chunks: Vec<Chunk>,
    },
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
}

impl FileOp {
    pub(crate) fn path(&self) -> &str {
        match self {
            FileOp::Update { path, .. } | FileOp::Add { path, .. } | FileOp::Delete { path } => {
                path
            }
        }
    }
}

/// A single hunk within an Update operation.
#[derive(Debug)]
pub(crate) struct Chunk {
    /// The context text used to locate this chunk in the file (from `@@ ... @@`).
    pub(crate) context_anchor: String,
    /// The lines in this chunk, in order.
    pub(crate) lines: Vec<ChunkLine>,
}

/// A single line within a chunk.
#[derive(Debug, PartialEq)]
pub(crate) enum ChunkLine {
    /// Context line (must match existing file content).
    Context(String),
    /// Line to remove from the file.
    Remove(String),
    /// Line to add to the file.
    Add(String),
}

/// Parse the custom patch format into a structured `Patch`.
pub(crate) fn parse_patch(input: &str) -> Result<Patch, String> {
    let lines: Vec<&str> = input.lines().collect();
    if lines.is_empty() {
        return Err("empty patch input".to_string());
    }

    // Find *** Begin Patch
    let start = lines
        .iter()
        .position(|l| l.trim() == "*** Begin Patch")
        .ok_or("patch must start with '*** Begin Patch'")?;

    // Find *** End Patch
    let end = lines
        .iter()
        .rposition(|l| l.trim() == "*** End Patch")
        .ok_or("patch must end with '*** End Patch'")?;

    if start >= end {
        return Err("'*** Begin Patch' must come before '*** End Patch'".to_string());
    }

    let body = &lines[start + 1..end];
    let mut operations = Vec::new();
    let mut i = 0;

    while i < body.len() {
        let line = body[i].trim();
        if line.is_empty() {
            i += 1;
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Update File:") {
            let path = path.trim().to_string();
            i += 1;
            let (chunks, next_i) = parse_update_chunks(body, i)?;
            if chunks.is_empty() {
                return Err(format!("*** Update File: {path} has no @@ chunks"));
            }
            operations.push(FileOp::Update { path, chunks });
            i = next_i;
        } else if let Some(path) = line.strip_prefix("*** Add File:") {
            let path = path.trim().to_string();
            i += 1;
            let mut content_lines = Vec::new();
            while i < body.len() {
                let l = body[i];
                if l.starts_with("*** ") {
                    break;
                }
                if let Some(rest) = l.strip_prefix('+') {
                    content_lines.push(rest.to_string());
                } else if l.trim().is_empty() {
                    // Allow blank lines between + lines
                    content_lines.push(String::new());
                } else {
                    return Err(format!(
                        "*** Add File: unexpected line (expected '+' prefix): {l}"
                    ));
                }
                i += 1;
            }
            let content = content_lines.join("\n");
            // Add trailing newline if there's content
            let content = if content.is_empty() {
                content
            } else {
                content + "\n"
            };
            operations.push(FileOp::Add { path, content });
        } else if let Some(path) = line.strip_prefix("*** Delete File:") {
            let path = path.trim().to_string();
            operations.push(FileOp::Delete { path });
            i += 1;
        } else {
            return Err(format!("unexpected line in patch: {line}"));
        }
    }

    if operations.is_empty() {
        return Err("patch contains no file operations".to_string());
    }

    Ok(Patch { operations })
}

/// Parse the `@@ context @@` chunks for an Update operation.
/// Returns the parsed chunks and the index after the last consumed line.
fn parse_update_chunks(body: &[&str], start: usize) -> Result<(Vec<Chunk>, usize), String> {
    let mut chunks = Vec::new();
    let mut i = start;

    while i < body.len() {
        let line = body[i];
        // Stop at a new file operation
        if line.starts_with("*** ") {
            break;
        }
        // Skip blank lines between chunks
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        // Must be a @@ line
        if line.starts_with("@@") {
            let context_anchor = parse_context_anchor(line)?;
            i += 1;
            let mut chunk_lines = Vec::new();
            while i < body.len() {
                let cl = body[i];
                if cl.starts_with("@@") || cl.starts_with("*** ") {
                    break;
                }
                if cl.trim().is_empty() && i + 1 < body.len() {
                    let next = body[i + 1];
                    if next.starts_with("@@") || next.starts_with("*** ") {
                        // Blank line before next chunk/op — skip it
                        i += 1;
                        break;
                    }
                }
                if let Some(rest) = cl.strip_prefix('+') {
                    chunk_lines.push(ChunkLine::Add(rest.to_string()));
                } else if let Some(rest) = cl.strip_prefix('-') {
                    chunk_lines.push(ChunkLine::Remove(rest.to_string()));
                } else if let Some(rest) = cl.strip_prefix(' ') {
                    chunk_lines.push(ChunkLine::Context(rest.to_string()));
                } else if cl.trim().is_empty() {
                    // Treat empty lines inside a chunk as empty context lines
                    chunk_lines.push(ChunkLine::Context(String::new()));
                } else {
                    return Err(format!(
                        "unexpected line in chunk (expected ' ', '+', '-' prefix): {cl}"
                    ));
                }
                i += 1;
            }
            chunks.push(Chunk {
                context_anchor,
                lines: chunk_lines,
            });
        } else {
            return Err(format!(
                "expected @@ context @@ line in Update block, got: {line}"
            ));
        }
    }

    Ok((chunks, i))
}

/// Extract the context text from `@@ some text @@`.
fn parse_context_anchor(line: &str) -> Result<String, String> {
    let trimmed = line.trim();
    if !trimmed.starts_with("@@") {
        return Err(format!("expected @@ context @@ line, got: {line}"));
    }
    let inner = trimmed.strip_prefix("@@").unwrap();
    let inner = inner
        .strip_suffix("@@")
        .ok_or_else(|| format!("@@ line missing closing @@: {line}"))?;
    Ok(inner.trim().to_string())
}

/// Apply a parsed patch to files within the given worktree.
///
/// Returns a list of (path, action) pairs for files that were modified.
/// The caller is responsible for FileTime assertions and LSP notifications.
pub(crate) async fn apply_patch(
    patch: &Patch,
    worktree_path: &Path,
) -> Result<Vec<(PathBuf, &'static str)>, String> {
    let mut results = Vec::new();

    for op in &patch.operations {
        match op {
            FileOp::Update { path, chunks } => {
                let file_path = resolve_op_path(path, worktree_path);
                let content = tokio::fs::read_to_string(&file_path)
                    .await
                    .map_err(|e| format!("failed to read {path}: {e}"))?;
                let updated = apply_chunks(&content, chunks, path)?;
                tokio::fs::write(&file_path, &updated)
                    .await
                    .map_err(|e| format!("failed to write {path}: {e}"))?;
                results.push((file_path, "updated"));
            }
            FileOp::Add {
                path,
                content: new_content,
            } => {
                let file_path = resolve_op_path(path, worktree_path);
                if let Some(parent) = file_path.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| format!("failed to create directories for {path}: {e}"))?;
                }
                tokio::fs::write(&file_path, new_content)
                    .await
                    .map_err(|e| format!("failed to write new file {path}: {e}"))?;
                results.push((file_path, "created"));
            }
            FileOp::Delete { path } => {
                let file_path = resolve_op_path(path, worktree_path);
                tokio::fs::remove_file(&file_path)
                    .await
                    .map_err(|e| format!("failed to delete {path}: {e}"))?;
                results.push((file_path, "deleted"));
            }
        }
    }

    Ok(results)
}

fn resolve_op_path(raw: &str, worktree: &Path) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() { p.to_path_buf() } else { worktree.join(p) }
}

/// Apply all chunks to the file content, returning the modified content.
fn apply_chunks(content: &str, chunks: &[Chunk], file_path: &str) -> Result<String, String> {
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    // Track if original content ended with newline
    let trailing_newline = content.ends_with('\n');

    // Apply chunks in reverse order so that earlier line indices stay valid
    // after modifications from later chunks.
    let mut chunk_positions: Vec<(usize, &Chunk)> = Vec::new();

    for chunk in chunks {
        let pos = find_chunk_position(&lines, chunk, file_path)?;
        chunk_positions.push((pos, chunk));
    }

    // Sort by position descending so we apply from bottom to top
    chunk_positions.sort_by(|a, b| b.0.cmp(&a.0));

    for (start_pos, chunk) in chunk_positions {
        apply_single_chunk(&mut lines, start_pos, chunk)?;
    }

    let mut result = lines.join("\n");
    if trailing_newline {
        result.push('\n');
    }
    Ok(result)
}

/// Find the line index where a chunk should be applied.
fn find_chunk_position(
    lines: &[String],
    chunk: &Chunk,
    file_path: &str,
) -> Result<usize, String> {
    // First, try to find using the context anchor
    let anchor = &chunk.context_anchor;
    if !anchor.is_empty() {
        // Exact match
        for (i, line) in lines.iter().enumerate() {
            if line.trim() == anchor.trim() {
                return Ok(i);
            }
        }
        // Fuzzy: contains
        for (i, line) in lines.iter().enumerate() {
            if line.contains(anchor.trim()) {
                return Ok(i);
            }
        }
    }

    // Fall back: try matching the first context or remove line in the chunk
    let first_match_text = chunk.lines.iter().find_map(|cl| match cl {
        ChunkLine::Context(t) | ChunkLine::Remove(t) => Some(t.as_str()),
        ChunkLine::Add(_) => None,
    });

    if let Some(text) = first_match_text {
        for (i, line) in lines.iter().enumerate() {
            if line.trim() == text.trim() {
                return Ok(i);
            }
        }
    }

    Err(format!(
        "could not locate chunk in {file_path}: context anchor '@@\u{a0}{anchor}\u{a0}@@' not found in file. \
         Ensure the @@ line contains text that appears verbatim in the file."
    ))
}

/// Apply a single chunk starting at the given position.
fn apply_single_chunk(
    lines: &mut Vec<String>,
    start_pos: usize,
    chunk: &Chunk,
) -> Result<(), String> {
    let mut consumed = 0; // how many original lines we consume
    let mut output: Vec<String> = Vec::new();

    for cl in &chunk.lines {
        match cl {
            ChunkLine::Context(text) => {
                let actual_idx = start_pos + consumed;
                if actual_idx >= lines.len() {
                    return Err(format!(
                        "context line '{text}' extends past end of file at line {}",
                        actual_idx + 1
                    ));
                }
                let actual = &lines[actual_idx];
                if actual.trim() != text.trim() {
                    return Err(format!(
                        "context mismatch at line {}: expected '{}', found '{}'",
                        actual_idx + 1,
                        text,
                        actual
                    ));
                }
                output.push(actual.clone());
                consumed += 1;
            }
            ChunkLine::Remove(text) => {
                let actual_idx = start_pos + consumed;
                if actual_idx >= lines.len() {
                    return Err(format!(
                        "remove line '{text}' extends past end of file at line {}",
                        actual_idx + 1
                    ));
                }
                let actual = &lines[actual_idx];
                if actual.trim() != text.trim() {
                    return Err(format!(
                        "remove mismatch at line {}: expected '{}', found '{}'",
                        actual_idx + 1,
                        text,
                        actual
                    ));
                }
                // Don't add to output — it's deleted
                consumed += 1;
            }
            ChunkLine::Add(text) => {
                output.push(text.clone());
                // Don't consume any original lines
            }
        }
    }

    // Replace the consumed range with the output
    let range = start_pos..start_pos + consumed;
    lines.splice(range, output);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_parse_basic_patch() {
        let input = r#"*** Begin Patch
*** Update File: src/main.rs
@@ fn main() @@
 fn main() {
-    println!("hello");
+    println!("world");
 }
*** End Patch"#;

        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 1);
        match &patch.operations[0] {
            FileOp::Update { path, chunks } => {
                assert_eq!(path, "src/main.rs");
                assert_eq!(chunks.len(), 1);
                assert_eq!(chunks[0].context_anchor, "fn main()");
                assert_eq!(chunks[0].lines.len(), 4);
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn test_parse_add_file() {
        let input = r#"*** Begin Patch
*** Add File: src/new.rs
+fn new_func() {
+    todo!()
+}
*** End Patch"#;

        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 1);
        match &patch.operations[0] {
            FileOp::Add { path, content } => {
                assert_eq!(path, "src/new.rs");
                assert_eq!(content, "fn new_func() {\n    todo!()\n}\n");
            }
            _ => panic!("expected Add"),
        }
    }

    #[test]
    fn test_parse_delete_file() {
        let input = r#"*** Begin Patch
*** Delete File: src/old.rs
*** End Patch"#;

        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 1);
        match &patch.operations[0] {
            FileOp::Delete { path } => assert_eq!(path, "src/old.rs"),
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn test_parse_multiple_operations() {
        let input = r#"*** Begin Patch
*** Update File: a.rs
@@ fn a() @@
 fn a() {
-    old();
+    new();
 }

*** Add File: b.rs
+fn b() {}

*** Delete File: c.rs
*** End Patch"#;

        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 3);
    }

    #[test]
    fn test_parse_multiple_chunks() {
        let input = r#"*** Begin Patch
*** Update File: src/lib.rs
@@ fn first() @@
 fn first() {
-    old1();
+    new1();
 }

@@ fn second() @@
 fn second() {
-    old2();
+    new2();
 }
*** End Patch"#;

        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            FileOp::Update { chunks, .. } => {
                assert_eq!(chunks.len(), 2);
                assert_eq!(chunks[0].context_anchor, "fn first()");
                assert_eq!(chunks[1].context_anchor, "fn second()");
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn test_apply_update_chunk() {
        let content = "fn main() {\n    println!(\"hello\");\n}\n";
        let chunks = vec![Chunk {
            context_anchor: "fn main()".to_string(),
            lines: vec![
                ChunkLine::Context("fn main() {".to_string()),
                ChunkLine::Remove("    println!(\"hello\");".to_string()),
                ChunkLine::Add("    println!(\"world\");".to_string()),
                ChunkLine::Context("}".to_string()),
            ],
        }];

        let result = apply_chunks(content, &chunks, "test.rs").unwrap();
        assert_eq!(result, "fn main() {\n    println!(\"world\");\n}\n");
    }

    #[test]
    fn test_apply_multiple_chunks_reverse_order() {
        let content = "fn a() {\n    old_a();\n}\n\nfn b() {\n    old_b();\n}\n";
        let chunks = vec![
            Chunk {
                context_anchor: "fn a()".to_string(),
                lines: vec![
                    ChunkLine::Context("fn a() {".to_string()),
                    ChunkLine::Remove("    old_a();".to_string()),
                    ChunkLine::Add("    new_a();".to_string()),
                    ChunkLine::Context("}".to_string()),
                ],
            },
            Chunk {
                context_anchor: "fn b()".to_string(),
                lines: vec![
                    ChunkLine::Context("fn b() {".to_string()),
                    ChunkLine::Remove("    old_b();".to_string()),
                    ChunkLine::Add("    new_b();".to_string()),
                    ChunkLine::Context("}".to_string()),
                ],
            },
        ];

        let result = apply_chunks(content, &chunks, "test.rs").unwrap();
        assert_eq!(
            result,
            "fn a() {\n    new_a();\n}\n\nfn b() {\n    new_b();\n}\n"
        );
    }

    #[test]
    fn test_context_mismatch_error() {
        let content = "fn main() {\n    actual_line();\n}\n";
        let chunks = vec![Chunk {
            context_anchor: "fn main()".to_string(),
            lines: vec![
                ChunkLine::Context("fn main() {".to_string()),
                ChunkLine::Remove("    wrong_line();".to_string()),
                ChunkLine::Context("}".to_string()),
            ],
        }];

        let result = apply_chunks(content, &chunks, "test.rs");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("remove mismatch"));
    }

    #[test]
    fn test_anchor_not_found_error() {
        let content = "fn main() {\n}\n";
        let chunks = vec![Chunk {
            context_anchor: "fn nonexistent()".to_string(),
            lines: vec![ChunkLine::Context("fn nonexistent() {".to_string())],
        }];

        let result = apply_chunks(content, &chunks, "test.rs");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found in file"));
    }

    #[tokio::test]
    async fn test_apply_patch_add_file() {
        let dir = TempDir::new().unwrap();
        let patch = Patch {
            operations: vec![FileOp::Add {
                path: "new_file.txt".to_string(),
                content: "hello\nworld\n".to_string(),
            }],
        };

        let results = apply_patch(&patch, dir.path()).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, "created");

        let content = tokio::fs::read_to_string(dir.path().join("new_file.txt"))
            .await
            .unwrap();
        assert_eq!(content, "hello\nworld\n");
    }

    #[tokio::test]
    async fn test_apply_patch_delete_file() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("to_delete.txt");
        tokio::fs::write(&file_path, "content").await.unwrap();

        let patch = Patch {
            operations: vec![FileOp::Delete {
                path: "to_delete.txt".to_string(),
            }],
        };

        let results = apply_patch(&patch, dir.path()).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, "deleted");
        assert!(!file_path.exists());
    }

    #[tokio::test]
    async fn test_apply_patch_update_file() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("update_me.rs");
        tokio::fs::write(&file_path, "fn main() {\n    println!(\"old\");\n}\n")
            .await
            .unwrap();

        let patch_input = r#"*** Begin Patch
*** Update File: update_me.rs
@@ fn main() @@
 fn main() {
-    println!("old");
+    println!("new");
 }
*** End Patch"#;

        let parsed = parse_patch(patch_input).unwrap();
        let results = apply_patch(&parsed, dir.path()).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, "updated");

        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "fn main() {\n    println!(\"new\");\n}\n");
    }

    #[test]
    fn test_parse_error_no_begin() {
        let result = parse_patch("*** Update File: foo.rs\n*** End Patch");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Begin Patch"));
    }

    #[test]
    fn test_parse_error_no_end() {
        let result = parse_patch("*** Begin Patch\n*** Update File: foo.rs");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("End Patch"));
    }

    #[test]
    fn test_parse_error_empty() {
        let result = parse_patch("");
        assert!(result.is_err());
    }

    #[test]
    fn test_add_only_lines() {
        let content = "line1\nline2\nline3\n";
        let chunks = vec![Chunk {
            context_anchor: "line2".to_string(),
            lines: vec![
                ChunkLine::Context("line2".to_string()),
                ChunkLine::Add("inserted".to_string()),
                ChunkLine::Context("line3".to_string()),
            ],
        }];

        let result = apply_chunks(content, &chunks, "test.rs").unwrap();
        assert_eq!(result, "line1\nline2\ninserted\nline3\n");
    }

    #[test]
    fn test_remove_only_lines() {
        let content = "line1\nline2\nline3\n";
        let chunks = vec![Chunk {
            context_anchor: "line1".to_string(),
            lines: vec![
                ChunkLine::Context("line1".to_string()),
                ChunkLine::Remove("line2".to_string()),
                ChunkLine::Context("line3".to_string()),
            ],
        }];

        let result = apply_chunks(content, &chunks, "test.rs").unwrap();
        assert_eq!(result, "line1\nline3\n");
    }
}
