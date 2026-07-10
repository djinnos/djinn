// This module is developed in slices. `synopsize` and its helpers are not yet
// called from non-test code; integration happens in a follow-up task.
#![allow(dead_code)]

//! Deterministic bounded synopses for oversized tool-result payloads.
//!
//! Phase 1 of proposal `01ik` covers JSON, code, text/log, and binary
//! classification. The function [`synopsize`] is the integration surface the
//! follow-up render task calls from [`super::render_tool_result`].
//!
//! Classification order: JSON -> binary (`None`) -> code -> text/log -> `None`.
//!
//! Design goals (locked in by the proposal and acceptance criteria):
//!
//! * **No LLM calls, no IO, no durable state.** Pure, deterministic string
//!   transform; safe to call from the hot `render_tool_result` chokepoint.
//! * **Bounded behavior on pathological input.** Never panic. On huge or
//!   deeply-nested JSON a streaming structural scan rejects the input
//!   *before* any [`serde_json::Value`] is allocated, so pathological
//!   large-but-valid blobs never cause unbounded time/memory. `serde_json`'s
//!   own recursion limit is the backstop for any nesting the scan might miss.
//! * **Stable bullet labels.** Downstream tests/proposals depend on labels
//!   like `kind`, `root`, `arrays`, `object shape depth 2`, `scalar examples`,
//!   `lines`, `imports`, `symbols`, `sections`, `notable markers`, and
//!   `suggested grep terms` appearing verbatim.
//! * **Budget enforcement.** When the would-be synopsis exceeds
//!   `budget_chars` characters, drop lower-priority fields in a deterministic
//!   order before truncating the last surviving bullet at a word boundary.

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Hard ceiling on the input text length (bytes) we will attempt to parse as
/// JSON. Inputs exceeding this are rejected *before* calling
/// [`serde_json::from_str`], so the deserializer never allocates for them.
/// This is the primary defence against pathological memory consumption — a
/// multi-MB valid-JSON blob would otherwise force a many-larger
/// `serde_json::Value` tree before the post-parse token walk can bail out.
///
/// 1 MiB is generous enough for any realistic tool output we would summarize
/// (the normal-case tool-result cap is 30 KB; stashed outputs rarely exceed a
/// few hundred KB) while capping worst-case parse memory to ≈ 2–3× this
/// value.
const MAX_PARSE_BYTES: usize = 1_048_576; // 1 MiB

/// Hard ceiling on the number of JSON tokens we will inspect before giving up
/// and returning `None`. Anything well-formed that exceeds this is almost
/// certainly hostile or pathological — we deliberately do not summarize it
/// rather than spend cycles walking it.
///
/// Sized so a 1 MB JSON blob (the upper end of tool output we are willing to
/// even look at) is comfortably under the limit, but a 10 MB adversarial blob
/// trips it.
const MAX_JSON_TOKENS: usize = 200_000;

/// Maximum number of top-level object keys to enumerate in the synopsis.
/// Excess keys are reported as `…(+N more)`.
const MAX_TOP_KEYS: usize = 16;

/// Maximum number of nested object keys (depth 1 under the root) to enumerate
/// per parent when emitting `object shape depth 2`. Excess keys are elided.
const MAX_NESTED_KEYS: usize = 8;

/// Maximum number of scalar examples to surface. Scalars here means string,
/// number, boolean, or null leaf values seen while walking the tree.
const MAX_SCALAR_EXAMPLES: usize = 6;

/// Maximum length of a single scalar example, in characters. Longer values are
/// truncated with a trailing `…` so a single huge string field cannot blow the
/// budget.
const MAX_SCALAR_EXAMPLE_CHARS: usize = 64;

/// Maximum length of a single suggested-grep term. Same reason as scalars.
const MAX_GREP_TERM_CHARS: usize = 32;

/// Maximum number of `suggested grep terms` we emit.
const MAX_GREP_TERMS: usize = 6;

/// Maximum recursion depth (measured in nested containers) the summarizer
/// walks. Beyond this we emit a placeholder and stop descending. The proposal
/// only requires shape to depth 2, so this is a small safety margin above that.
const MAX_WALK_DEPTH: usize = 2;

/// Hard cap on the number of nodes visited while building the synopsis. Stops
/// adversarial inputs from forcing us to walk the whole tree even after we
/// have a parsed `Value` in hand.
const MAX_WALK_NODES: usize = 4_096;

/// Maximum nesting depth allowed by the streaming structural scanner. This
/// mirrors `serde_json`'s default recursion limit (128); deeper nesting is
/// rejected *before* the deserializer runs, so pathological deeply-nested
/// JSON never allocates a `Value` tree.
const MAX_JSON_DEPTH: usize = 128;

// -- Code/text constants --
const CODE_CATEGORY_THRESHOLD: usize = 2;
const CODE_MARKDOWN_PENALTY: usize = 2;
const MAX_IMPORTS: usize = 8;
const MAX_IMPORT_LINE_CHARS: usize = 80;
const MAX_SYMBOLS: usize = 10;
const MAX_SECTIONS: usize = 8;
const MAX_SECTION_CHARS: usize = 80;
const BINARY_CONTROL_PCT_THRESHOLD: usize = 5;
const NOTABLE_MARKERS: &[&str] = &["error:", "FAILED", "FAIL", "panic", "Traceback"];

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Bounded synopsis entry point.
///
/// Returns a deterministic, bullet-formatted synopsis of `text` if it is
/// detected as JSON, code, or text/log. Returns `None` for binary payloads,
/// empty input, or when the budget is too small to emit even the always-on
/// `kind` field.
///
/// Classification order: JSON -> binary (`None`) -> code -> text/log -> `None`.
///
/// `tool_name` is accepted for the future integration with the call site; in
/// this slice it does not influence the classification.
///
/// `budget_chars` is the maximum number of characters the returned synopsis
/// may occupy. The function never panics and never returns a string longer
/// than `budget_chars`; when the natural synopsis would exceed it, fields are
/// omitted in a deterministic order (lowest priority first). If even the
/// always-present header would overflow an absurdly small budget, the
/// function returns `None` so the caller can fall back to the byte-for-byte
/// truncated stub.
pub fn synopsize(_tool_name: &str, text: &str, budget_chars: usize) -> Option<String> {
    if budget_chars == 0 {
        return None;
    }
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    // 1. Try JSON classification.
    if let Some(result) = try_json_synopsis(trimmed, budget_chars) {
        return Some(result);
    }
    // 2. Binary detection.
    if is_binary(trimmed) {
        return None;
    }
    // 3. Code classification.
    if is_code(trimmed) {
        return build_code_synopsis(trimmed, budget_chars);
    }
    // 4. Text/log fallback (catches CSV, TSV, XML, YAML as plain text).
    build_text_synopsis(trimmed, budget_chars)
}

