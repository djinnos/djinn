//! Prompt templates used by MCP workers.

pub const MEMORY_L0_ABSTRACT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/prompts/memory_l0_abstract.md"
));

pub const MEMORY_L1_OVERVIEW: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/prompts/memory_l1_overview.md"
));

/// The literal lead-in [`MEMORY_L0_ABSTRACT`] requires every abstract to open
/// with.
///
/// This doubles as the **vintage marker**: an abstract that starts with this
/// phrase was produced by the applicability-condition prompt, and one that does
/// not predates it. Consumers count on that (see the abstract-vintage coverage
/// counter in `djinn-control-plane`), so changing this string without changing
/// the prompt — or the prompt without changing this string — silently reclassifies
/// the entire corpus.
pub const MEMORY_L0_APPLICABILITY_PREFIX: &str = "Applies when";

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
    fn l0_abstract_prompt_demands_the_vintage_marker() {
        assert!(
            MEMORY_L0_ABSTRACT.contains(MEMORY_L0_APPLICABILITY_PREFIX),
            "the prompt must ask for the literal lead-in the coverage counter matches on"
        );
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
