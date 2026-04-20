//! Tiny Java-ecosystem detectors. We deliberately avoid a full Maven /
//! Gradle parser — we only need "is this a Maven project?" / "is this
//! a Gradle project?" plus best-effort framework-dep presence (`spring`
//! / `junit`). Everything is regex-based on the raw file body.

use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, Default)]
pub struct JavaInfo {
    pub has_spring: bool,
    pub has_junit: bool,
}

fn spring_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"org\.springframework|spring-boot").unwrap())
}

fn junit_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"org\.junit|junit-jupiter|junit\s*:").unwrap())
}

pub fn parse_pom(body: &str) -> JavaInfo {
    scan(body)
}

pub fn parse_gradle(body: &str) -> JavaInfo {
    scan(body)
}

fn scan(body: &str) -> JavaInfo {
    JavaInfo {
        has_spring: spring_re().is_match(body),
        has_junit: junit_re().is_match(body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pom_detects_spring_and_junit() {
        let body = r#"
<dependency>
  <groupId>org.springframework.boot</groupId>
  <artifactId>spring-boot-starter</artifactId>
</dependency>
<dependency>
  <groupId>org.junit.jupiter</groupId>
  <artifactId>junit-jupiter</artifactId>
</dependency>
"#;
        let info = parse_pom(body);
        assert!(info.has_spring);
        assert!(info.has_junit);
    }
}
