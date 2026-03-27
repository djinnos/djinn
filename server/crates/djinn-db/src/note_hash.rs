use sha2::{Digest, Sha256};

/// Normalize note content for stable hashing.
///
/// - converts CRLF/CR to LF
/// - trims leading/trailing whitespace
pub fn normalize_note_content(content: &str) -> String {
    content
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .to_string()
}

/// Compute a hex SHA-256 content hash for normalized note content.
pub fn note_content_hash(content: &str) -> String {
    let normalized = normalize_note_content(content);
    let digest = Sha256::digest(normalized.as_bytes());
    format!("{digest:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_unifies_line_endings_and_trim() {
        assert_eq!(normalize_note_content("\r\n hello\r\n"), "hello");
        assert_eq!(normalize_note_content("a\rb\r\n"), "a\nb");
    }

    #[test]
    fn hash_is_stable_for_equivalent_content() {
        assert_eq!(
            note_content_hash(" hello\r\nworld \n"),
            note_content_hash("hello\nworld")
        );
    }
}
