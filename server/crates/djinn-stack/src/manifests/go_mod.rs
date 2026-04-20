//! `go.mod` parser — regex-based, matches the two lines we care about:
//! `module <path>` and `go <version>`.

use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, Default)]
pub struct GoModInfo {
    pub module: Option<String>,
    /// Normalized major.minor from the `go <version>` directive.
    pub go_version: Option<String>,
}

fn module_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^module\s+(\S+)").unwrap())
}

fn go_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^go\s+([0-9]+\.[0-9]+(?:\.[0-9]+)?)").unwrap())
}

pub fn parse_go_mod(body: &str) -> GoModInfo {
    let module = module_re()
        .captures(body)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
    let go_version = go_re()
        .captures(body)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
    GoModInfo { module, go_version }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_module_and_go_version() {
        let body = "module github.com/foo/bar\n\ngo 1.22\n\nrequire (\n  github.com/x/y v1.0.0\n)\n";
        let info = parse_go_mod(body);
        assert_eq!(info.module.as_deref(), Some("github.com/foo/bar"));
        assert_eq!(info.go_version.as_deref(), Some("1.22"));
    }

    #[test]
    fn handles_absent_fields() {
        let info = parse_go_mod("");
        assert!(info.module.is_none());
        assert!(info.go_version.is_none());
    }
}
