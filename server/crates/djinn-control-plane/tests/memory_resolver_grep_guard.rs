use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const SCANNED_CRATES: &[&str] = &["djinn-control-plane", "djinn-agent", "djinn-provider"];

#[test]
fn production_memory_resolution_uses_scoped_credentials() {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("canonicalize server/crates directory");

    let mut violations = Vec::new();
    for krate in SCANNED_CRATES {
        collect_violations(&workspace.join(krate).join("src"), &mut violations);
    }

    assert!(
        violations.is_empty(),
        "production provider-resolution paths must not use the unscoped \
         resolve_memory_provider entry point or CredentialRepository::list(); \
         use resolve_memory_provider_for_user(..., user_id) and list_for_user instead:\n{}",
        violations.join("\n")
    );
}

fn collect_violations(dir: &Path, violations: &mut Vec<String>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|error| {
        panic!("read {}: {error}", dir.display());
    });

    for entry in entries {
        let entry = entry.expect("read directory entry");
        let path = entry.path();
        let file_type = entry.file_type().expect("read file type");
        if file_type.is_dir() {
            if path.file_name().and_then(|name| name.to_str()) != Some("tests") {
                collect_violations(&path, violations);
            }
            continue;
        }

        if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            scan_file(&path, violations);
        }
    }
}

fn scan_file(path: &Path, violations: &mut Vec<String>) {
    let content = fs::read_to_string(path).unwrap_or_else(|error| {
        panic!("read {}: {error}", path.display());
    });

    let mut pending_cfg_test = false;
    let mut cfg_test_brace_depth: Option<i32> = None;
    let mut credential_repo_vars = BTreeSet::new();
    let mut pending_credential_repo_binding: Option<String> = None;

    for (index, line) in content.lines().enumerate() {
        let trimmed = line.trim();

        if cfg_test_brace_depth.is_none() && trimmed.starts_with("#[cfg(test)]") {
            pending_cfg_test = true;
            continue;
        }

        if pending_cfg_test && trimmed.starts_with("mod tests") {
            let depth = brace_delta(line);
            if depth > 0 {
                cfg_test_brace_depth = Some(depth);
            }
            pending_cfg_test = false;
            continue;
        }

        pending_cfg_test = false;

        if let Some(depth) = cfg_test_brace_depth.as_mut() {
            *depth += brace_delta(line);
            if *depth <= 0 {
                cfg_test_brace_depth = None;
            }
            continue;
        }

        let line_number = index + 1;
        if let Some(var_name) = let_binding_var_name(line) {
            pending_credential_repo_binding = Some(var_name.to_string());
        }
        if line.contains("CredentialRepository::new(") {
            if let Some(var_name) = credential_repo_var_name(line)
                .map(str::to_string)
                .or_else(|| pending_credential_repo_binding.take())
            {
                credential_repo_vars.insert(var_name);
            }
        } else if line.contains(';') {
            pending_credential_repo_binding = None;
        }
        if contains_unscoped_memory_resolver(path, line) {
            violations.push(format!("{}:{line_number}: {trimmed}", path.display()));
        }
        if contains_unscoped_credential_list(line, &credential_repo_vars) {
            violations.push(format!("{}:{line_number}: {trimmed}", path.display()));
        }
    }
}

fn contains_unscoped_memory_resolver(path: &Path, line: &str) -> bool {
    if !line.contains("resolve_memory_provider(") {
        return false;
    }

    // g2od's compatibility wrapper is responsible for making the no-arg entry
    // point safe. Production call sites outside the wrapper must use the
    // explicit caller-scoped resolver instead.
    !(path.ends_with("djinn-provider/src/completion.rs")
        && line.contains("pub async fn resolve_memory_provider("))
}

fn credential_repo_var_name(line: &str) -> Option<&str> {
    if !line.contains("CredentialRepository::new(") {
        return None;
    }

    let_binding_var_name(line)
}

fn let_binding_var_name(line: &str) -> Option<&str> {
    let binding = line.split_once("let ")?.1.trim_start();
    binding
        .strip_prefix("mut ")
        .unwrap_or(binding)
        .split(|ch: char| !(ch == '_' || ch.is_ascii_alphanumeric()))
        .next()
        .filter(|name| !name.is_empty())
}

fn contains_unscoped_credential_list(line: &str, repo_vars: &BTreeSet<String>) -> bool {
    line.contains("CredentialRepository::list(")
        || repo_vars
            .iter()
            .any(|var_name| line.contains(&format!("{var_name}.list(")))
}

fn brace_delta(line: &str) -> i32 {
    line.chars().fold(0, |delta, ch| match ch {
        '{' => delta + 1,
        '}' => delta - 1,
        _ => delta,
    })
}
