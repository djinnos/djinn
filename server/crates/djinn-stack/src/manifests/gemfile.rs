//! `Gemfile` parser — regex-based. Ruby is low-frequency in our corpus,
//! so we only extract the gem name list (for `rspec` test-runner
//! detection) and whether `ruby "<ver>"` was declared.

use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, Default)]
pub struct GemfileInfo {
    pub ruby_version: Option<String>,
    pub gems: Vec<String>,
}

fn ruby_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?m)^\s*ruby\s+['"]([^'"]+)['"]"#).unwrap())
}

fn gem_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?m)^\s*gem\s+['"]([^'"]+)['"]"#).unwrap())
}

pub fn parse_gemfile(body: &str) -> GemfileInfo {
    let ruby_version = ruby_re()
        .captures(body)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
    let mut gems: Vec<String> = gem_re()
        .captures_iter(body)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .collect();
    gems.sort();
    gems.dedup();
    GemfileInfo { ruby_version, gems }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gems_and_ruby_version() {
        let body = r#"
source "https://rubygems.org"
ruby "3.3.0"
gem "rails"
gem "rspec"
"#;
        let info = parse_gemfile(body);
        assert_eq!(info.ruby_version.as_deref(), Some("3.3.0"));
        assert!(info.gems.contains(&"rails".to_string()));
        assert!(info.gems.contains(&"rspec".to_string()));
    }
}
