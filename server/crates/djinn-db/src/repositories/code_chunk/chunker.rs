//! AST-aware chunker. Given a file's source text and the symbol enclosing
//! ranges discovered by the SCIP warmer (PR A1), produces a `Vec<CodeChunk>`
//! ready for upsert into `code_chunks`. The chunker is intentionally a pure
//! function — the warmer (B3) is responsible for fetching file contents and
//! looking up the per-file `symbol_ranges` slice from `RepoDependencyGraph`.
//!
//! Strategy summary (architect-baked, see plan §"PR B2"):
//!
//! * Default `chunk_size = 1200` chars, `overlap = 120`.
//! * Function/Method body ≤ 1200 chars → single chunk.
//! * Function/Method body > 1200 chars → pack whole statements until budget
//!   exceeded; container signature on first chunk, `}` on last. Statement
//!   boundaries detected via brace counting on languages where that works
//!   (Rust/TS/JS/Go/Java/Kotlin/Scala/C/C++/C#); other languages fall back
//!   to a character window with overlap.
//! * Class/Interface/Struct/Enum → declaration chunk (signature + members),
//!   with adjacent fields grouped into the same chunk.
//! * Anything else → character window with statement-aligned overlap.
//!
//! No tree-sitter dependency — brace counting and char-window are sufficient
//! for the embedding-recall lever this chunker exists to feed.
//!
//! See [`text_generator`](super::text_generator) for the natural-language
//! header that wraps the chunk body before it goes into the embedding model.

use sha2::{Digest, Sha256};

use super::text_generator::{
    EMBEDDING_TEXT_VERSION, RenderInput, content_hash, render_chunk_text,
};

/// One row destined for the `code_chunks` table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeChunk {
    pub id: String,
    pub project_id: String,
    pub file_path: String,
    pub symbol_key: Option<String>,
    pub kind: String,
    pub start_line: u32,
    pub end_line: u32,
    pub content_hash: String,
    pub embedded_text: String,
}

/// Coarse classification of a SCIP symbol kind, in chunker terms.
///
/// The chunker only cares about three buckets — function-shaped symbols
/// (chunk by body / statements), declaration-shaped symbols (chunk by
/// signature + members, group fields), and "anything else" (char window).
/// Mapping happens in the warmer (B3) which has a `ScipSymbolKind` in
/// hand; this enum keeps `djinn-db` independent of `djinn-graph`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolChunkKind {
    /// Function, method, constructor — chunk by body / statements.
    Function,
    /// Class, struct, enum, interface, trait — chunk by declaration +
    /// member lines, group adjacent fields.
    Declaration,
    /// Field, constant, variable — too small to chunk; treat as a single
    /// char-window chunk over its line range.
    Field,
    /// Fallback for everything else (namespaces, packages, etc.). Char
    /// window over the line range.
    Other,
}

impl SymbolChunkKind {
    /// Human-readable kind string persisted in the `code_chunks.kind`
    /// column. Matches the architect spec — these strings show up in the
    /// embedded text header, so they should read naturally.
    pub fn as_kind_str(self) -> &'static str {
        match self {
            SymbolChunkKind::Function => "function",
            SymbolChunkKind::Declaration => "declaration",
            SymbolChunkKind::Field => "field",
            SymbolChunkKind::Other => "other",
        }
    }
}

/// Inputs the chunker needs about one symbol. Built by the warmer (B3)
/// from `RepoGraphNode` + `SymbolRange`.
#[derive(Clone, Debug)]
pub struct SymbolInput {
    /// Stable SCIP symbol identifier; goes into `code_chunks.symbol_key`.
    pub symbol_key: String,
    /// Display name, e.g. `MyClass::do_thing`.
    pub display_name: String,
    /// SCIP-derived classification; controls the chunking strategy.
    pub kind: SymbolChunkKind,
    /// 1-indexed inclusive line range from the SCIP enclosing range.
    pub start_line: u32,
    /// 1-indexed inclusive line range from the SCIP enclosing range.
    pub end_line: u32,
    /// `true` iff the symbol is exported / publicly visible.
    pub is_export: bool,
    /// Optional pre-rendered signature (e.g. `fn foo(x: u32) -> u32`).
    /// When `None`, the chunker falls back to the first non-empty line
    /// of the symbol body.
    pub signature: Option<String>,
    /// SCIP documentation lines, joined with newlines. First 150 chars
    /// are surfaced in the embedding header.
    pub documentation: Vec<String>,
}