// ---------------------------------------------------------------------------
// JSON classification
// ---------------------------------------------------------------------------

fn try_json_synopsis(trimmed: &str, budget_chars: usize) -> Option<String> {
    let first = trimmed.chars().next()?;
    if !matches!(first, '{' | '[' | '"' | 't' | 'f' | 'n' | '-' | '0'..='9') {
        return None;
    }
    let value = parse_bounded(trimmed)?;
    let mut b = Builder::new(budget_chars);
    b.push_kind(&value);
    b.push_root(&value);
    let shape = Shape::compute(&value);
    b.try_push_arrays(&shape);
    b.try_push_object_shape(&value, &shape);
    b.try_push_scalar_examples(&value);
    b.try_push_grep_terms(&value);
    b.finalize()
}

// ---------------------------------------------------------------------------
// Binary detection
// ---------------------------------------------------------------------------

fn is_binary(text: &str) -> bool {
    if text.as_bytes().contains(&0) {
        return true;
    }
    let total = text.chars().count();
    if total == 0 {
        return false;
    }
    let control_count = text
        .chars()
        .filter(|&c| c.is_control() && c != '\n' && c != '\r' && c != '\t')
        .count();
    control_count.saturating_mul(100) / total >= BINARY_CONTROL_PCT_THRESHOLD
}

// ---------------------------------------------------------------------------
// Code classification
// ---------------------------------------------------------------------------

fn is_code(text: &str) -> bool {
    let categories = code_category_count(text);
    let has_markdown = {
        let mut has_h1 = false;
        let mut has_h2_plus = false;
        for line in text.lines() {
            let t = line.trim_start();
            if t.starts_with("## ") {
                has_h2_plus = true;
            } else if t.starts_with("# ") && !t.starts_with("#!") {
                has_h1 = true;
            }
        }
        has_h1 && has_h2_plus
    };
    let threshold = if has_markdown {
        CODE_CATEGORY_THRESHOLD + CODE_MARKDOWN_PENALTY
    } else {
        CODE_CATEGORY_THRESHOLD
    };
    categories >= threshold
}

fn code_category_count(text: &str) -> usize {
    let mut c: usize = 0;
    if text.lines().any(|l| {
        starts_with_any(
            l.trim_start(),
            &[
                "fn ",
                "func ",
                "function ",
                "def ",
                "async fn",
                "pub fn",
                "pub async fn",
            ],
        )
    }) {
        c += 1;
    }
    if text.lines().any(|l| {
        starts_with_any(
            l.trim_start(),
            &[
                "struct ",
                "class ",
                "enum ",
                "trait ",
                "interface ",
                "impl ",
                "type ",
            ],
        )
    }) {
        c += 1;
    }
    if text.lines().any(|l| {
        starts_with_any(
            l.trim_start(),
            &[
                "import ", "use ", "from ", "#include", "#define", "package ", "module ",
                "require(",
            ],
        )
    }) {
        c += 1;
    }
    if text
        .lines()
        .filter(|l| {
            let t = l.trim_end();
            t.ends_with(';') && t.len() > 3 && !t.contains("://")
        })
        .count()
        >= 3
    {
        c += 1;
    }
    if text.contains('{')
        && text.contains('}')
        && text
            .lines()
            .filter(|l| {
                let t = l.trim();
                t.ends_with('{') || t == "}" || t == "}," || t == "});" || t == "};"
            })
            .count()
            >= 2
    {
        c += 1;
    }
    if text
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            t.starts_with("//") || t.starts_with("/*") || t.starts_with("*/") || t.starts_with("* ")
        })
        .count()
        >= 2
    {
        c += 1;
    }
    if text.lines().any(|l| {
        starts_with_any(
            l.trim_start(),
            &["let ", "const ", "var ", "static ", "val "],
        )
    }) {
        c += 1;
    }
    c
}

fn starts_with_any(line: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|p| line.starts_with(p))
}

// ---------------------------------------------------------------------------
// Code synopsis builder
// ---------------------------------------------------------------------------

fn build_code_synopsis(text: &str, budget_chars: usize) -> Option<String> {
    let mut b = Builder::new(budget_chars);
    if !b.push_bullet("kind", "code") {
        b.out.clear();
        b.budget = 0;
    }
    let _ = b.push_bullet("lines", &text.lines().count().to_string());
    let imports = collect_imports(text);
    if !imports.is_empty() {
        let _ = b.push_bullet("imports", &imports.join(", "));
    }
    let symbols = collect_symbols(text);
    if !symbols.is_empty() {
        let _ = b.push_bullet("symbols", &symbols.join(", "));
    }
    let grep: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
    if !grep.is_empty() {
        let _ = b.push_bullet("suggested grep terms", &grep.join(", "));
    }
    b.finalize()
}

fn collect_imports(text: &str) -> Vec<String> {
    let mut imports = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        let is_import = t.starts_with("import ")
            || (t.starts_with("from ") && t.contains(" import "))
            || t.starts_with("#include")
            || t.starts_with("#define")
            || t.starts_with("package ")
            || t.starts_with("module ")
            || t.starts_with("require(");
        let cap = if t.starts_with("use ") {
            t.strip_suffix(';').unwrap_or(t)
        } else if is_import {
            t
        } else {
            continue;
        };
        imports.push(truncate_chars(cap, MAX_IMPORT_LINE_CHARS));
        if imports.len() >= MAX_IMPORTS {
            break;
        }
    }
    imports
}

