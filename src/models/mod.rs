pub mod credential;
pub mod epic;
pub mod git_settings;
pub mod note;
pub mod project;
pub mod provider;
pub mod session;
pub mod session_message;
pub mod settings;
pub mod task;

/// Parse a JSON array string (e.g. `'["a","b"]'`) into a `Vec<String>`.
/// Returns an empty vec on any parse failure.
pub fn parse_json_array(json: &str) -> Vec<String> {
    serde_json::from_str(json).unwrap_or_default()
}