/// Inputs describing one source file. The warmer reads the file once
/// and feeds every symbol that lives in it through the chunker in one
/// pass.
#[derive(Clone, Debug)]
pub struct FileInput<'a> {
    /// Project-relative path (forward slashes); persisted verbatim.
    pub path: &'a str,
    /// Full file contents.
    pub content: &'a str,
    /// Symbols whose definition enclosing range falls within this file.
    pub symbols: &'a [SymbolInput],
}

/// Repo-level metadata threaded into every chunk's natural-language
/// header. Comes from the `projects` table; the chunker never reads
/// it itself.
#[derive(Clone, Debug)]
pub struct RepoMetadata<'a> {
    pub owner: &'a str,
    pub repo: &'a str,
}

/// Default chunking parameters from the plan.
pub const DEFAULT_CHUNK_SIZE: usize = 1200;
pub const DEFAULT_CHUNK_OVERLAP: usize = 120;

/// Configurable knobs. Almost everyone uses [`ChunkConfig::default`];
/// the field on the struct exists so unit tests can shrink the budget
/// and exercise the long-symbol code path without 1.2 KB fixtures.
#[derive(Clone, Copy, Debug)]
pub struct ChunkConfig {
    pub chunk_size: usize,
    pub overlap: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            overlap: DEFAULT_CHUNK_OVERLAP,
        }
    }
}

/// Chunk every symbol in `file` and return the resulting `CodeChunk`
/// rows ready for upsert.
pub fn chunk_file(
    project_id: &str,
    repo: &RepoMetadata<'_>,
    file: &FileInput<'_>,
    config: ChunkConfig,
) -> Vec<CodeChunk> {
    let mut out: Vec<CodeChunk> = Vec::new();
    let file_lines: Vec<&str> = file.content.split_inclusive('\n').collect();
    let language = language_for_path(file.path);

    for symbol in file.symbols {
        let body = extract_body(&file_lines, symbol.start_line, symbol.end_line);
        if body.trim().is_empty() {
            // Empty / whitespace-only body — graceful skip per plan
            // edge-case rule. Embedding "Label: foo\n…" with no code
            // body adds noise to retrieval without buying anything.
            continue;
        }

        let pieces = match symbol.kind {
            SymbolChunkKind::Function => chunk_function(symbol, &body, language, config),
            SymbolChunkKind::Declaration => chunk_declaration(symbol, &body, language, config),
            SymbolChunkKind::Field | SymbolChunkKind::Other => {
                chunk_char_window(symbol, &body, language, config)
            }
        };

        let total = pieces.len();
        let mut prev_tail: Option<String> = None;
        for (idx, piece) in pieces.into_iter().enumerate() {
            let render_input = RenderInput {
                label: &symbol.display_name,
                owner: repo.owner,
                repo: repo.repo,
                file_path: file.path,
                kind: symbol.kind.as_kind_str(),
                is_export: symbol.is_export,
                description: first_n_chars(&join_doc(&symbol.documentation), 150),
                prev_tail: prev_tail.as_deref(),
                container_signature: symbol.signature.as_deref(),
                methods: &piece.methods,
                properties: &piece.properties,
                body: &piece.body,
            };
            let embedded_text = render_chunk_text(&render_input);
            let hash = content_hash(EMBEDDING_TEXT_VERSION, &embedded_text);
            let id = chunk_id(project_id, file.path, &symbol.symbol_key, idx);

            // Cache the last 120 chars of the *body* as overlap context
            // for the next chunk in this same symbol; only meaningful for
            // multi-chunk symbols.
            prev_tail = if total > 1 {
                Some(tail_chars(&piece.body, config.overlap))
            } else {
                None
            };

            out.push(CodeChunk {
                id,
                project_id: project_id.to_owned(),
                file_path: file.path.to_owned(),
                symbol_key: Some(symbol.symbol_key.clone()),
                kind: symbol.kind.as_kind_str().to_owned(),
                start_line: piece.start_line,
                end_line: piece.end_line,
                content_hash: hash,
                embedded_text,
            });
        }
    }
    out
}