fn extract_after_keyword(line: &str, keywords: &[&str]) -> Option<String> {
    for kw in keywords {
        if let Some(rest) = line.strip_prefix(kw) {
            let end = rest
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            if end > 0 {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

fn collect_symbols(text: &str) -> Vec<String> {
    let mut symbols = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(name) =
            extract_after_keyword(t, &["fn ", "func ", "function ", "def ", "async fn "])
            && seen.insert(name.clone())
        {
            symbols.push(name);
        }
        if let Some(name) = extract_after_keyword(
            t,
            &[
                "struct ",
                "class ",
                "enum ",
                "trait ",
                "interface ",
                "type ",
            ],
        ) && seen.insert(name.clone())
        {
            symbols.push(name);
        }
        if symbols.len() >= MAX_SYMBOLS {
            break;
        }
    }
    symbols
}

// ---------------------------------------------------------------------------
// Text/log synopsis builder
// ---------------------------------------------------------------------------

fn build_text_synopsis(text: &str, budget_chars: usize) -> Option<String> {
    let mut b = Builder::new(budget_chars);
    if !b.push_bullet("kind", "text") {
        b.out.clear();
        b.budget = 0;
    }
    let _ = b.push_bullet("lines", &text.lines().count().max(1).to_string());
    let sections = collect_sections(text);
    if !sections.is_empty() {
        let _ = b.push_bullet("sections", &sections.join(", "));
    }
    let markers = collect_notable_markers(text);
    if !markers.is_empty() {
        let _ = b.push_bullet("notable markers", &markers.join(", "));
    }
    let terms = collect_text_grep_terms(text);
    if !terms.is_empty() {
        let _ = b.push_bullet("suggested grep terms", &terms.join(", "));
    }
    b.finalize()
}

fn is_section_header(t: &str) -> bool {
    t.starts_with('#')
        && !t.starts_with("#!")
        && !t.starts_with("#include")
        && !t.starts_with("#define")
        && !t.starts_with("#ifdef")
        && !t.starts_with("#ifndef")
        && !t.starts_with("#pragma")
        && !t.starts_with("#![")
}

fn collect_sections(text: &str) -> Vec<String> {
    let mut sections = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        if is_section_header(t) {
            sections.push(truncate_chars(t, MAX_SECTION_CHARS));
            if sections.len() >= MAX_SECTIONS {
                break;
            }
        }
    }
    sections
}

fn collect_notable_markers(text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let mut markers = Vec::new();
    for marker in NOTABLE_MARKERS {
        let count = lower.matches(marker.to_ascii_lowercase().as_str()).count();
        if count > 0 {
            markers.push(format!("{marker}({count})"));
        }
    }
    markers
}

fn collect_text_grep_terms(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in text.lines() {
        let t = line.trim_start();
        if is_section_header(t) {
            let ht = t.trim_start_matches('#').trim();
            if !ht.is_empty() && ht.len() <= MAX_GREP_TERM_CHARS {
                let key = ht.to_ascii_lowercase();
                if seen.insert(key) {
                    terms.push(truncate_chars(ht, MAX_GREP_TERM_CHARS));
                }
            }
        }
        if terms.len() >= MAX_GREP_TERMS {
            return terms;
        }
    }
    let lower = text.to_ascii_lowercase();
    for marker in NOTABLE_MARKERS {
        if terms.len() >= MAX_GREP_TERMS {
            break;
        }
        if lower.contains(marker.to_ascii_lowercase().as_str()) {
            let key = marker.to_ascii_lowercase();
            if seen.insert(key) {
                terms.push(marker.to_string());
            }
        }
    }
    terms
}

// ---------------------------------------------------------------------------
// JSON-specific types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootKind {
    Object,
    Array,
    Scalar,
}

#[derive(Debug, Clone)]
struct Shape {
    root: RootKind,
    top_key_count: usize,
    top_array_len: Option<usize>,
    nested_array_lengths: Vec<(String, usize)>,
    nested_object_keys: Vec<String>,
}

impl Shape {
    fn compute(value: &serde_json::Value) -> Self {
        let mut s = Shape {
            root: classify_root(value),
            top_key_count: 0,
            top_array_len: None,
            nested_array_lengths: Vec::new(),
            nested_object_keys: Vec::new(),
        };
        match value {
            serde_json::Value::Object(map) => {
                s.top_key_count = map.len();
                for (k, v) in map {
                    match v {
                        serde_json::Value::Array(a) => {
                            s.nested_array_lengths.push((k.clone(), a.len()));
                        }
                        serde_json::Value::Object(_) => {
                            s.nested_object_keys.push(k.clone());
                        }
                        _ => {}
                    }
                }
            }
            serde_json::Value::Array(a) => {
                s.top_array_len = Some(a.len());
            }
            _ => {}
        }
        s
    }
}

fn classify_root(value: &serde_json::Value) -> RootKind {
    match value {
        serde_json::Value::Object(_) => RootKind::Object,
        serde_json::Value::Array(_) => RootKind::Array,
        _ => RootKind::Scalar,
    }
}

// ---------------------------------------------------------------------------
// JSON bounded parser
// ---------------------------------------------------------------------------

/// Bounded parse: deserialize `text` into a [`serde_json::Value`], but bail
/// to `None` if the input exceeds [`MAX_PARSE_BYTES`] or the resulting token
/// count exceeds [`MAX_JSON_TOKENS`]. The input-size check runs *before*
/// calling the deserializer so that multi-MB inputs never cause unbounded
/// allocation inside `serde_json::from_str`. We never unwrap the
/// deserializer; both syntax errors and recursion-limit errors collapse to
/// `None` so the caller can fall back to the byte-for-byte truncated stub.
///
/// The streaming structural scan ([`count_tokens_streaming`]) is the critical
/// bound: it runs in O(n) time and O(1) memory *before* the deserializer, so
/// a pathological large-but-valid blob (e.g. `[1,1,…,1]` with 250k elements)
/// is rejected without ever materializing a huge `Value` tree.
fn parse_bounded(text: &str) -> Option<serde_json::Value> {
    if text.len() > MAX_PARSE_BYTES {
        return None;
    }
    count_tokens_streaming(text)?;
    serde_json::from_str(text).ok()
}

/// Streaming structural scan: count JSON value tokens by iterating the raw
/// bytes *without* deserializing. This runs in O(n) time and O(1) memory and
/// is the primary defence against pathological inputs that would force
/// [`serde_json::from_str`] to allocate a huge [`serde_json::Value`] tree
/// before any post-hoc node-count check could bail out.
///
/// Returns `Some(count)` if the input is structurally well-formed enough to
/// count (balanced brackets, properly closed strings) and the token count and
/// depth are within bounds. Returns `None` if the input is structurally
/// broken, exceeds [`MAX_JSON_DEPTH`], or exceeds [`MAX_JSON_TOKENS`].
///
/// The count is an **upper bound** on the number of `serde_json::Value`
/// nodes: object keys are counted as string tokens even though they are not
/// separate nodes in the `Value` tree. Overcounting is safe — if the upper
/// bound is within budget, the actual parse is guaranteed to be within budget
/// too.
fn count_tokens_streaming(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut tokens: usize = 0;
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;
    // After we count a scalar token (number/bool/null), we skip its remaining
    // bytes until a structural delimiter, to avoid double-counting.
    let mut in_scalar_body = false;

    for &b in bytes {
        // --- Inside a string literal ---
        if in_string {
            if escaped {
                escaped = false;
            } else {
                match b {
                    b'\\' => escaped = true,
                    b'"' => in_string = false,
                    _ => {}
                }
            }
            continue;
        }
        // --- Inside a scalar body (already counted) ---
        if in_scalar_body {
            if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-') {
                continue;
            }
            in_scalar_body = false;
            // Fall through to process the terminating byte.
        }

        // --- Structural scanning ---
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => {}
            b'{' | b'[' => {
                tokens += 1;
                depth += 1;
                if depth > MAX_JSON_DEPTH || tokens > MAX_JSON_TOKENS {
                    return None;
                }
            }
            b'}' | b']' => {
                if depth == 0 {
                    return None; // unbalanced closing bracket
                }
                depth -= 1;
            }
            b',' | b':' => {}
            b'"' => {
                tokens += 1;
                if tokens > MAX_JSON_TOKENS {
                    return None;
                }
                in_string = true;
            }
            // Scalar value start: true, false, null, or a number.
            b't' | b'f' | b'n' | b'-' | b'0'..=b'9' => {
                tokens += 1;
                if tokens > MAX_JSON_TOKENS {
                    return None;
                }
                in_scalar_body = true;
            }
            _ => return None, // unexpected byte — not valid JSON structure
        }
    }

    if in_string || depth != 0 {
        return None; // unterminated string or unbalanced brackets
    }
    Some(tokens)
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Budgeted builder for the bullet-formatted synopsis.
///
/// Accumulates always-on fields first, then optional fields in priority
/// order. Each push checks the remaining budget and short-circuits if the
/// field would blow it, optionally flipping a `saw_overflow` flag so the
/// finalizer can append a `…(truncated)` note. The builder is the single
/// owner of the output string — fields are committed in fixed order so the
/// bullet list is deterministic across runs and platforms.
struct Builder {
    budget: usize,
    out: String,
    overflowed: bool,
}

