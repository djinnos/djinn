//! Linguist `heuristics.yml` — currently parsed-but-unused.
//!
//! The current detection pass is extension-only (fast path, <1s target)
//! and deliberately does not open arbitrary source files. We vendor the
//! heuristics data + a minimal parser so a future PR can enable
//! content-based disambiguation for the small handful of ambiguous
//! extensions (`.h`, `.m`, `.pl`) without a second vendoring step.

use serde::Deserialize;

const HEURISTICS_YML: &str = include_str!("../data/heuristics.yml");

#[derive(Debug, Deserialize)]
pub struct HeuristicsFile {
    pub disambiguations: Vec<Disambiguation>,
}

#[derive(Debug, Deserialize)]
pub struct Disambiguation {
    pub extensions: Vec<String>,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Deserialize)]
pub struct Rule {
    pub language: String,
    #[serde(default)]
    pub pattern: Option<String>,
}

impl HeuristicsFile {
    pub fn load() -> Result<Self, serde_yaml::Error> {
        serde_yaml::from_str(HEURISTICS_YML)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heuristics_yaml_parses() {
        let h = HeuristicsFile::load().expect("vendored heuristics.yml is well-formed");
        assert!(!h.disambiguations.is_empty());
    }
}