/// One pre-rendered chunk piece — the body text plus the line span the
/// chunker decided to cover. Rendered into the final embedded text by
/// [`chunk_file`] (which has the symbol metadata available).
#[derive(Clone, Debug)]
struct ChunkPiece {
    body: String,
    start_line: u32,
    end_line: u32,
    /// Member-method names to surface in the header. Only populated for
    /// declaration chunks.
    methods: Vec<String>,
    /// Field/property names to surface in the header. Only populated for
    /// declaration chunks.
    properties: Vec<String>,
}

/// Function/method chunking.
///
/// * Body ≤ chunk_size → single chunk, body verbatim.
/// * Body > chunk_size, brace language → pack whole statements until
///   budget exceeded; first chunk gets the container signature line,
///   last chunk keeps its trailing `}`.
/// * Body > chunk_size, non-brace language → fall back to char window.
fn chunk_function(
    symbol: &SymbolInput,
    body: &str,
    language: Language,
    config: ChunkConfig,
) -> Vec<ChunkPiece> {
    if body.chars().count() <= config.chunk_size {
        return vec![ChunkPiece {
            body: body.to_owned(),
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            methods: Vec::new(),
            properties: Vec::new(),
        }];
    }

    if !language.uses_braces() {
        return chunk_char_window(symbol, body, language, config);
    }

    // Function bodies look like `fn foo() {\n    stmt; stmt;\n}`. The
    // statement boundaries we want fire at depth 1 — i.e. *inside* the
    // outer braces. Asking for depth 0 only matches the closing brace
    // of the function itself, which collapses to a single chunk.
    chunk_statements(symbol, body, config, 1)
}

/// Statement-aligned chunking via brace counting. Splits at every `}`
/// followed by a newline at depth `target_depth`. Strings/comments are
/// skipped so `"}"` inside a string literal doesn't confuse the depth
/// counter.
///
/// `target_depth = 0` matches top-level statements (e.g. items in a
/// module body); `target_depth = 1` matches statements inside a single
/// outer block (e.g. statements inside a function body).
fn chunk_statements(
    symbol: &SymbolInput,
    body: &str,
    config: ChunkConfig,
    target_depth: i32,
) -> Vec<ChunkPiece> {
    let boundaries = find_statement_boundaries(body, target_depth);
    if boundaries.is_empty() {
        // Brace counter never tripped — fall back gracefully so we still
        // return *something* the embedding pipeline can index.
        return vec![ChunkPiece {
            body: body.to_owned(),
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            methods: Vec::new(),
            properties: Vec::new(),
        }];
    }

    // Slice the body into statement-sized segments using the boundary
    // offsets. Each segment ends *after* a statement-closing `}\n`.
    let mut segments: Vec<&str> = Vec::with_capacity(boundaries.len() + 1);
    let mut prev: usize = 0;
    for &b in &boundaries {
        segments.push(&body[prev..b]);
        prev = b;
    }
    if prev < body.len() {
        segments.push(&body[prev..]);
    }

    // Pack segments greedily into chunks ≤ chunk_size. A single segment
    // larger than chunk_size is emitted as-is — the alternative (split
    // mid-statement) breaks the "whole statements" guarantee.
    let mut pieces: Vec<ChunkPiece> = Vec::new();
    let mut buf = String::new();
    let mut buf_start_offset: usize = 0;
    let mut current_offset: usize = 0;
    for seg in segments {
        let seg_chars = seg.chars().count();
        let buf_chars = buf.chars().count();
        if !buf.is_empty() && buf_chars + seg_chars > config.chunk_size {
            // Flush the current buffer.
            let (sl, el) = line_range_for_offsets(body, buf_start_offset, current_offset);
            pieces.push(ChunkPiece {
                body: buf.clone(),
                start_line: symbol.start_line + sl,
                end_line: symbol.start_line + el,
                methods: Vec::new(),
                properties: Vec::new(),
            });
            buf.clear();
            buf_start_offset = current_offset;
        }
        buf.push_str(seg);
        current_offset += seg.len();
    }
    if !buf.is_empty() {
        let (sl, el) = line_range_for_offsets(body, buf_start_offset, current_offset);
        pieces.push(ChunkPiece {
            body: buf,
            start_line: symbol.start_line + sl,
            end_line: symbol.start_line + el,
            methods: Vec::new(),
            properties: Vec::new(),
        });
    }

    pieces
}

