use serde::{Deserialize, Serialize};

/// A single key/value setting row in `settings`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Setting {
    pub key: String,
    pub value: String,
}

/// Typed settings snapshot used by API responses.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct DjinnSettings {
    pub auto_commit: bool,
    pub auto_push: bool,
    pub setup_commands: Vec<String>,
    pub verification_commands: Vec<String>,
}

impl DjinnSettings {
    /// Parse a list of `Setting` rows into a typed struct.
    pub fn from_rows(rows: &[Setting]) -> Self {
        let mut out = Self {
            auto_commit: false,
            auto_push: false,
            setup_commands: Vec::new(),
            verification_commands: Vec::new(),
        };

        for row in rows {
            match row.key.as_str() {
                "auto_commit" => out.auto_commit = row.value == "true",
                "auto_push" => out.auto_push = row.value == "true",
                "setup_commands" => {
                    if let Ok(v) = serde_json::from_str::<Vec<String>>(&row.value) {
                        out.setup_commands = v;
                    }
                }
                "verification_commands" => {
                    if let Ok(v) = serde_json::from_str::<Vec<String>>(&row.value) {
                        out.verification_commands = v;
                    }
                }
                _ => {}
            }
        }

        out
    }
}
