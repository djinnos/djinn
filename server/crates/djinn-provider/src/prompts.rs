//! Prompt templates used by MCP workers.

pub const MEMORY_L0_ABSTRACT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/prompts/memory_l0_abstract.md"
));

pub const MEMORY_L1_OVERVIEW: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/prompts/memory_l1_overview.md"
));

/// The literal lead-in [`MEMORY_L0_ABSTRACT`] asks every abstract to open with.
///
/// This is a *readability* convention only. It is deliberately **not** the
/// vintage marker: keying staleness on the model emitting an exact prefix made
/// convergence depend on model compliance, so any note the model phrased
/// differently was re-selected and re-paid-for on every sweep, forever. Vintage
/// is now recorded per note against [`memory_l0_prompt_version`].
pub const MEMORY_L0_APPLICABILITY_PREFIX: &str = "Applies when";

/// Content-addressed version of [`MEMORY_L0_ABSTRACT`].
///
/// The abstract backfill records this against each note it regenerates, and a
/// note is stale when its recorded version differs from this one. Deriving it
/// from the prompt's own bytes means editing the prompt *automatically*
/// invalidates the corpus — there is no separate constant to forget to bump,
/// which is the failure mode a hand-maintained version number invites.
pub fn memory_l0_prompt_version() -> &'static str {
    use sha2::{Digest, Sha256};
    use std::sync::OnceLock;

    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let digest = Sha256::digest(MEMORY_L0_ABSTRACT.as_bytes());
        format!("l0-{:x}", digest)[..19].to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l0_abstract_prompt_keeps_render_placeholders() {
        // `render_memory_prompt` does a literal `.replace()` on exactly these two
        // tokens; renaming either silently breaks rendering.
        assert!(MEMORY_L0_ABSTRACT.contains("{{title}}"));
        assert!(MEMORY_L0_ABSTRACT.contains("{{content}}"));
    }

    #[test]
    fn l0_abstract_prompt_asks_for_the_applicability_lead_in() {
        assert!(
            MEMORY_L0_ABSTRACT.contains(MEMORY_L0_APPLICABILITY_PREFIX),
            "the prompt must still ask for the applicability lead-in"
        );
    }

    #[test]
    fn l0_prompt_version_is_derived_from_the_prompt_bytes() {
        let version = memory_l0_prompt_version();
        assert!(version.starts_with("l0-"), "unexpected shape: {version}");
        assert_eq!(version.len(), 19, "version must be a stable width");
        // Stable across calls — the corpus is compared against it.
        assert_eq!(version, memory_l0_prompt_version());

        // And it genuinely tracks the bytes: a different prompt hashes
        // differently, so editing the prompt invalidates the corpus without
        // anyone remembering to bump a constant.
        use sha2::{Digest, Sha256};
        let other = format!("l0-{:x}", Sha256::digest(b"a different prompt"));
        assert_ne!(version, &other[..19]);
    }

    #[test]
    fn l0_abstract_prompt_permits_inline_code() {
        // The pre-u46i prompt banned markdown outright, which made an abstract
        // structurally unable to carry a command. Guard the reversal.
        assert!(MEMORY_L0_ABSTRACT.contains("backticks"));
        assert!(
            !MEMORY_L0_ABSTRACT.contains("Do not include headings, labels, markdown"),
            "the blanket markdown ban must stay removed"
        );
    }
}