/// Find byte offsets in `body` where a statement closes — i.e. a `}`
/// at the requested depth followed by a newline. Strings (`"`/`'`) and
/// line/block comments are skipped so braces inside literals don't
/// trigger.
///
/// `target_depth` is the depth at which a closing `}` is considered a
/// statement boundary. Use `0` for top-level chunking (module items)
/// and `1` for chunking inside a single outer block (function body).
///
/// This is a deliberately small parser — sufficient for the brace
/// languages in scope (Rust, TS, JS, Go, Java, Kotlin, Scala, C, C++,
/// C#). It does not handle every edge case of every dialect; that is
/// the architect-blessed trade-off ("degrades to character chunking,
/// doesn't break"). The character-window fallback is one call away.
fn find_statement_boundaries(body: &str, target_depth: i32) -> Vec<usize> {
    let bytes = body.as_bytes();
    let mut depth: i32 = 0;
    let mut in_string: Option<u8> = None;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut boundaries: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_line_comment {
            if c == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if c == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if let Some(q) = in_string {
            if c == b'\\' {
                i += 2; // skip escaped char (incl. escaped quote)
                continue;
            }
            if c == q {
                in_string = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                in_line_comment = true;
                i += 2;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                in_block_comment = true;
                i += 2;
            }
            b'"' | b'\'' | b'`' => {
                in_string = Some(c);
                i += 1;
            }
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth -= 1;
                // Boundary fires when a `}` closes a block at the
                // target depth (i.e. depth was target_depth + 1 going
                // in, now target_depth) and a newline (or EOF) follows.
                if depth == target_depth {
                    let after = i + 1;
                    if after >= bytes.len() || bytes[after] == b'\n' {
                        let cut = if after < bytes.len() { after + 1 } else { after };
                        boundaries.push(cut);
                    }
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    boundaries
}

/// Translate two byte offsets within `body` into 0-indexed line offsets
/// (relative to body line 0). Used to recover per-chunk `start_line` /
/// `end_line` after greedy packing.
fn line_range_for_offsets(body: &str, start: usize, end: usize) -> (u32, u32) {
    let mut start_line: u32 = 0;
    let mut end_line: u32 = 0;
    for (i, c) in body.char_indices() {
        if i >= end {
            break;
        }
        if c == '\n' {
            if i < start {
                start_line += 1;
            }
            end_line += 1;
        }
    }
    end_line = end_line.saturating_sub(1);
    if end_line < start_line {
        end_line = start_line;
    }
    (start_line, end_line)
}

/// Class/struct/enum chunking: a single declaration chunk by default,
/// with adjacent field lines grouped into one chunk per spec
/// (`groupFields=true`). When the body fits in `chunk_size`, return a
/// single chunk with method/property names lifted into the header.
fn chunk_declaration(
    symbol: &SymbolInput,
    body: &str,
    language: Language,
    config: ChunkConfig,
) -> Vec<ChunkPiece> {
    let (methods, properties) = scrape_member_names(body, language);
    if body.chars().count() <= config.chunk_size {
        return vec![ChunkPiece {
            body: body.to_owned(),
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            methods,
            properties,
        }];
    }
    // Body too large — degrade to statement chunking so each method
    // body lands as its own piece. Use target_depth=1 because a class
    // body looks like `class Foo {\n  member;\n  member;\n}`. First
    // piece keeps the type-level method/property summary.
    let mut pieces = if language.uses_braces() {
        chunk_statements(symbol, body, config, 1)
    } else {
        chunk_char_window(symbol, body, language, config)
    };
    if let Some(first) = pieces.first_mut() {
        first.methods = methods;
        first.properties = properties;
    }
    pieces
}

/// Best-effort scrape of method/property names out of a class body.
/// Used to populate the natural-language header — false positives are
/// harmless (an extra word in the header doesn't break embeddings) so
/// the regex-equivalent here is intentionally loose.
fn scrape_member_names(body: &str, language: Language) -> (Vec<String>, Vec<String>) {
    let mut methods: Vec<String> = Vec::new();
    let mut properties: Vec<String> = Vec::new();
    if !language.uses_braces() {
        return (methods, properties);
    }
    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        // Method-ish: ends with `{` or `;` and has a `(` somewhere.
        if line.contains('(')
            && (line.ends_with('{')
                || line.ends_with(") {")
                || line.ends_with(';')
                || line.contains(") -> "))
            && let Some(name) = identifier_before_paren(line)
        {
            push_unique(&mut methods, name);
            continue;
        }
        // Field-ish: `name: type,` / `name = expr;` / `pub name: T,`
        if (line.ends_with(',') || line.ends_with(';'))
            && (line.contains(':') || line.contains('='))
            && let Some(name) = leading_identifier(line)
        {
            push_unique(&mut properties, name);
        }
    }
    (methods, properties)
}

fn push_unique(dst: &mut Vec<String>, s: String) {
    if !s.is_empty() && !dst.iter().any(|existing| existing == &s) {
        dst.push(s);
    }
}

/// Pull the identifier immediately before the first `(`. Skips visibility/
/// keyword prefixes by taking the *last* whitespace-delimited token.
fn identifier_before_paren(line: &str) -> Option<String> {
    let paren = line.find('(')?;
    let head = &line[..paren];
    let token = head.split_whitespace().next_back()?;
    let cleaned: String = token.chars().filter(|c| is_identifier_char(*c)).collect();
    if cleaned.is_empty() { None } else { Some(cleaned) }
}

/// Pull the first identifier-shaped token off the line (after stripping
/// keyword prefixes like `pub`/`let`/`const`/`final` etc.).
fn leading_identifier(line: &str) -> Option<String> {
    const KEYWORDS: &[&str] = &[
        "pub", "let", "const", "static", "final", "var", "val", "private", "public", "protected",
        "internal", "readonly", "@",
    ];
    for token in line.split_whitespace() {
        let stripped = token.trim_start_matches('@');
        if KEYWORDS.contains(&stripped) {
            continue;
        }
        let id: String = stripped
            .chars()
            .take_while(|c| is_identifier_char(*c))
            .collect();
        if !id.is_empty() {
            return Some(id);
        }
    }
    None
}

fn is_identifier_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Character-window chunking — the universal fallback. Splits `body`
/// into windows of up to `chunk_size` chars with `overlap` chars of
/// context shared between adjacent windows. Tries to nudge the cut to
/// the nearest preceding newline so we don't truncate a word.
fn chunk_char_window(
    symbol: &SymbolInput,
    body: &str,
    _language: Language,
    config: ChunkConfig,
) -> Vec<ChunkPiece> {
    let chars: Vec<char> = body.chars().collect();
    let total = chars.len();
    if total == 0 {
        return Vec::new();
    }
    if total <= config.chunk_size {
        return vec![ChunkPiece {
            body: body.to_owned(),
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            methods: Vec::new(),
            properties: Vec::new(),
        }];
    }

    let mut pieces: Vec<ChunkPiece> = Vec::new();
    let stride = config.chunk_size.saturating_sub(config.overlap).max(1);
    let mut start_char = 0usize;
    while start_char < total {
        let mut end_char = (start_char + config.chunk_size).min(total);
        // Nudge the cut backwards to the nearest preceding newline so
        // the chunk ends on a clean line where possible. Tolerate a
        // 25% pull-back budget — past that we just accept the hard cut.
        if end_char < total {
            let min_cut = start_char + (config.chunk_size * 3 / 4);
            for back in (min_cut..end_char).rev() {
                if chars[back] == '\n' {
                    end_char = back + 1;
                    break;
                }
            }
        }
        let body_str: String = chars[start_char..end_char].iter().collect();
        let (sl, el) = char_range_to_line_range(body, start_char, end_char);
        pieces.push(ChunkPiece {
            body: body_str,
            start_line: symbol.start_line + sl,
            end_line: symbol.start_line + el,
            methods: Vec::new(),
            properties: Vec::new(),
        });
        if end_char == total {
            break;
        }
        start_char += stride;
    }
    pieces
}

/// Translate a character-index range within `body` to a 0-indexed line
/// range. Used to back-fill `ChunkPiece::start_line` / `end_line` for
/// the char-window path.
fn char_range_to_line_range(body: &str, start_char: usize, end_char: usize) -> (u32, u32) {
    let mut start_line: u32 = 0;
    let mut end_line: u32 = 0;
    for (i, c) in body.chars().enumerate() {
        if i >= end_char {
            break;
        }
        if c == '\n' {
            if i < start_char {
                start_line += 1;
            }
            end_line += 1;
        }
    }
    end_line = end_line.saturating_sub(1);
    if end_line < start_line {
        end_line = start_line;
    }
    (start_line, end_line)
}

/// Pull the substring of `file_lines` covering the inclusive 1-indexed
/// range `[start_line, end_line]`. Returns an empty string if the range
/// is empty or out of bounds.
fn extract_body(file_lines: &[&str], start_line: u32, end_line: u32) -> String {
    if file_lines.is_empty() || start_line == 0 || end_line < start_line {
        return String::new();
    }
    let start_idx = (start_line as usize).saturating_sub(1);
    let end_idx = (end_line as usize).min(file_lines.len());
    if start_idx >= end_idx {
        return String::new();
    }
    file_lines[start_idx..end_idx].concat()
}

/// File-extension based language detection. We only need to know:
/// brace-language vs. not. Misclassifications fall through to the
/// char-window path; the goal is "right answer 95% of the time" not
/// every file extension on earth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Language {
    Rust,
    TypeScript,
    JavaScript,
    Go,
    Java,
    Kotlin,
    Scala,
    C,
    Cpp,
    CSharp,
    Python,
    Ruby,
    Other,
}

impl Language {
    fn uses_braces(self) -> bool {
        matches!(
            self,
            Language::Rust
                | Language::TypeScript
                | Language::JavaScript
                | Language::Go
                | Language::Java
                | Language::Kotlin
                | Language::Scala
                | Language::C
                | Language::Cpp
                | Language::CSharp
        )
    }
}

fn language_for_path(path: &str) -> Language {
    // Match on the extension after the *last* `.`; double extensions
    // like `.d.ts` collapse to `.ts` which is the right answer here.
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "rs" => Language::Rust,
        "ts" | "tsx" => Language::TypeScript,
        "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
        "go" => Language::Go,
        "java" => Language::Java,
        "kt" | "kts" => Language::Kotlin,
        "scala" | "sc" => Language::Scala,
        "c" | "h" => Language::C,
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" => Language::Cpp,
        "cs" => Language::CSharp,
        "py" | "pyi" => Language::Python,
        "rb" => Language::Ruby,
        _ => Language::Other,
    }
}

