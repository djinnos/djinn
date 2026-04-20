//! Load the vendored Linguist `languages.yml` into an in-memory lookup
//! table keyed by (lowercased) extension and filename.
//!
//! The real Linguist pipeline layers heuristics, classifier, and
//! override rules on top — see the notes in `heuristics.rs`. Djinn only
//! needs the fast path today: extension -> language name.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

const LANGUAGES_YML: &str = include_str!("../data/languages.yml");

#[derive(Debug, Deserialize)]
struct RawLanguage {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    filenames: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Language {
    pub name: String,
    pub kind: LanguageKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageKind {
    Programming,
    Markup,
    Data,
    Prose,
    Other,
}

impl LanguageKind {
    fn parse(s: &str) -> Self {
        match s {
            "programming" => Self::Programming,
            "markup" => Self::Markup,
            "data" => Self::Data,
            "prose" => Self::Prose,
            _ => Self::Other,
        }
    }
}

/// Lookup table built once from the vendored YAML.
#[derive(Debug)]
pub struct LanguageTable {
    by_extension: HashMap<String, Language>,
    by_filename: HashMap<String, Language>,
}

impl LanguageTable {
    /// Return the process-wide singleton. Parse happens at most once.
    pub fn global() -> &'static Self {
        static TABLE: OnceLock<LanguageTable> = OnceLock::new();
        TABLE.get_or_init(|| {
            Self::from_yaml(LANGUAGES_YML).expect("vendored languages.yml is well-formed")
        })
    }

    pub fn from_yaml(yaml: &str) -> Result<Self, serde_yaml::Error> {
        let raw: HashMap<String, RawLanguage> = serde_yaml::from_str(yaml)?;
        let mut by_extension = HashMap::new();
        let mut by_filename = HashMap::new();
        for (name, raw_lang) in raw {
            let kind = raw_lang
                .kind
                .as_deref()
                .map_or(LanguageKind::Other, LanguageKind::parse);
            let lang = Language {
                name: name.clone(),
                kind,
            };
            for ext in raw_lang.extensions {
                // Later insertions win — callers may want to tune this
                // per-language but for our subset any collision is
                // intentional (e.g. `.h` -> C; heuristics.yml can
                // re-route later).
                by_extension.insert(ext.to_ascii_lowercase(), lang.clone());
            }
            for fname in raw_lang.filenames {
                by_filename.insert(fname.to_string(), lang.clone());
            }
        }
        Ok(Self {
            by_extension,
            by_filename,
        })
    }

    /// Look up by file basename first, then by extension (lowercased).
    pub fn classify(&self, path: &str) -> Option<&Language> {
        let basename = path.rsplit('/').next().unwrap_or(path);
        if let Some(lang) = self.by_filename.get(basename) {
            return Some(lang);
        }
        let dot = basename.rfind('.')?;
        let ext = &basename[dot..];
        self.by_extension.get(&ext.to_ascii_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_rust_by_extension() {
        let table = LanguageTable::global();
        assert_eq!(table.classify("src/lib.rs").map(|l| l.name.as_str()), Some("Rust"));
    }

    #[test]
    fn classifies_dockerfile_by_filename() {
        let table = LanguageTable::global();
        assert_eq!(
            table.classify("deploy/Dockerfile").map(|l| l.name.as_str()),
            Some("Dockerfile")
        );
    }

    #[test]
    fn unknown_extension_returns_none() {
        let table = LanguageTable::global();
        assert!(table.classify("weird.xyzzy").is_none());
    }
}