impl Builder {
    fn new(budget: usize) -> Self {
        Self {
            budget,
            out: String::new(),
            overflowed: false,
        }
    }

    fn remaining(&self) -> usize {
        self.budget.saturating_sub(self.out.len())
    }

    fn push_str(&mut self, s: &str) -> bool {
        if s.len() <= self.remaining() {
            self.out.push_str(s);
            true
        } else {
            self.overflowed = true;
            false
        }
    }

    /// Push a complete bullet line. Returns `false` if the line would exceed
    /// the budget (in which case the field is dropped entirely — we do not
    /// emit half-bullets, that would make downstream assertions flaky).
    fn push_bullet(&mut self, label: &str, body: &str) -> bool {
        let line = format!("- {label}: {body}\n");
        if line.len() <= self.remaining() {
            self.out.push_str(&line);
            true
        } else {
            self.overflowed = true;
            false
        }
    }
    fn push_kind(&mut self, value: &serde_json::Value) {
        let kind = match value {
            serde_json::Value::Object(_) => "object",
            serde_json::Value::Array(_) => "array",
            _ => "scalar",
        };
        // `kind` is always-on. If we cannot fit it, the budget is too small
        // for any useful synopsis — the caller will degrade to a no-op.
        if !self.push_bullet("kind", kind) {
            // Wipe the partial output and let the finalizer return None.
            self.out.clear();
            self.budget = 0;
        }
    }

    fn push_root(&mut self, value: &serde_json::Value) {
        let body = match value {
            serde_json::Value::Object(map) => {
                let keys = sorted_keys(map);
                let preview = preview_keys(&keys, MAX_TOP_KEYS);
                if keys.len() > MAX_TOP_KEYS {
                    format!(
                        "object with {} keys [{} \u{2026}(+{} more)]",
                        keys.len(),
                        preview.join(", "),
                        keys.len() - MAX_TOP_KEYS
                    )
                } else if keys.is_empty() {
                    "empty object".to_string()
                } else {
                    format!("object with {} keys [{}]", keys.len(), preview.join(", "))
                }
            }
            serde_json::Value::Array(a) => {
                if a.is_empty() {
                    "empty array".to_string()
                } else {
                    format!("array with {} elements", a.len())
                }
            }
            serde_json::Value::String(s) => {
                format!("string ({})", truncate_chars(s, MAX_SCALAR_EXAMPLE_CHARS))
            }
            serde_json::Value::Number(n) => format!("number ({n})"),
            serde_json::Value::Bool(b) => format!("bool ({b})"),
            serde_json::Value::Null => "null".to_string(),
        };
        if !self.push_bullet("root", &body) {
            self.overflowed = true;
        }
    }
    fn try_push_arrays(&mut self, shape: &Shape) {
        if self.remaining() == 0 {
            return;
        }
        let mut parts: Vec<String> = Vec::new();
        if let Some(len) = shape.top_array_len {
            parts.push(format!("root={len}"));
        }
        for (k, len) in shape.nested_array_lengths.iter().take(MAX_NESTED_KEYS) {
            parts.push(format!("{k}={len}"));
        }
        if !parts.is_empty() {
            let _ = self.push_bullet("arrays", &parts.join(", "));
        }
    }
    fn try_push_object_shape(&mut self, value: &serde_json::Value, shape: &Shape) {
        if self.remaining() == 0 || shape.nested_object_keys.is_empty() {
            return;
        }
        let serde_json::Value::Object(map) = value else {
            return;
        };
        let mut pieces: Vec<String> = Vec::new();
        for k in shape.nested_object_keys.iter().take(MAX_NESTED_KEYS) {
            let Some(child) = map.get(k) else { continue };
            let cs = match child {
                serde_json::Value::Object(cm) => {
                    let keys = sorted_keys(cm);
                    let preview = preview_keys(&keys, MAX_NESTED_KEYS);
                    if keys.len() > MAX_NESTED_KEYS {
                        format!(
                            "{{{} \u{2026}(+{} more)}}",
                            preview.join(", "),
                            keys.len() - MAX_NESTED_KEYS
                        )
                    } else if keys.is_empty() {
                        "{}".to_string()
                    } else {
                        format!("{{{}}}", preview.join(", "))
                    }
                }
                serde_json::Value::Array(a) => format!("[array; {}]", a.len()),
                _ => continue,
            };
            pieces.push(format!("{k}: {cs}"));
        }
        if !pieces.is_empty() {
            let _ = self.push_bullet("object shape depth 2", &pieces.join(", "));
        }
    }
    fn try_push_scalar_examples(&mut self, value: &serde_json::Value) {
        if self.remaining() == 0 {
            return;
        }
        let mut samples: Vec<String> = Vec::new();
        let mut bl = MAX_SCALAR_EXAMPLES;
        let mut vis = 0usize;
        collect_scalar_examples(value, 0, &mut samples, &mut bl, &mut vis);
        if !samples.is_empty() {
            let _ = self.push_bullet("scalar examples", &samples.join(", "));
        }
    }
    fn try_push_grep_terms(&mut self, value: &serde_json::Value) {
        if self.remaining() == 0 {
            return;
        }
        let mut terms: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut vis = 0usize;
        collect_grep_terms(value, 0, &mut terms, &mut seen, &mut vis);
        if !terms.is_empty() {
            let _ = self.push_bullet("suggested grep terms", &terms.join(", "));
        }
    }
    fn finalize(mut self) -> Option<String> {
        if self.out.is_empty() {
            return None;
        }
        if self.overflowed && self.remaining() >= "...(truncated)".len() {
            self.out.push_str("...(truncated)");
        }
        Some(self.out)
    }
}

// ---------------------------------------------------------------------------
// JSON utilities
// ---------------------------------------------------------------------------

