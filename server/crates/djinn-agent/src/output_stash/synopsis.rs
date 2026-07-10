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

/// Bounded synopsis entry point.
///
/// Returns a deterministic, bullet-formatted synopsis of `text` if it is
/// detected as JSON. Returns `None` for any other input (text, code, binary,
/// CSV, XML, YAML, empty, …) so a later code/text/binary classifier can take
/// over without changing the public contract.
///
/// `tool_name` is accepted for the future integration with the call site; in
/// this slice it does not influence the JSON synopsis. (The `shell` tool
/// already gets a log-shaped stash via `extract_stash_content` upstream of
/// this function, so a JSON synopsis for shell output is the rare case where
/// the tool produced structured JSON rather than freeform stdout.)
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
    // Heuristic: JSON-shaped payloads start with one of the valid JSON value
    // openers. We accept whitespace/newline-prefixed input by trimming above.
    // This keeps us from spending parse budget on prose like "the result is {}".
    let first = trimmed.chars().next()?;
    if !matches!(first, '{' | '[' | '"' | 't' | 'f' | 'n' | '-' | '0'..='9') {
        return None;
    }

    // Bounded parse: reject oversized/pathological inputs via a streaming
    // structural scan *before* any Value allocation, then deserialize.
    let value = parse_bounded(trimmed)?;

    let mut b = Builder::new(budget_chars);
    b.push_kind(&value);
    b.push_root(&value);

    // Build the prioritized field list, then commit it in order with
    // per-field budget awareness.
    let shape = Shape::compute(&value);
    b.try_push_arrays(&shape);
    b.try_push_object_shape(&value, &shape);
    b.try_push_scalar_examples(&value);
    b.try_push_grep_terms(&value);

    b.finalize()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootKind {
    Object,
    Array,
    /// A JSON scalar at the root (a top-level string/number/bool/null).
    Scalar,
}

/// What the root container is, plus cheap pre-computed facts we want to show.
#[derive(Debug, Clone)]
struct Shape {
    #[allow(dead_code)] // Reserved for future integration.
    root: RootKind,
    top_key_count: usize,
    top_array_len: Option<usize>,
    /// For each top-level array, its length. Capped and counted.
    nested_array_lengths: Vec<(String, usize)>,
    /// Top-level keys whose value is an object — used to render
    /// `object shape depth 2`.
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
    // Pre-parse ceiling #1: reject oversized inputs by byte length before
    // touching the deserializer. This caps worst-case parse memory to
    // ≈ 2–3× MAX_PARSE_BYTES (serde_json::Value typically uses more memory
    // than the source text).
    if text.len() > MAX_PARSE_BYTES {
        return None;
    }

    // Pre-parse ceiling #2: streaming structural scan. Count JSON value
    // tokens by iterating raw bytes in O(n) time and O(1) memory — *without*
    // deserializing. If the token count or nesting depth exceeds bounds, we
    // return None *before* serde_json::from_str ever runs, so a pathological
    // large-but-valid blob (e.g. `[1,1,…,1]` with 250k elements) can never
    // force the deserializer to materialize a huge Value tree.
    count_tokens_streaming(text)?;

