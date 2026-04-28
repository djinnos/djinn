//! Enriched-text rendering for `code_chunks`. The chunker calls
//! [`render_chunk_text`] once per piece to produce the natural-language
//! header that wraps the raw code body before it goes into the embedding
//! model.
//!
//! Format (architect-pinned, see plan §"PR B2 → Enriched text"):
//!
//! ```text
//! Label: <name>
//! Repo: <owner/repo>
//! Path: <file>
//! Export: <true|false>
//! <description, ≤150 chars — only emitted if non-empty>
//! [preceding context]: <prevTail>
//! <container signature — only emitted if non-empty>
//! Methods: <names…>
//! Properties: <names…>
//! <chunk body>
//! ```
//!
//! The header is the recall lever — embedding raw code alone collapses
//! semantic distance between near-identical helper functions. The header
//! gives the model anchors (kind, path, export status, container, sibling
//! names) it can match against natural-language queries.

use sha2::{Digest, Sha256};

/// Bumped when the rendered chunk text changes shape. The
/// [`content_hash`] function folds this in, so a rules change here
/// invalidates every existing meta row in one pass without a manual
/// re-hash sweep.
///
/// History:
/// * `v0` — PR B1 stub (no rendering yet).
/// * `v1` — PR B2: full natural-language header + body wrapping.
pub const EMBEDDING_TEXT_VERSION: &str = "v1";

/// Inputs to [`render_chunk_text`]. Constructed by the chunker once it
/// has carved a piece off the symbol body.
#[derive(Clone, Debug)]
pub struct RenderInput<'a> {
    pub label: &'a str,
    pub owner: &'a str,
    pub repo: &'a str,
    pub file_path: &'a str,
    pub kind: &'a str,
    pub is_export: bool,
    /// Symbol description / docstring summary (≤150 chars). May be
    /// empty — the renderer drops the line entirely in that case.
    pub description: String,
    /// Last 120 chars of the previous chunk in the same symbol, when
    /// this is a follow-up chunk in a multi-chunk body. `None` for the
    /// first / only chunk.
    pub prev_tail: Option<&'a str>,
    /// Container signature (e.g. `fn foo()` or `class Greeter`). May
    /// be `None` — line dropped if so.
    pub container_signature: Option<&'a str>,
    /// Member methods to surface in the header. Only populated for
    /// declaration chunks. Empty → line dropped.
    pub methods: &'a [String],
    /// Member properties to surface in the header. Only populated for
    /// declaration chunks. Empty → line dropped.
    pub properties: &'a [String],
    /// The raw code body for this chunk.
    pub body: &'a str,
}

/// Produce the embedding-friendly text for one chunk. Always includes
/// the four mandatory lines (`Label`/`Repo`/`Path`/`Export`); optional
/// lines (description, prev-tail, signature, methods, properties) are
/// skipped when their inputs are empty so the header stays compact.
pub fn render_chunk_text(input: &RenderInput<'_>) -> String {
    let mut out = String::new();
    out.push_str("Label: ");
    out.push_str(input.label);
    out.push('\n');

    out.push_str("Repo: ");
    out.push_str(input.owner);
    out.push('/');
    out.push_str(input.repo);
    out.push('\n');

    out.push_str("Path: ");
    out.push_str(input.file_path);
    out.push('\n');

    out.push_str("Export: ");
    out.push_str(if input.is_export { "true" } else { "false" });
    out.push('\n');

    if !input.description.trim().is_empty() {
        out.push_str(input.description.trim());
        out.push('\n');
    }

    if let Some(prev) = input.prev_tail
        && !prev.is_empty()
    {
        out.push_str("[preceding context]: ");
        out.push_str(prev);
        out.push('\n');
    }

    if let Some(sig) = input.container_signature
        && !sig.trim().is_empty()
    {
        out.push_str(sig.trim());
        out.push('\n');
    }

    if !input.methods.is_empty() {
        out.push_str("Methods: ");
        out.push_str(&input.methods.join(", "));
        out.push('\n');
    }

    if !input.properties.is_empty() {
        out.push_str("Properties: ");
        out.push_str(&input.properties.join(", "));
        out.push('\n');
    }

    out.push_str(input.body);
    if !input.body.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// `sha256(EMBEDDING_TEXT_VERSION || generated_text)` for invalidation.
///
/// The plan specifies `sha1` conceptually; we use SHA-256 to reuse the
/// existing `sha2` workspace dependency (no new deps per architect call).
/// Algorithm choice doesn't matter for invalidation semantics — only
/// stability + uniqueness matter, and SHA-256 has both.
pub fn content_hash(version: &str, text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(version.as_bytes());
    hasher.update(b"\0");
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();
    format!("{digest:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input<'a>(body: &'a str) -> RenderInput<'a> {
        RenderInput {
            label: "do_thing",
            owner: "djinnos",
            repo: "djinn",
            file_path: "src/lib.rs",
            kind: "function",
            is_export: true,
            description: String::new(),
            prev_tail: None,
            container_signature: Some("fn do_thing(x: u32) -> u32"),
            methods: &[],
            properties: &[],
            body,
        }
    }

    #[test]
    fn header_renders_required_fields_in_order() {
        let body = "    let y = x + 1;\n    y\n";
        let text = render_chunk_text(&base_input(body));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Label: do_thing");
        assert_eq!(lines[1], "Repo: djinnos/djinn");
        assert_eq!(lines[2], "Path: src/lib.rs");
        assert_eq!(lines[3], "Export: true");
        assert_eq!(lines[4], "fn do_thing(x: u32) -> u32");
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn description_inserted_after_export_line_when_present() {
        let mut input = base_input("body\n");
        input.description = "Computes a thing from a number.".to_owned();
        let text = render_chunk_text(&input);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[3], "Export: true");
        assert_eq!(lines[4], "Computes a thing from a number.");
    }

    #[test]
    fn prev_tail_emits_preceding_context_line() {
        let mut input = base_input("body\n");
        input.prev_tail = Some("…the tail of last chunk\n}\n");
        let text = render_chunk_text(&input);
        assert!(text.contains("[preceding context]: …the tail of last chunk"));
    }

    #[test]
    fn methods_and_properties_emit_when_non_empty() {
        let methods = vec!["greet".to_owned(), "shout".to_owned()];
        let properties = vec!["name".to_owned()];
        let mut input = base_input("class Greeter {}\n");
        input.kind = "declaration";
        input.methods = &methods;
        input.properties = &properties;
        let text = render_chunk_text(&input);
        assert!(text.contains("Methods: greet, shout"));
        assert!(text.contains("Properties: name"));
    }

    #[test]
    fn empty_optional_fields_are_dropped() {
        let mut input = base_input("body\n");
        input.container_signature = None;
        let text = render_chunk_text(&input);
        assert!(!text.contains("Methods:"));
        assert!(!text.contains("Properties:"));
        assert!(!text.contains("[preceding context]:"));
    }

    #[test]
    fn content_hash_is_stable_and_version_sensitive() {
        let h1 = content_hash("v1", "Label: foo\nbody\n");
        let h2 = content_hash("v1", "Label: foo\nbody\n");
        assert_eq!(h1, h2, "same inputs → same hash");
        let h3 = content_hash("v2", "Label: foo\nbody\n");
        assert_ne!(h1, h3, "version bump should change the hash");
        let h4 = content_hash("v1", "Label: bar\nbody\n");
        assert_ne!(h1, h4, "different text → different hash");
    }
}