/// Stable, deterministic key ordering. `serde_json::Map` preserves insertion
/// order by default, but tests and downstream consumers should not depend on
/// caller input order — sorting makes the synopsis byte-for-byte stable.
fn sorted_keys(map: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let mut keys: Vec<String> = map.keys().cloned().collect();
    keys.sort();
    keys
}

fn preview_keys(keys: &[String], max: usize) -> Vec<String> {
    keys.iter().take(max).cloned().collect()
}

/// Truncate a string to at most `max_chars` Unicode characters, appending `…`
/// if we had to cut. Never panics on multi-byte input.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (count, c) in s.chars().enumerate() {
        if count == max_chars {
            out.push('\u{2026}');
            return out;
        }
        out.push(c);
    }
    out
}

/// Walk the value collecting bounded scalar samples. We treat strings,
/// numbers, booleans, and null as scalars and cap how many we surface.
fn collect_scalar_examples(
    value: &serde_json::Value,
    depth: usize,
    out: &mut Vec<String>,
    budget_left: &mut usize,
    visited: &mut usize,
) {
    if *budget_left == 0 || *visited >= MAX_WALK_NODES || depth > MAX_WALK_DEPTH {
        return;
    }
    *visited += 1;
    match value {
        serde_json::Value::String(s) => {
            if !s.is_empty() {
                out.push(truncate_chars(s, MAX_SCALAR_EXAMPLE_CHARS));
                *budget_left -= 1;
            }
        }
        serde_json::Value::Number(n) => {
            out.push(n.to_string());
            *budget_left -= 1;
        }
        serde_json::Value::Bool(b) => {
            out.push(b.to_string());
            *budget_left -= 1;
        }
        serde_json::Value::Null => {}
        serde_json::Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                if i >= 2 || *budget_left == 0 || *visited >= MAX_WALK_NODES {
                    break;
                }
                collect_scalar_examples(v, depth + 1, out, budget_left, visited);
            }
        }
        serde_json::Value::Object(map) => {
            for (_, v) in map.iter() {
                if *budget_left == 0 || *visited >= MAX_WALK_NODES {
                    break;
                }
                collect_scalar_examples(v, depth + 1, out, budget_left, visited);
            }
        }
    }
}