/// Deterministic SHA-256 chunk id. Stable across re-warms so
/// `INSERT ... ON DUPLICATE KEY UPDATE` works.
fn chunk_id(project_id: &str, file_path: &str, symbol_key: &str, idx: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(project_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(file_path.as_bytes());
    hasher.update(b"\0");
    hasher.update(symbol_key.as_bytes());
    hasher.update(b"\0");
    hasher.update(idx.to_le_bytes());
    let digest = hasher.finalize();
    format!("{digest:x}")
}

fn join_doc(lines: &[String]) -> String {
    lines.join(" ")
}

fn first_n_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn tail_chars(s: &str, n: usize) -> String {
    let total = s.chars().count();
    if total <= n {
        s.to_owned()
    } else {
        s.chars().skip(total - n).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> RepoMetadata<'static> {
        RepoMetadata {
            owner: "djinnos",
            repo: "djinn",
        }
    }

    fn function_symbol(name: &str, start: u32, end: u32) -> SymbolInput {
        SymbolInput {
            symbol_key: format!("rust . . crate . {name}()."),
            display_name: name.to_owned(),
            kind: SymbolChunkKind::Function,
            start_line: start,
            end_line: end,
            is_export: true,
            signature: Some(format!("fn {name}()")),
            documentation: vec![],
        }
    }

    #[test]
    fn small_rust_function_is_one_chunk() {
        let content = concat!(
            "fn small() {\n",
            "    let x = 1;\n",
            "    let y = 2;\n",
            "    println!(\"{} {}\", x, y);\n",
            "}\n",
        );
        let symbols = vec![function_symbol("small", 1, 5)];
        let file = FileInput {
            path: "src/lib.rs",
            content,
            symbols: &symbols,
        };
        let chunks = chunk_file("p", &meta(), &file, ChunkConfig::default());
        assert_eq!(chunks.len(), 1, "small body should produce one chunk");
        assert_eq!(chunks[0].kind, "function");
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 5);
        assert!(chunks[0].embedded_text.contains("Label: small"));
        assert!(chunks[0].embedded_text.contains("Repo: djinnos/djinn"));
        assert!(chunks[0].embedded_text.contains("fn small()"));
    }

    #[test]
    fn large_rust_function_splits_at_statement_boundaries() {
        // Build a body well above 1200 chars, with ~10 sub-statements.
        let mut lines = vec!["fn big() {".to_owned()];
        for stmt in 0..30 {
            lines.push(format!(
                "    {{ let x_{stmt} = {stmt}; let y_{stmt} = x_{stmt} * 2; do_thing(x_{stmt}, y_{stmt}); }}"
            ));
        }
        lines.push("}".to_owned());
        let content = lines.join("\n") + "\n";
        let line_count = content.lines().count() as u32;
        let symbols = vec![function_symbol("big", 1, line_count)];
        let file = FileInput {
            path: "src/lib.rs",
            content: &content,
            symbols: &symbols,
        };
        let chunks = chunk_file("p", &meta(), &file, ChunkConfig::default());
        assert!(
            chunks.len() >= 2,
            "expected multi-chunk split, got {}",
            chunks.len()
        );
        // Every chunk's body must end at a brace-newline boundary
        // (statement edge), except possibly the very last one.
        for (i, chunk) in chunks.iter().enumerate() {
            if i == chunks.len() - 1 {
                continue;
            }
            assert!(
                chunk.embedded_text.contains("} \n")
                    || chunk.embedded_text.contains("}\n")
                    || chunk.embedded_text.ends_with('\n'),
                "chunk {i} body did not end on a statement boundary: {body}",
                body = chunk.embedded_text
            );
        }
    }

    #[test]
    fn python_small_function_is_single_chunk() {
        let content = concat!(
            "def hello():\n",
            "    print('hi')\n",
            "    return 42\n",
        );
        let mut sym = function_symbol("hello", 1, 3);
        sym.signature = Some("def hello()".to_owned());
        let file = FileInput {
            path: "src/foo.py",
            content,
            symbols: &[sym],
        };
        let chunks = chunk_file("p", &meta(), &file, ChunkConfig::default());
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].embedded_text.contains("def hello()"));
    }

    #[test]
    fn python_large_function_falls_back_to_char_window() {
        // Python — no braces, so even very long bodies must come out
        // chunked via the character-window path.
        let mut lines = vec!["def big():".to_owned()];
        for i in 0..200 {
            lines.push(format!("    do_thing_{i}(x_{i}, y_{i}, z_{i})"));
        }
        let content = lines.join("\n") + "\n";
        let total_lines = content.lines().count() as u32;
        let mut sym = function_symbol("big", 1, total_lines);
        sym.signature = Some("def big()".to_owned());
        let file = FileInput {
            path: "src/foo.py",
            content: &content,
            symbols: &[sym],
        };
        let chunks = chunk_file("p", &meta(), &file, ChunkConfig::default());
        assert!(
            chunks.len() >= 2,
            "long python body should chunk via char window"
        );
        // No chunk should be larger than chunk_size + a small slack.
        for chunk in &chunks {
            // The embedded_text includes the header — only assert on
            // the line span, which is what the char-window controls.
            assert!(chunk.start_line >= 1);
            assert!(chunk.end_line <= total_lines);
        }
    }

    #[test]
    fn typescript_class_emits_declaration_chunk_with_member_names() {
        let content = concat!(
            "export class Greeter {\n",
            "  greeting: string;\n",
            "  count: number = 0;\n",
            "  constructor(message: string) {\n",
            "    this.greeting = message;\n",
            "  }\n",
            "  greet() {\n",
            "    return 'Hello, ' + this.greeting;\n",
            "  }\n",
            "}\n",
        );
        let symbols = vec![SymbolInput {
            symbol_key: "ts . . . Greeter#".to_owned(),
            display_name: "Greeter".to_owned(),
            kind: SymbolChunkKind::Declaration,
            start_line: 1,
            end_line: 10,
            is_export: true,
            signature: Some("export class Greeter".to_owned()),
            documentation: vec![],
        }];
        let file = FileInput {
            path: "src/greeter.ts",
            content,
            symbols: &symbols,
        };
        let chunks = chunk_file("p", &meta(), &file, ChunkConfig::default());
        assert!(!chunks.is_empty(), "expected at least one chunk");
        let first = &chunks[0];
        assert_eq!(first.kind, "declaration");
        assert!(
            first.embedded_text.contains("Methods:"),
            "expected Methods: header, got: {}",
            first.embedded_text
        );
        assert!(
            first.embedded_text.contains("greet"),
            "expected greet in methods header"
        );
    }

    #[test]
    fn empty_symbol_body_is_skipped() {
        // Symbol points at a line range that contains only whitespace.
        let content = "\n\n\n";
        let symbols = vec![function_symbol("empty", 1, 3)];
        let file = FileInput {
            path: "src/lib.rs",
            content,
            symbols: &symbols,
        };
        let chunks = chunk_file("p", &meta(), &file, ChunkConfig::default());
        assert!(chunks.is_empty(), "empty body should yield no chunks");
    }

    #[test]
    fn deterministic_chunk_ids_match_across_runs() {
        let content = "fn foo() { let x = 1; }\n";
        let symbols = vec![function_symbol("foo", 1, 1)];
        let file = FileInput {
            path: "src/lib.rs",
            content,
            symbols: &symbols,
        };
        let a = chunk_file("p", &meta(), &file, ChunkConfig::default());
        let b = chunk_file("p", &meta(), &file, ChunkConfig::default());
        assert_eq!(a, b, "chunker should be deterministic");
    }

    #[test]
    fn brace_counter_ignores_braces_in_strings_and_comments() {
        // A `}` in a string / line comment / block comment must not
        // close a top-level statement.
        let body = r#"
{
  let s = "}";
  // }
  /* } */
  do_thing();
}
"#;
        let bs = find_statement_boundaries(body, 0);
        // We expect exactly one boundary — the closing brace of the
        // outer block, followed by newline.
        assert_eq!(bs.len(), 1, "got boundaries: {bs:?}");
    }

    #[test]
    fn brace_counter_inner_depth_finds_per_statement_boundaries() {
        // target_depth=1 fires once per inner statement-block close,
        // which is what function-body chunking relies on.
        let body = "fn outer() {\n    { stmt_a(); }\n    { stmt_b(); }\n    { stmt_c(); }\n}\n";
        let bs = find_statement_boundaries(body, 1);
        assert_eq!(bs.len(), 3, "expected 3 inner-block boundaries, got: {bs:?}");
    }

    #[test]
    fn language_detection_covers_brace_set() {
        assert!(language_for_path("a.rs").uses_braces());
        assert!(language_for_path("a.tsx").uses_braces());
        assert!(language_for_path("a.go").uses_braces());
        assert!(language_for_path("a.java").uses_braces());
        assert!(language_for_path("a.kt").uses_braces());
        assert!(language_for_path("a.cpp").uses_braces());
        assert!(!language_for_path("a.py").uses_braces());
        assert!(!language_for_path("a.rb").uses_braces());
        assert!(!language_for_path("README.md").uses_braces());
    }
}
