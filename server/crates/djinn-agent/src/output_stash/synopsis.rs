// `synopsize` is called from `render_tool_result` in the parent module.
// `push_str` is reserved for future integration and kept with a targeted
// allow.

//! Deterministic bounded synopses for oversized tool-result payloads.
//!
//! Phase 1 of proposal `01ik` covers JSON, code, text/log, and binary no-op
//! classification. The function [`synopsize`] is the integration surface
//! called from [`super::render_tool_result`] for oversized payloads.
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
//!   and `suggested grep terms` appearing verbatim.
//! * **Budget enforcement.** When the would-be synopsis exceeds
//!   `budget_chars` characters, drop lower-priority fields in a deterministic
//!   order before truncating the last surviving bullet at a word boundary.
//!
//! Non-JSON inputs (text, code, binary, CSV, XML, YAML, …) return `None` in
//! this slice; later tasks extend the classifier without changing the contract.

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Max input bytes for JSON parse (rejects before deserializer allocates).
const MAX_PARSE_BYTES: usize = 1_048_576; // 1 MiB
/// Max JSON tokens inspected before returning `None`.
const MAX_JSON_TOKENS: usize = 200_000;
/// Max top-level keys enumerated; excess shown as `…(+N more)`.
const MAX_TOP_KEYS: usize = 16;
/// Max nested keys per parent for `object shape depth 2`.
const MAX_NESTED_KEYS: usize = 8;
/// Max scalar examples surfaced from the tree.
const MAX_SCALAR_EXAMPLES: usize = 6;
/// Max chars per scalar example; longer values truncated with `…`.
const MAX_SCALAR_EXAMPLE_CHARS: usize = 64;
/// Max chars per suggested grep term.
const MAX_GREP_TERM_CHARS: usize = 32;
/// Max suggested grep terms emitted.
const MAX_GREP_TERMS: usize = 6;

/// Max recursion depth when walking nested containers.
const MAX_WALK_DEPTH: usize = 2;
/// Hard cap on visited nodes during tree walk.
const MAX_WALK_NODES: usize = 4_096;
/// Max nesting depth for streaming scanner (mirrors serde_json default).
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

/// Bounded synopsis entry point. Returns `None` for binary, empty, or
/// too-small budgets. Classification: JSON -> binary -> code -> text -> `None`.
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
    if control_count.saturating_mul(100) / total >= BINARY_CONTROL_PCT_THRESHOLD {
        return true;
    }
    // Degenerate content: a single character repeated with no meaningful
    // variation (e.g. "xxxxx…").  Such content carries no structural signal
    // and should not receive a text synopsis.
    if total >= 100 {
        let mut counts = std::collections::HashMap::new();
        for c in text.chars() {
            *counts.entry(c).or_insert(0usize) += 1;
        }
        if let Some(&max_count) = counts.values().max()
            && max_count * 100 / total >= 99
        {
            return true;
        }
    }
    false
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
    #[allow(dead_code)] // Reserved for future integration.
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

/// Bounded parse: bail to `None` if input exceeds MAX_PARSE_BYTES or token
/// count exceeds MAX_JSON_TOKENS. Streaming scan runs before deserializer.
fn parse_bounded(text: &str) -> Option<serde_json::Value> {
    if text.len() > MAX_PARSE_BYTES {
        return None;
    }
    count_tokens_streaming(text)?;
    serde_json::from_str(text).ok()
}

/// Streaming structural scan: count JSON tokens without deserializing.
/// O(n) time, O(1) memory. Returns None if malformed or over limits.
fn count_tokens_streaming(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut tokens: usize = 0;
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut in_scalar_body = false;

    for &b in bytes {
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
        if in_scalar_body {
            if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-') {
                continue;
            }
            in_scalar_body = false;
        }

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
                    return None;
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

/// Budgeted builder for bullet-formatted synopsis. Fields committed in
/// fixed order for deterministic output; overflow tracked for `…(truncated)`.
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

    #[allow(dead_code)] // Reserved for future integration; not yet called.
    fn push_str(&mut self, s: &str) -> bool {
        if s.len() <= self.remaining() {
            self.out.push_str(s);
            true
        } else {
            self.overflowed = true;
            false
        }
    }

    /// Push a complete bullet line; returns false if it would exceed budget.
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
        // `kind` is always-on; if it doesn't fit, budget is too small.
        if !self.push_bullet("kind", kind) {
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

/// Stable sorted key list for deterministic synopsis output.
fn sorted_keys(map: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let mut keys: Vec<String> = map.keys().cloned().collect();
    keys.sort();
    keys
}

fn preview_keys(keys: &[String], max: usize) -> Vec<String> {
    keys.iter().take(max).cloned().collect()
}

/// Truncate string to `max_chars` Unicode chars, appending `…` if cut.
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

/// Collect bounded scalar samples from the tree.
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

/// Collect distinctive short strings from JSON for grep suggestions.
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
#[path = "synopsis_tests.rs"]
mod synopsis_tests;