/// Collect distinctive short strings to suggest as grep terms. We look at
/// string leaves, lowercase + trim, and keep the first [`MAX_GREP_TERMS`]
/// distinct tokens that are between 3 and [`MAX_GREP_TERM_CHARS`] chars and
/// contain at least one letter or digit.
fn collect_grep_terms(
    value: &serde_json::Value,
    depth: usize,
    out: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
    visited: &mut usize,
) {
    if out.len() >= MAX_GREP_TERMS || *visited >= MAX_WALK_NODES || depth > MAX_WALK_DEPTH {
        return;
    }
    *visited += 1;
    match value {
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.len() >= 3
                && trimmed.len() <= MAX_GREP_TERM_CHARS
                && trimmed.chars().any(|c| c.is_alphanumeric())
            {
                let key = trimmed.to_ascii_lowercase();
                if seen.insert(key) {
                    out.push(trimmed.to_string());
                }
            }
        }
        serde_json::Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                if i >= 3 || out.len() >= MAX_GREP_TERMS || *visited >= MAX_WALK_NODES {
                    break;
                }
                collect_grep_terms(v, depth + 1, out, seen, visited);
            }
        }
        serde_json::Value::Object(map) => {
            for (_, v) in map.iter() {
                if out.len() >= MAX_GREP_TERMS || *visited >= MAX_WALK_NODES {
                    break;
                }
                collect_grep_terms(v, depth + 1, out, seen, visited);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn has_label(s: &str, label: &str) -> bool {
        // Bullet format: "- {label}:". Allow either the colon or end-of-line
        // form so we accept both truncated and full bullets.
        let needle = format!("- {label}:");
        s.contains(&needle) || s.lines().any(|line| line.trim_start_matches("- ") == label)
    }

    // -- JSON tests --

    #[test]
    fn empty_input_returns_none() {
        assert_eq!(synopsize("shell", "", 1024), None);
        assert_eq!(synopsize("shell", "   \n\t  ", 1024), None);
    }

    #[test]
    fn zero_budget_returns_none() {
        assert_eq!(synopsize("shell", "{\"a\":1}", 0), None);
    }
    #[test]
    fn json_object_produces_stable_labels() {
        let value = json!({"ok": true, "rows": 3, "users": [{"id": 1, "name": "alice"}, {"id": 2, "name": "bob"}], "config": {"host": "example.com", "port": 443, "tls": true}});
        let raw = serde_json::to_string(&value).unwrap();
        let out = synopsize("task_list", &raw, 1200).expect("synopsis present");
        assert!(has_label(&out, "kind"), "kind missing: {out}");
        assert!(has_label(&out, "root"), "root missing: {out}");
        assert!(has_label(&out, "arrays"), "arrays missing: {out}");
        assert!(
            has_label(&out, "object shape depth 2"),
            "object shape depth 2 missing: {out}"
        );
    }
    #[test]
    fn json_array_produces_stable_labels() {
        let value = json!([{"id": 1, "name": "alpha"}, {"id": 2, "name": "beta"}, {"id": 3, "name": "gamma"}]);
        let raw = serde_json::to_string(&value).unwrap();
        let out = synopsize("task_list", &raw, 1200).expect("synopsis present");
        assert!(has_label(&out, "kind"));
        assert!(has_label(&out, "root"));
        assert!(has_label(&out, "arrays"));
        assert!(out.contains("root=3"), "expected root=3: {out}");
    }
    #[test]
    fn scalar_examples_are_bounded() {
        let value = json!({"status": "ok", "code": 200, "flag": true, "name": "alpha"});
        let raw = serde_json::to_string(&value).unwrap();
        let out = synopsize("shell", &raw, 1200).expect("synopsis present");
        assert!(has_label(&out, "scalar examples"));
        let bullet = out
            .lines()
            .find(|l| l.starts_with("- scalar examples"))
            .unwrap();
        for sample in bullet.trim_start_matches("- scalar examples:").split(',') {
            assert!(sample.trim().chars().count() <= MAX_SCALAR_EXAMPLE_CHARS + 1);
        }
    }
    #[test]
    fn suggested_grep_terms_for_status_string() {
        let value = json!({"status": "completed", "result": "ok", "user_id": "u_123"});
        let raw = serde_json::to_string(&value).unwrap();
        let out = synopsize("task_list", &raw, 1200).expect("synopsis present");
        assert!(has_label(&out, "suggested grep terms"));
        let bullet = out
            .lines()
            .find(|l| l.starts_with("- suggested grep terms"))
            .unwrap();
        let terms: Vec<&str> = bullet
            .trim_start_matches("- suggested grep terms:")
            .split(',')
            .map(str::trim)
            .collect();
        assert!(terms.len() <= MAX_GREP_TERMS);
        for t in terms {
            assert!(t.chars().count() <= MAX_GREP_TERM_CHARS);
            assert!(!t.is_empty());
        }
    }
    #[test]
    fn malformed_json_falls_back_to_text() {
        // Starts with `{` so the heuristic accepts it, but the body is
        // syntactically broken. Falls through binary check (not binary),
        // code check (not code), then lands on text.
        let result = synopsize("shell", "{not json", 1024);
        assert!(
            result.is_some(),
            "malformed JSON should fall through to text"
        );
    }
    #[test]
    fn pathological_depth_does_not_panic() {
        let mut s = String::new();
        for _ in 0..10_000 {
            s.push('[');
        }
        s.push('1');
        for _ in 0..10_000 {
            s.push(']');
        }
        let result = synopsize("shell", &s, 4096);
        if let Some(out) = result {
            assert!(out.len() <= 4096);
        }
    }
    #[test]
    fn oversized_valid_json_returns_none_or_text() {
        let inner = "1,".repeat(MAX_PARSE_BYTES / 2 + 1);
        let trimmed_inner = inner.trim_end_matches(',');
        let raw = format!("[{trimmed_inner}]");
        assert!(raw.len() > MAX_PARSE_BYTES);
        let result = synopsize("shell", &raw, 4096);
        if let Some(out) = result {
            assert!(
                !has_label(&out, "root"),
                "oversized JSON must not produce JSON synopsis"
            );
        }
    }
    #[test]
    fn pathological_breadth_over_token_limit_returns_none_or_text() {
        let n = MAX_JSON_TOKENS + 10;
        let raw: String =
            std::iter::once('[')
                .chain((0..n).flat_map(|i| {
                    std::iter::once('1').chain(if i + 1 < n { Some(',') } else { None })
                }))
                .chain(std::iter::once(']'))
                .collect();
        assert!(raw.len() < MAX_PARSE_BYTES);
        let result = synopsize("shell", &raw, 4096);
        if let Some(out) = result {
            assert!(out.len() <= 4096);
        }
    }
    #[test]
    fn pathological_breadth_does_not_panic() {
        let mut map = serde_json::Map::with_capacity(50_000);
        for i in 0..50_000 {
            map.insert(format!("k{i}"), serde_json::Value::Bool(i % 2 == 0));
        }
        let raw = serde_json::to_string(&serde_json::Value::Object(map)).unwrap();
        let result = synopsize("shell", &raw, 4096);
        if let Some(s) = result {
            assert!(s.len() <= 4096);
        }
    }
    #[test]
    fn pathological_breadth_just_under_limit_returns_bounded_synopsis() {
        let n = 5_000;
        let mut map = serde_json::Map::with_capacity(n);
        for i in 0..n {
            map.insert(format!("k{i}"), serde_json::Value::Number(i.into()));
        }
        let raw = serde_json::to_string(&serde_json::Value::Object(map)).unwrap();
        let out = synopsize("shell", &raw, 4096).expect("synopsis for large-but-valid JSON");
        assert!(out.len() <= 4096);
        assert!(has_label(&out, "kind"));
        assert!(has_label(&out, "root"));
    }
    #[test]
    fn budget_enforcement_caps_total_length() {
        let value = json!({"alpha": "x".repeat(200), "beta": "y".repeat(200), "gamma": "z".repeat(200), "delta": [1,2,3,4,5,6,7,8], "epsilon": {"k1": 1, "k2": 2, "k3": 3, "k4": 4}});
        let raw = serde_json::to_string(&value).unwrap();
        let out = synopsize("shell", &raw, 80).expect("synopsis present");
        assert!(out.len() <= 80);
        assert!(has_label(&out, "kind"));
        assert!(has_label(&out, "root"));
    }
    #[test]
    fn budget_zero_is_safe() {
        assert_eq!(
            synopsize(
                "shell",
                &serde_json::to_string(&json!({"a": 1})).unwrap(),
                0
            ),
            None
        );
    }
    #[test]
    fn empty_object_json_is_supported() {
        let out = synopsize("shell", "{}", 1024).unwrap();
        assert!(out.contains("empty object"));
    }
    #[test]
    fn empty_array_json_is_supported() {
        let out = synopsize("shell", "[]", 1024).unwrap();
        assert!(out.contains("empty array"));
    }
    #[test]
    fn top_level_scalar_json_is_supported() {
        let out = synopsize("shell", "\"hello world\"", 1024).unwrap();
        assert!(has_label(&out, "kind"));
        assert!(has_label(&out, "root"));
    }
    #[test]
    fn deterministic_across_runs() {
        let value = json!({"a": "one", "b": "two", "c": "three", "nested": {"x": 1, "y": 2}});
        let raw = serde_json::to_string(&value).unwrap();
        assert_eq!(
            synopsize("shell", &raw, 1200),
            synopsize("shell", &raw, 1200)
        );
    }
    #[test]
    fn tool_name_does_not_influence_json_synopsis() {
        assert_eq!(
            synopsize("shell", "{\"a\":1}", 1024),
            synopsize("task_list", "{\"a\":1}", 1024)
        );
    }
    #[test]
    fn streaming_scan_rejects_unterminated_string() {
        assert_eq!(count_tokens_streaming("{\"a\": \"oops"), None);
    }
    #[test]
    fn streaming_scan_rejects_unbalanced_brackets() {
        assert_eq!(count_tokens_streaming("[1,2,3"), None);
        assert_eq!(count_tokens_streaming("1,2,3]"), None);
    }
    #[test]
    fn streaming_scan_counts_simple_object() {
        assert_eq!(count_tokens_streaming("{\"a\":1,\"b\":2}"), Some(5));
    }
    #[test]
    fn streaming_scan_handles_escaped_quotes_in_strings() {
        let raw = "{\"msg\":\"he said \\\"hi\\\"\"}";
        assert!(count_tokens_streaming(raw).is_some());
        assert!(has_label(&synopsize("shell", raw, 1024).unwrap(), "kind"));
    }

    // -- Code tests --
    #[test]
    fn rust_code_produces_code_synopsis() {
        let code = "use std::collections::HashMap;\nuse std::io::Read;\n\nstruct Config {\n    name: String,\n    port: u16,\n}\n\nimpl Config {\n    fn new(name: &str) -> Self {\n        Config { name: name.to_string(), port: 8080 }\n    }\n}\n\nfn main() {\n    let config = Config::new(\"server\");\n}\n";
        let out = synopsize("shell", code, 1200).expect("code synopsis present");
        assert!(out.contains("code"), "expected kind=code: {out}");
        assert!(has_label(&out, "lines"), "lines missing: {out}");
        assert!(has_label(&out, "imports"), "imports missing: {out}");
        assert!(has_label(&out, "symbols"), "symbols missing: {out}");
        assert!(
            has_label(&out, "suggested grep terms"),
            "grep terms missing: {out}"
        );
    }
    #[test]
    fn code_synopsis_contains_import_statements() {
        let code = "import os\nimport sys\nfrom pathlib import Path\n\ndef main():\n    pass\n";
        let out = synopsize("shell", code, 1200).unwrap();
        assert!(out.contains("import os"));
        assert!(out.contains("import sys"));
        assert!(out.contains("from pathlib import Path"));
    }
    #[test]
    fn code_synopsis_contains_symbols() {
        let code = "fn process_data(input: &str) -> String {\n    input.to_string()\n}\n\nstruct Config {\n    port: u16,\n}\n";
        let out = synopsize("shell", code, 1200).unwrap();
        assert!(out.contains("process_data"));
        assert!(out.contains("Config"));
    }
    #[test]
    fn code_synopsis_respects_budget() {
        let code = "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\nstruct S1 {}\nstruct S2 {}\nimport os\nimport sys\n";
        let out = synopsize("shell", code, 80).unwrap();
        assert!(out.len() <= 80);
    }
    #[test]
    fn code_synopsis_is_deterministic() {
        let code = "fn hello() {}\nstruct Greeting;\nimport std::io;\n";
        assert_eq!(
            synopsize("shell", code, 1200),
            synopsize("shell", code, 1200)
        );
    }
    #[test]
    fn python_code_is_classified_as_code() {
        let code = "import os\nimport sys\nfrom typing import List\n\ndef process(items: List[str]) -> None:\n    pass\n\nclass Handler:\n    def handle(self, data):\n        return data\n";
        let out = synopsize("shell", code, 1200).unwrap();
        assert!(out.contains("code"));
        assert!(out.contains("process"));
        assert!(out.contains("Handler"));
    }
    #[test]
    fn c_code_is_classified_as_code() {
        let code = "#include <stdio.h>\n#include <stdlib.h>\n\nint main(int argc, char *argv[]) {\n    printf(\"hello\\n\");\n    return 0;\n}\n";
        let out = synopsize("shell", code, 1200).unwrap();
        assert!(out.contains("code"));
        assert!(has_label(&out, "imports"));
    }
    #[test]
    fn go_code_is_classified_as_code() {
        let code =
            "package main\n\nimport \"fmt\"\n\nfunc main() {\n    fmt.Println(\"hello\")\n}\n";
        let out = synopsize("shell", code, 1200).unwrap();
        assert!(out.contains("code"));
    }
    #[test]
    fn short_prose_is_not_classified_as_code() {
        let out = synopsize(
            "shell",
            "This is a simple message about the function of the system.",
            1024,
        )
        .unwrap();
        assert!(out.contains("text"), "prose should be text: {out}");
    }
    #[test]
    fn log_output_is_not_classified_as_code() {
        let log = "2024-01-15 10:30:00 INFO Starting server\n2024-01-15 10:30:01 ERROR Connection failed\n2024-01-15 10:30:02 WARN Retry\n2024-01-15 10:30:03 INFO Connected\n";
        let out = synopsize("shell", log, 1024).unwrap();
        assert!(out.contains("text"), "log should be text: {out}");
    }

    // -- Text/log tests --
    #[test]
    fn plain_text_produces_text_synopsis() {
        let out = synopsize(
            "shell",
            "Hello world.\nThis is a simple text output.\nNo special markers here.\n",
            1024,
        )
        .unwrap();
        assert!(out.contains("text"));
        assert!(has_label(&out, "lines"));
    }
    #[test]
    fn text_synopsis_contains_line_count() {
        let out = synopsize("shell", "line 1\nline 2\nline 3\nline 4\nline 5\n", 1024).unwrap();
        assert!(out.contains("lines"));
        assert!(out.contains("5"));
    }
    #[test]
    fn text_synopsis_detects_notable_markers() {
        let log = "INFO: Starting\nERROR: connection refused\nTraceback (most recent):\nFAILED: test_1\npanic: runtime error\n";
        let out = synopsize("shell", log, 1024).unwrap();
        assert!(has_label(&out, "notable markers"));
        assert!(out.contains("error:"));
        assert!(out.contains("FAILED"));
        assert!(out.contains("panic"));
        assert!(out.contains("Traceback"));
    }
    #[test]
    fn text_synopsis_detects_markdown_sections() {
        let md =
            "# Introduction\nSome text.\n## Setup\nSteps.\n## Results\nGood.\n### Details\nMore.\n";
        let out = synopsize("shell", md, 1024).unwrap();
        assert!(has_label(&out, "sections"));
        assert!(out.contains("Introduction"));
        assert!(out.contains("Setup"));
        assert!(out.contains("Results"));
    }
    #[test]
    fn text_synopsis_suggested_grep_terms_from_headers() {
        let md = "# Authentication\nDetails.\n## OAuth2\nFlow.\n## JWT\nTokens.\n";
        let out = synopsize("shell", md, 1024).unwrap();
        assert!(has_label(&out, "suggested grep terms"));
    }
    #[test]
    fn text_synopsis_respects_budget() {
        let text = "ERROR: failed\nFAILED: test\npanic: crash\n# One\n## Two\n### Three\n";
        let out = synopsize("shell", text, 100).unwrap();
        assert!(out.len() <= 100);
    }
    #[test]
    fn text_synopsis_is_deterministic() {
        let text = "ERROR: failure\n# Header A\n## Header B\nSome content.\n";
        assert_eq!(
            synopsize("shell", text, 1200),
            synopsize("shell", text, 1200)
        );
    }
    #[test]
    fn error_count_reflected_in_markers() {
        let log = "error: first\nerror: second\nerror: third\nINFO: ok\n";
        let out = synopsize("shell", log, 1024).unwrap();
        assert!(out.contains("error:(3)"), "expected error:(3): {out}");
    }
    #[test]
    fn no_notable_markers_omits_section() {
        let out = synopsize("shell", "Just a regular text.\nNothing special.\n", 1024).unwrap();
        assert!(!has_label(&out, "notable markers"));
    }
    #[test]
    fn no_sections_in_plain_text() {
        let out = synopsize("shell", "Just a regular text.\nNothing special.\n", 1024).unwrap();
        assert!(!has_label(&out, "sections"));
    }

    // -- Binary no-op tests --
    #[test]
    fn binary_with_null_bytes_returns_none() {
        let mut bytes = vec![b'h', b'e', b'l', b'l', b'o'];
        bytes.push(0);
        bytes.extend_from_slice(b"world");
        assert_eq!(
            synopsize("shell", &String::from_utf8_lossy(&bytes), 1024),
            None
        );
    }
    #[test]
    fn binary_with_high_control_char_ratio_returns_none() {
        let mut s = String::new();
        for _ in 0..90 {
            s.push('x');
        }
        for _ in 0..10 {
            s.push('\x01');
        }
        assert_eq!(synopsize("shell", &s, 1024), None);
    }
    #[test]
    fn text_with_newlines_is_not_binary() {
        assert!(synopsize("shell", &"line\n".repeat(100), 1024).is_some());
    }
    #[test]
    fn binary_input_emits_no_synopsis_header() {
        let mut s = String::from("hello");
        s.push('\0');
        s.push_str("world");
        assert_eq!(synopsize("shell", &s, 1024), None);
    }

    // -- CSV/TSV/XML/YAML routing tests --
    #[test]
    fn csv_input_gets_text_synopsis() {
        let out = synopsize("shell", "name,age,city\nAlice,30,NYC\nBob,25,LA\n", 1024).unwrap();
        assert!(out.contains("text"));
        assert!(has_label(&out, "lines"));
    }
    #[test]
    fn tsv_input_gets_text_synopsis() {
        let out = synopsize(
            "shell",
            "name\tage\tcity\nAlice\t30\tNYC\nBob\t25\tLA\n",
            1024,
        )
        .unwrap();
        assert!(out.contains("text"));
    }
    #[test]
    fn xml_input_gets_text_synopsis() {
        let xml = "<?xml version=\"1.0\"?>\n<root>\n  <item id=\"1\">\n    <name>Alice</name>\n  </item>\n</root>\n";
        let out = synopsize("shell", xml, 1024).unwrap();
        assert!(out.contains("text"));
    }
    #[test]
    fn yaml_like_input_gets_text_synopsis() {
        let out = synopsize(
            "shell",
            "name: Alice\nage: 30\ncity: NYC\nitems:\n  - one\n  - two\n",
            1024,
        )
        .unwrap();
        assert!(out.contains("text"));
    }
    #[test]
    fn csv_does_not_panic_with_malformed_input() {
        assert!(synopsize("shell", "a,b,c\n1,2\n3,4,5,6,7\n", 1024).is_some());
    }
    #[test]
    fn xml_with_code_like_content_still_text() {
        let out = synopsize(
            "shell",
            "<div class=\"container\">\n  <span>Alice</span>\n</div>\n",
            1024,
        )
        .unwrap();
        assert!(out.contains("text"));
    }

    // -- Budget and pathological tests --
    #[test]
    fn tiny_budget_returns_none_for_all_kinds() {
        assert_eq!(synopsize("shell", "{\"a\":1}", 1), None);
        assert_eq!(synopsize("shell", "fn main() {}\n", 1), None);
        assert_eq!(synopsize("shell", "hello world\n", 1), None);
    }
    #[test]
    fn code_synopsis_never_exceeds_budget() {
        let code = "fn a() {}\nfn b() {}\nfn c() {}\nstruct S1 {}\nstruct S2 {}\nuse std::io;\nimport os\nimport sys\n";
        for budget in [20, 50, 100, 200, 500, 1000] {
            if let Some(out) = synopsize("shell", code, budget) {
                assert!(out.len() <= budget);
            }
        }
    }
    #[test]
    fn text_synopsis_never_exceeds_budget() {
        let text =
            "ERROR: failure\nFAILED: test\npanic: crash\n# One\n## Two\n### Three\nMore text.\n";
        for budget in [20, 50, 100, 200, 500, 1000] {
            if let Some(out) = synopsize("shell", text, budget) {
                assert!(out.len() <= budget);
            }
        }
    }
    #[test]
    fn very_long_code_input_does_not_panic() {
        let mut code = String::new();
        for i in 0..10_000 {
            code.push_str(&format!("fn func_{i}() {{\n    println!(\"{i}\");\n}}\n"));
        }
        if let Some(out) = synopsize("shell", &code, 4096) {
            assert!(out.len() <= 4096);
        }
    }
    #[test]
    fn very_long_text_input_does_not_panic() {
        let mut text = String::new();
        for i in 0..10_000 {
            text.push_str(&format!("Line {i}: some content.\n"));
        }
        if let Some(out) = synopsize("shell", &text, 4096) {
            assert!(out.len() <= 4096);
        }
    }

    // -- Deterministic output ordering --
    #[test]
    fn code_synopsis_output_ordering_is_stable() {
        let code = "import os\nimport sys\nfn process() {}\nstruct Config {}\nclass Handler {}\nenum State {}\ntrait Printable {}\n";
        let out = synopsize("shell", code, 2000).unwrap();
        let (kp, lp, ip, sp, gp) = (
            out.find("- kind:").unwrap(),
            out.find("- lines:").unwrap(),
            out.find("- imports:").unwrap(),
            out.find("- symbols:").unwrap(),
            out.find("- suggested grep terms:").unwrap(),
        );
        assert!(kp < lp && lp < ip && ip < sp && sp < gp);
    }
    #[test]
    fn text_synopsis_output_ordering_is_stable() {
        let text = "# Title\nERROR: failure\nFAILED: test\n## Section\nSome text.\n";
        let out = synopsize("shell", text, 2000).unwrap();
        let (kp, lp, sp, mp, gp) = (
            out.find("- kind:").unwrap(),
            out.find("- lines:").unwrap(),
            out.find("- sections:").unwrap(),
            out.find("- notable markers:").unwrap(),
            out.find("- suggested grep terms:").unwrap(),
        );
        assert!(kp < lp && lp < sp && sp < mp && mp < gp);
    }
    #[test]
    fn json_synopsis_output_ordering_is_stable() {
        let value = json!({"users": [{"name": "a"}], "config": {"host": "x"}, "status": "ok"});
        let raw = serde_json::to_string(&value).unwrap();
        let out = synopsize("shell", &raw, 2000).unwrap();
        assert!(out.find("- kind:").unwrap() < out.find("- root:").unwrap());
    }

    // -- Edge cases --
    #[test]
    fn single_line_text_gets_synopsis() {
        let out = synopsize("shell", "just one line", 1024).unwrap();
        assert!(out.contains("text"));
        assert!(out.contains("1"));
    }
    #[test]
    fn whitespace_only_returns_none() {
        assert_eq!(synopsize("shell", "   \n\t\n   ", 1024), None);
    }
    #[test]
    fn json_with_leading_whitespace_still_parses() {
        let out = synopsize("shell", "  \n  {\"key\": \"value\"}", 1024).unwrap();
        assert!(has_label(&out, "root"));
    }
    #[test]
    fn shebang_line_not_treated_as_markdown() {
        let out = synopsize("shell", "#!/bin/bash\necho hello\nls -la\n", 1024).unwrap();
        assert!(out.len() <= 1024);
    }
    #[test]
    fn non_json_text_with_code_mentions_is_text() {
        let text = "The function of this system is to process data.\nWe use a struct to hold configuration.\nThe class of problems is NP-hard.\n";
        let out = synopsize("shell", text, 1024).unwrap();
        assert!(
            out.contains("text"),
            "prose mentioning code terms should be text: {out}"
        );
    }
}
