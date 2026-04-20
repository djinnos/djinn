//! `pyproject.toml` parser.
//!
//! Detects the package-manager flavour (`uv`, `poetry`, `pdm`, or the
//! generic PEP 621 `[project]` table), the declared Python version
//! constraint, and the dependency name set.

use serde::Deserialize;

#[derive(Debug, Clone, Default)]
pub struct PyprojectInfo {
    /// Canonical slug: `uv` | `poetry` | `pdm` | `pip`. `None` when
    /// no build/tool hint is present.
    pub package_manager: Option<String>,
    /// Normalized major.minor from `requires-python` or Poetry's
    /// `[tool.poetry.dependencies].python`. `None` if unspecified.
    pub python_version: Option<String>,
    /// Flat name set of declared dependencies (runtime only).
    pub dep_names: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawPyproject {
    #[serde(default)]
    project: Option<RawProject>,
    #[serde(default)]
    tool: Option<RawTool>,
    #[serde(rename = "build-system", default)]
    build_system: Option<RawBuildSystem>,
}

#[derive(Debug, Deserialize)]
struct RawProject {
    #[serde(rename = "requires-python", default)]
    requires_python: Option<String>,
    #[serde(default)]
    dependencies: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RawBuildSystem {
    #[serde(rename = "build-backend", default)]
    build_backend: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawTool {
    #[serde(default)]
    uv: Option<toml::Value>,
    #[serde(default)]
    poetry: Option<RawPoetry>,
    #[serde(default)]
    pdm: Option<toml::Value>,
}

#[derive(Debug, Deserialize)]
struct RawPoetry {
    #[serde(default)]
    dependencies: Option<toml::Table>,
}

pub fn parse_pyproject(body: &str) -> PyprojectInfo {
    let raw: RawPyproject = match toml::from_str(body) {
        Ok(v) => v,
        Err(err) => {
            tracing::debug!(%err, "pyproject.toml parse failed, returning default");
            return PyprojectInfo::default();
        }
    };

    let package_manager = detect_pm(&raw);
    let python_version = raw
        .project
        .as_ref()
        .and_then(|p| p.requires_python.as_deref())
        .map(extract_python_major_minor)
        .or_else(|| {
            raw.tool
                .as_ref()
                .and_then(|t| t.poetry.as_ref())
                .and_then(|p| p.dependencies.as_ref())
                .and_then(|d| d.get("python"))
                .and_then(|v| v.as_str())
                .map(extract_python_major_minor)
        });

    let mut dep_names: Vec<String> = Vec::new();
    if let Some(project) = raw.project
        && let Some(deps) = project.dependencies
    {
        for spec in deps {
            dep_names.push(dependency_name(&spec));
        }
    }
    if let Some(tool) = raw.tool
        && let Some(poetry) = tool.poetry
        && let Some(deps) = poetry.dependencies
    {
        for (k, _v) in deps {
            if k != "python" {
                dep_names.push(k);
            }
        }
    }
    dep_names.sort();
    dep_names.dedup();

    PyprojectInfo {
        package_manager,
        python_version,
        dep_names,
    }
}

fn detect_pm(raw: &RawPyproject) -> Option<String> {
    if let Some(tool) = &raw.tool {
        if tool.uv.is_some() {
            return Some("uv".into());
        }
        if tool.poetry.is_some() {
            return Some("poetry".into());
        }
        if tool.pdm.is_some() {
            return Some("pdm".into());
        }
    }
    if let Some(bs) = &raw.build_system
        && let Some(backend) = bs.build_backend.as_deref()
    {
        if backend.starts_with("poetry") {
            return Some("poetry".into());
        }
        if backend.starts_with("pdm") {
            return Some("pdm".into());
        }
        if backend.starts_with("hatchling") || backend.starts_with("setuptools") {
            return Some("pip".into());
        }
    }
    None
}

/// `">=3.11"` → `Some("3.11")`, `"^3.12.1"` → `Some("3.12")`.
fn extract_python_major_minor(raw: &str) -> String {
    let mut out = String::new();
    let mut saw_digit = false;
    let mut dots = 0;
    for ch in raw.chars() {
        if ch.is_ascii_digit() {
            out.push(ch);
            saw_digit = true;
        } else if ch == '.' && saw_digit && dots == 0 {
            out.push('.');
            dots += 1;
            saw_digit = false;
        } else if saw_digit && dots >= 1 {
            break;
        }
    }
    // Strip trailing `.` when only major was present.
    if out.ends_with('.') {
        out.pop();
    }
    if out.is_empty() { raw.to_string() } else { out }
}

/// `"requests>=2.31"` → `"requests"`. Handles extras + markers.
fn dependency_name(spec: &str) -> String {
    let end = spec
        .find(['[', '>', '<', '=', '!', '~', ';', ' '])
        .unwrap_or(spec.len());
    spec[..end].trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_uv_tool_section() {
        let body = r#"
[project]
name = "x"
requires-python = ">=3.12"
dependencies = ["requests>=2.31", "httpx[http2]~=0.27"]

[tool.uv]
package = true
"#;
        let info = parse_pyproject(body);
        assert_eq!(info.package_manager.as_deref(), Some("uv"));
        assert_eq!(info.python_version.as_deref(), Some("3.12"));
        assert_eq!(info.dep_names, vec!["httpx", "requests"]);
    }

    #[test]
    fn detects_poetry_with_python_pin() {
        let body = r#"
[tool.poetry]
name = "x"
version = "0"
description = ""
authors = []

[tool.poetry.dependencies]
python = "^3.11"
django = "^5"
"#;
        let info = parse_pyproject(body);
        assert_eq!(info.package_manager.as_deref(), Some("poetry"));
        assert_eq!(info.python_version.as_deref(), Some("3.11"));
        assert_eq!(info.dep_names, vec!["django"]);
    }
}