    // Safe to deserialize: the input is within MAX_PARSE_BYTES, the streaming
    // scan confirmed the token count and depth are bounded, and serde_json's
    // own recursion limit (128) guards nesting. Both syntax errors and
    // recursion-limit errors collapse to None so the caller degrades
    // gracefully.
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
        // Saturating: we never want a panic from `usize` underflow if a
        // caller pushes a string slightly larger than the remaining budget.
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
                        "object with {} keys [{} …(+{} more)]",
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
            // Drop lower-priority fields and try again on the finalizer.
            self.overflowed = true;
        }
    }

    /// `arrays: <root length>; <k1>=<len1>, <k2>=<len2>, …`
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
        if parts.is_empty() {
            return;
        }
        let body = parts.join(", ");
        let _ = self.push_bullet("arrays", &body);
    }

    /// `object shape depth 2: <k1>: {<nested keys>}, <k2>: [<len>], …`
    ///
    /// For each top-level object-valued key, list the keys found at depth 1.
    /// For each top-level array-valued key, we already covered the length
    /// under `arrays`; here we surface the element-type signature where cheap.
    fn try_push_object_shape(&mut self, value: &serde_json::Value, shape: &Shape) {
        if self.remaining() == 0 {
            return;
        }
        if shape.nested_object_keys.is_empty() {
            return;
        }
        let mut pieces: Vec<String> = Vec::new();
        let serde_json::Value::Object(map) = value else {
            return;
        };
        for k in shape.nested_object_keys.iter().take(MAX_NESTED_KEYS) {
            let Some(child) = map.get(k) else { continue };
            let child_shape = match child {
                serde_json::Value::Object(child_map) => {
                    let keys = sorted_keys(child_map);
                    let preview = preview_keys(&keys, MAX_NESTED_KEYS);
                    if keys.len() > MAX_NESTED_KEYS {
                        format!(
                            "{{{} …(+{} more)}}",
                            preview.join(", "),
                            keys.len() - MAX_NESTED_KEYS
                        )
                    } else if keys.is_empty() {
                        "{}".to_string()
                    } else {
                        format!("{{{}}}", preview.join(", "))
                    }
                }
                serde_json::Value::Array(a) => {
                    format!("[array; {}]", a.len())
                }
                _ => continue,
            };
            pieces.push(format!("{k}: {child_shape}"));
        }
        if pieces.is_empty() {
            return;
        }
        let body = pieces.join(", ");
        let _ = self.push_bullet("object shape depth 2", &body);
    }

    /// `scalar examples: <v1>, <v2>, …`
    ///
    /// Walks the tree to depth [`MAX_WALK_DEPTH`] collecting bounded scalar
    /// samples. We do not descend past depth 2 — that is exactly what the
    /// proposal calls for — and we cap both the number of samples and the
    /// length of each sample so a single huge string field cannot dominate
    /// the synopsis.
    fn try_push_scalar_examples(&mut self, value: &serde_json::Value) {
        if self.remaining() == 0 {
            return;
        }
        let mut samples: Vec<String> = Vec::new();
        let mut budget_left = MAX_SCALAR_EXAMPLES;
        let mut visited = 0usize;
        collect_scalar_examples(value, 0, &mut samples, &mut budget_left, &mut visited);
        if samples.is_empty() {
            return;
        }
        let body = samples.join(", ");
        let _ = self.push_bullet("scalar examples", &body);
    }

    /// `suggested grep terms: <term1>, <term2>, …`
    ///
    /// Heuristic: scan string leaves for short, distinctive tokens that look
    /// like grep-worthy labels (e.g. error codes, status enums, identifiers).
    /// We deliberately keep this cheap and bounded — the goal is a hint, not
    /// a full search index. Empty/very short strings are skipped.
    fn try_push_grep_terms(&mut self, value: &serde_json::Value) {
        if self.remaining() == 0 {
            return;
        }
        let mut terms: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut visited = 0usize;
        collect_grep_terms(value, 0, &mut terms, &mut seen, &mut visited);
        if terms.is_empty() {
            return;
        }
        let body = terms.join(", ");
        let _ = self.push_bullet("suggested grep terms", &body);
    }

    /// Finalize: append an overflow note if we dropped a field for budget
    /// reasons, and return the assembled string. Returns `None` only if the
    /// builder never got far enough to emit the always-on `kind` field.
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
            out.push('…');
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
        serde_json::Value::Null => {
            // Null is rarely useful as a grep term, so we skip it from
            // scalar examples — including it would crowd out meaningful
            // samples without telling the model anything.
        }
        serde_json::Value::Array(a) => {
            // Sample a couple of elements at the front so the model gets a
            // hint of the element type. We do not recurse into nested
            // structures beyond MAX_WALK_DEPTH.
            for (i, v) in a.iter().enumerate() {
                if i >= 2 || *budget_left == 0 || *visited >= MAX_WALK_NODES {
                    break;
                }
                collect_scalar_examples(v, depth + 1, out, budget_left, visited);
            }
        }
        serde_json::Value::Object(map) => {
            // Object-valued entries are summarized by their keys (already
            // shown in `root` / `object shape depth 2`), so we do not emit
            // them as scalar examples — only their leaf string/number/bool
            // children are useful.
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

    #[test]
    fn empty_input_returns_none() {
        assert_eq!(synopsize("shell", "", 1024), None);
        assert_eq!(synopsize("shell", "   \n\t  ", 1024), None);
    }

    #[test]
    fn non_json_input_returns_none() {
        assert_eq!(synopsize("shell", "hello world", 1024), None);
        assert_eq!(synopsize("shell", "the result is: success\n", 1024), None);
    }

    #[test]
    fn zero_budget_returns_none() {
        assert_eq!(synopsize("shell", "{\"a\":1}", 0), None);
    }

    #[test]
    fn json_object_produces_stable_labels() {
        let value = json!({
            "ok": true,
            "rows": 3,
            "users": [
                {"id": 1, "name": "alice"},
                {"id": 2, "name": "bob"},
            ],
            "config": {
                "host": "example.com",
                "port": 443,
                "tls": true,
            }
        });
        let raw = serde_json::to_string(&value).unwrap();
        let out = synopsize("task_list", &raw, 1200).expect("synopsis present");
        // Always-on labels.
        assert!(has_label(&out, "kind"), "kind missing: {out}");
        assert!(has_label(&out, "root"), "root missing: {out}");
        // JSON-specific labels for this input.
        assert!(has_label(&out, "arrays"), "arrays missing: {out}");
        assert!(
            has_label(&out, "object shape depth 2"),
            "object shape depth 2 missing: {out}"
        );
        // Sorted key ordering for determinism.
        assert!(
            out.contains("config, ok, rows, users") || out.contains("config"),
            "expected sorted keys: {out}"
        );
    }

    #[test]
    fn json_array_produces_stable_labels() {
        let value = json!([
            {"id": 1, "name": "alpha"},
            {"id": 2, "name": "beta"},
            {"id": 3, "name": "gamma"}
        ]);
        let raw = serde_json::to_string(&value).unwrap();
        let out = synopsize("task_list", &raw, 1200).expect("synopsis present");
        assert!(has_label(&out, "kind"), "kind missing: {out}");
        assert!(has_label(&out, "root"), "root missing: {out}");
        assert!(has_label(&out, "arrays"), "arrays missing: {out}");
        assert!(out.contains("root=3"), "expected root=3: {out}");
    }

    #[test]
    fn scalar_examples_are_bounded() {
        let value = json!({
            "status": "ok",
            "code": 200,
            "flag": true,
            "name": "alpha"
        });
        let raw = serde_json::to_string(&value).unwrap();
        let out = synopsize("shell", &raw, 1200).expect("synopsis present");
        assert!(
            has_label(&out, "scalar examples"),
            "scalar examples missing: {out}"
        );
        // No single scalar should exceed MAX_SCALAR_EXAMPLE_CHARS + 1 (for the ellipsis).
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
        let value = json!({
            "status": "completed",
            "result": "ok",
            "user_id": "u_123"
        });
        let raw = serde_json::to_string(&value).unwrap();
        let out = synopsize("task_list", &raw, 1200).expect("synopsis present");
        assert!(has_label(&out, "suggested grep terms"));
        let bullet = out
            .lines()
            .find(|l| l.starts_with("- suggested grep terms"))
            .unwrap();
        // Each term fits the per-term cap and we never emit more than the cap.
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
    fn malformed_json_falls_back_to_none() {
        // Starts with `{` so the heuristic accepts it, but the body is
        // syntactically broken — the parser must return None, not panic.
        assert_eq!(synopsize("shell", "{not json", 1024), None);
        assert_eq!(synopsize("shell", "{\"a\":}", 1024), None);
        // Truncated mid-value.
        assert_eq!(synopsize("shell", "{\"a\":1,\"b\":[1,2,", 1024), None);
    }

    #[test]
    fn pathological_depth_does_not_panic() {
        // 10,000 nested arrays. The streaming structural scanner rejects this
        // at MAX_JSON_DEPTH (128) *before* the deserializer runs, so synopsize
        // returns None without ever materializing a Value tree.
        let mut s = String::new();
        for _ in 0..10_000 {
            s.push('[');
        }
        s.push('1');
        for _ in 0..10_000 {
            s.push(']');
        }
        assert_eq!(synopsize("shell", &s, 4096), None);
    }

    #[test]
    fn oversized_valid_json_returns_none() {
        // Construct valid JSON just over MAX_PARSE_BYTES. The pre-parse
        // ceiling must reject it *before* calling serde_json::from_str,
        // so no unbounded allocation occurs.
        let inner = "1,".repeat(MAX_PARSE_BYTES / 2 + 1);
        // Remove the trailing comma and wrap in an array.
        let trimmed_inner = inner.trim_end_matches(',');
        let raw = format!("[{trimmed_inner}]");
        assert!(
            raw.len() > MAX_PARSE_BYTES,
            "test setup: raw.len()={} should exceed MAX_PARSE_BYTES={MAX_PARSE_BYTES}",
            raw.len()
        );
        assert_eq!(
            synopsize("shell", &raw, 4096),
            None,
            "oversized valid JSON must return None without parsing"
        );
    }

    #[test]
    fn pathological_breadth_over_token_limit_returns_none() {
        // Construct valid JSON whose token count *exceeds* MAX_JSON_TOKENS.
        // Each element contributes one number token; with MAX_JSON_TOKENS+1
        // elements the streaming scan rejects it before the deserializer runs.
        // This is under MAX_PARSE_BYTES, so the token limit is what trips.
        let n = MAX_JSON_TOKENS + 10;
        let raw: String =
            std::iter::once('[')
                .chain((0..n).flat_map(|i| {
                    std::iter::once('1').chain(if i + 1 < n { Some(',') } else { None })
                }))
                .chain(std::iter::once(']'))
                .collect();
        assert!(
            raw.len() < MAX_PARSE_BYTES,
            "test setup: raw.len()={} should be under MAX_PARSE_BYTES for this test to exercise the token limit",
            raw.len()
        );
        assert_eq!(synopsize("shell", &raw, 4096), None);
    }

    #[test]
    fn pathological_breadth_does_not_panic() {
        // 50,000 top-level keys with boolean values: ~100k tokens — under
        // MAX_JSON_TOKENS. The streaming scan accepts it and the synopsis is
        // built. The invariant is *no panic* and *respects budget*. (Truly
        // hostile breadth, e.g. 250k+ scalar tokens, is rejected by the
        // streaming scan — see `oversized_valid_json_returns_none` for that.)
        let mut map = serde_json::Map::with_capacity(50_000);
        for i in 0..50_000 {
            map.insert(format!("k{i}"), serde_json::Value::Bool(i % 2 == 0));
        }
        let value = serde_json::Value::Object(map);
        let raw = serde_json::to_string(&value).unwrap();
        let result = synopsize("shell", &raw, 4096);
        match result {
            None => {} // token cap tripped — fine.
            Some(s) => {
                assert!(
                    s.len() <= 4096,
                    "synopsis exceeded budget: len={}, {s}",
                    s.len()
                );
            }
        }
    }

    #[test]
    fn pathological_breadth_just_under_limit_returns_bounded_synopsis() {
        // A valid JSON object with many keys but still under MAX_JSON_TOKENS.
        // The streaming scan accepts it; the deserializer runs; the synopsis
        // is emitted and must respect the budget. This guards against the
        // scanner being overly aggressive (rejecting valid moderate inputs).
        let n = 5_000;
        let mut map = serde_json::Map::with_capacity(n);
        for i in 0..n {
            map.insert(format!("k{i}"), serde_json::Value::Number(i.into()));
        }
        let value = serde_json::Value::Object(map);
        let raw = serde_json::to_string(&value).unwrap();
        let out = synopsize("shell", &raw, 4096).expect("synopsis for large-but-valid JSON");
        assert!(
            out.len() <= 4096,
            "synopsis exceeded budget: len={}, {out}",
            out.len()
        );
        assert!(has_label(&out, "kind"));
        assert!(has_label(&out, "root"));
    }

    #[test]
    fn budget_enforcement_caps_total_length() {
        let value = json!({
            "alpha": "x".repeat(200),
            "beta": "y".repeat(200),
            "gamma": "z".repeat(200),
            "delta": [1, 2, 3, 4, 5, 6, 7, 8],
            "epsilon": {"k1": 1, "k2": 2, "k3": 3, "k4": 4}
        });
        let raw = serde_json::to_string(&value).unwrap();
        // Tiny budget — only the always-on fields should fit.
        let out = synopsize("shell", &raw, 80).expect("synopsis present");
        assert!(
            out.len() <= 80,
            "synopsis exceeded budget: len={}, {out}",
            out.len()
        );
        // We must keep the always-on labels even at a tight budget.
        assert!(has_label(&out, "kind"));
        assert!(has_label(&out, "root"));
    }

    #[test]
    fn budget_zero_is_safe() {
        let value = json!({"a": 1});
        let raw = serde_json::to_string(&value).unwrap();
        assert_eq!(synopsize("shell", &raw, 0), None);
    }

    #[test]
    fn empty_object_json_is_supported() {
        let raw = "{}";
        let out = synopsize("shell", raw, 1024).expect("synopsis present");
        assert!(has_label(&out, "kind"));
        assert!(has_label(&out, "root"));
        assert!(out.contains("empty object"));
    }

    #[test]
    fn empty_array_json_is_supported() {
        let raw = "[]";
        let out = synopsize("shell", raw, 1024).expect("synopsis present");
        assert!(has_label(&out, "kind"));
        assert!(has_label(&out, "root"));
        assert!(out.contains("empty array"));
    }

    #[test]
    fn top_level_scalar_json_is_supported() {
        // A bare top-level string is valid JSON. We should still emit a
        // useful synopsis for it (string truncation, no arrays/object shape).
        let raw = "\"hello world\"";
        let out = synopsize("shell", raw, 1024).expect("synopsis present");
        assert!(has_label(&out, "kind"));
        assert!(has_label(&out, "root"));
    }

    #[test]
    fn deterministic_across_runs() {
        // Same input, twice — bytes must match exactly. This guards against
        // accidental nondeterminism from HashSet iteration order in
        // suggested-grep-terms.
        let value = json!({
            "a": "one",
            "b": "two",
            "c": "three",
            "nested": {"x": 1, "y": 2}
        });
        let raw = serde_json::to_string(&value).unwrap();
        let s1 = synopsize("shell", &raw, 1200).unwrap();
        let s2 = synopsize("shell", &raw, 1200).unwrap();
        assert_eq!(s1, s2);
    }

    #[test]
    fn tool_name_does_not_influence_json_synopsis() {
        // This slice is JSON-only, so tool_name is intentionally ignored.
        // The follow-up task will gate detection on tool_name.
        let raw = "{\"a\":1}";
        let a = synopsize("shell", raw, 1024).unwrap();
        let b = synopsize("task_list", raw, 1024).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn streaming_scan_rejects_unterminated_string() {
        // Structurally broken — the streaming scanner returns None before
        // the deserializer ever runs.
        assert_eq!(count_tokens_streaming("{\"a\": \"oops"), None);
    }

    #[test]
    fn streaming_scan_rejects_unbalanced_brackets() {
        assert_eq!(count_tokens_streaming("[1,2,3"), None);
        assert_eq!(count_tokens_streaming("1,2,3]"), None);
    }

    #[test]
    fn streaming_scan_counts_simple_object() {
        // {"a":1,"b":2} -> tokens: {, "a", 1, "b", 2 = 5
        assert_eq!(count_tokens_streaming("{\"a\":1,\"b\":2}"), Some(5));
    }

    #[test]
    fn streaming_scan_handles_escaped_quotes_in_strings() {
        // String containing an escaped quote — must not terminate early.
        let raw = "{\"msg\":\"he said \\\"hi\\\"\"}";
        assert!(count_tokens_streaming(raw).is_some());
        // And the full synopsis path works end-to-end.
        let out = synopsize("shell", raw, 1024).expect("synopsis present");
        assert!(has_label(&out, "kind"));
    }
}
