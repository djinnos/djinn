use std::path::{Path, PathBuf};
use std::process::Stdio;

#[derive(Debug, Clone, Copy)]
pub(super) struct ServerDef {
    pub(super) id: &'static str,
    /// The binary name (first element) and fixed args.
    pub(super) cmd: &'static [&'static str],
    pub(super) root_markers: &'static [&'static str],
    /// How to install this server if it's not found on PATH.
    install: InstallMethod,
}

#[derive(Debug, Clone, Copy)]
enum InstallMethod {
    /// Install via `npm install -g <packages..>` into ~/.djinn/bin
    Npm(&'static [&'static str]),
    /// Install via `rustup component add`, or download from GitHub releases.
    RustAnalyzer,
    /// Install via `go install <pkg>@latest` with GOBIN=~/.djinn/bin
    GoInstall(&'static str),
}

pub(super) fn server_for_path(path: &Path) -> Option<ServerDef> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some(ServerDef {
            id: "rust-analyzer",
            cmd: &["rust-analyzer"],
            root_markers: &["Cargo.toml"],
            install: InstallMethod::RustAnalyzer,
        }),
        Some("go") => Some(ServerDef {
            id: "gopls",
            cmd: &["gopls"],
            root_markers: &["go.mod"],
            install: InstallMethod::GoInstall("golang.org/x/tools/gopls"),
        }),
        Some("ts") | Some("tsx") | Some("js") | Some("jsx") => Some(ServerDef {
            id: "typescript-language-server",
            cmd: &["typescript-language-server", "--stdio"],
            root_markers: &["package.json", "tsconfig.json"],
            install: InstallMethod::Npm(&["typescript-language-server", "typescript"]),
        }),
        Some("py") => Some(ServerDef {
            id: "pyright",
            cmd: &["pyright-langserver", "--stdio"],
            root_markers: &["pyproject.toml", "setup.py"],
            install: InstallMethod::Npm(&["pyright"]),
        }),
        _ => None,
    }
}

/// Djinn-managed binary directory for auto-installed LSP servers.
pub(super) fn djinn_bin_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share")
        })
        .join("djinn")
        .join("bin")
}

/// Resolve the binary for an LSP server: check PATH (augmented with
/// ~/.djinn/bin), and auto-install if missing.
pub(super) async fn resolve_binary(server: &ServerDef) -> Result<PathBuf, String> {
    let bin_dir = djinn_bin_dir();
    let system_path = std::env::var("PATH").unwrap_or_default();
    resolve_binary_inner(server, &bin_dir, &system_path)
}

/// Core resolution logic, factored out for testing.
pub(super) fn resolve_binary_inner(
    server: &ServerDef,
    bin_dir: &Path,
    system_path: &str,
) -> Result<PathBuf, String> {
    let binary_name = server.cmd[0];

    let augmented_path = format!("{}:{}", bin_dir.display(), system_path);

    if let Some(found) = which_in_path(binary_name, &augmented_path) {
        tracing::debug!(binary = binary_name, path = %found.display(), "lsp: binary found");
        return Ok(found);
    }

    if matches!(server.install, InstallMethod::RustAnalyzer)
        && let Some(rustup) = which_in_path("rustup", system_path)
        && let Some(o) = std::process::Command::new(rustup)
            .args(["which", binary_name])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .ok()
        && o.status.success()
    {
        let p = PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string());
        if p.is_file() {
            tracing::debug!(binary = binary_name, path = %p.display(), "lsp: found via rustup which");
            return Ok(p);
        }
    }

    tracing::info!(
        server = server.id,
        binary = binary_name,
        "lsp: binary not found, attempting auto-install"
    );

    std::fs::create_dir_all(bin_dir)
        .map_err(|e| format!("failed to create {}: {e}", bin_dir.display()))?;

    match server.install {
        InstallMethod::Npm(packages) => {
            let npm = which_in_path("npm", system_path)
                .ok_or_else(|| "npm not found — cannot auto-install LSP server".to_string())?;

            let mut cmd = std::process::Command::new(npm);
            cmd.arg("install")
                .arg("-g")
                .arg(format!("--prefix={}", bin_dir.display()));
            for pkg in packages {
                cmd.arg(*pkg);
            }
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

            tracing::info!(packages = ?packages, prefix = %bin_dir.display(), "lsp: running npm install");
            let output = cmd
                .output()
                .map_err(|e| format!("npm install failed: {e}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("npm install failed: {stderr}"));
            }
        }
        InstallMethod::RustAnalyzer => {
            if let Some(rustup) = which_in_path("rustup", system_path) {
                tracing::info!("lsp: running rustup component add rust-analyzer");
                let output = std::process::Command::new(&rustup)
                    .args(["component", "add", "rust-analyzer"])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .map_err(|e| format!("rustup failed: {e}"))?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(format!("rustup component add failed: {stderr}"));
                }

                if which_in_path(binary_name, &augmented_path).is_none()
                    && let Some(o) = std::process::Command::new(&rustup)
                        .args(["which", binary_name])
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .output()
                        .ok()
                    && o.status.success()
                {
                    let p = PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string());
                    if p.is_file() {
                        tracing::info!(path = %p.display(), "lsp: resolved via rustup which");
                        return Ok(p);
                    }
                }
            } else {
                return Err(
                    "rust-analyzer not found — install via `rustup component add rust-analyzer` \
                     or your system package manager (e.g. `pacman -S rust-analyzer`)"
                        .to_string(),
                );
            }
        }
        InstallMethod::GoInstall(pkg) => {
            let go = which_in_path("go", system_path)
                .ok_or_else(|| "go not found — cannot auto-install gopls".to_string())?;

            tracing::info!(pkg = pkg, gobin = %bin_dir.display(), "lsp: running go install");
            let output = std::process::Command::new(go)
                .args(["install", &format!("{pkg}@latest")])
                .env("GOBIN", bin_dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .map_err(|e| format!("go install failed: {e}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("go install failed: {stderr}"));
            }
        }
    }

    which_in_path(binary_name, &augmented_path).ok_or_else(|| {
        format!(
            "installed {} but binary '{}' still not found in PATH or {}",
            server.id,
            binary_name,
            bin_dir.display()
        )
    })
}

/// Simple which(1) — scan colon-delimited PATH for an executable.
pub(super) fn which_in_path(binary: &str, path_var: &str) -> Option<PathBuf> {
    for dir in path_var.split(':') {
        let candidate = Path::new(dir).join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
        let npm_candidate = Path::new(dir).join("bin").join(binary);
        if npm_candidate.is_file() {
            return Some(npm_candidate);
        }
    }
    None
}

pub(super) fn language_id_for_path(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some("rust"),
        Some("go") => Some("go"),
        Some("py") => Some("python"),
        Some("ts") => Some("typescript"),
        Some("tsx") => Some("typescriptreact"),
        Some("js") => Some("javascript"),
        Some("jsx") => Some("javascriptreact"),
        Some("json") => Some("json"),
        Some("toml") => Some("toml"),
        Some("yaml") | Some("yml") => Some("yaml"),
        Some("md") => Some("markdown"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_fake_binary(dir: &Path, name: &str, script: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    fn tempdir_in_tmp() -> tempfile::TempDir {
        crate::test_helpers::test_tempdir("djinn-lsp-")
    }

    #[test]
    fn server_for_rust_file() {
        let s = server_for_path(Path::new("/foo/bar.rs")).unwrap();
        assert_eq!(s.id, "rust-analyzer");
    }

    #[test]
    fn server_for_ts_file() {
        let s = server_for_path(Path::new("/foo/bar.ts")).unwrap();
        assert_eq!(s.id, "typescript-language-server");
    }

    #[test]
    fn server_for_tsx_file() {
        let s = server_for_path(Path::new("/foo/bar.tsx")).unwrap();
        assert_eq!(s.id, "typescript-language-server");
    }

    #[test]
    fn server_for_go_file() {
        let s = server_for_path(Path::new("/foo/bar.go")).unwrap();
        assert_eq!(s.id, "gopls");
    }

    #[test]
    fn server_for_python_file() {
        let s = server_for_path(Path::new("/foo/bar.py")).unwrap();
        assert_eq!(s.id, "pyright");
    }

    #[test]
    fn server_for_unknown_extension() {
        assert!(server_for_path(Path::new("/foo/bar.txt")).is_none());
        assert!(server_for_path(Path::new("/foo/bar")).is_none());
    }

    #[test]
    fn language_id_mappings() {
        assert_eq!(language_id_for_path(Path::new("a.rs")), Some("rust"));
        assert_eq!(language_id_for_path(Path::new("a.go")), Some("go"));
        assert_eq!(language_id_for_path(Path::new("a.py")), Some("python"));
        assert_eq!(language_id_for_path(Path::new("a.ts")), Some("typescript"));
        assert_eq!(
            language_id_for_path(Path::new("a.tsx")),
            Some("typescriptreact")
        );
        assert_eq!(language_id_for_path(Path::new("a.js")), Some("javascript"));
        assert_eq!(
            language_id_for_path(Path::new("a.jsx")),
            Some("javascriptreact")
        );
        assert_eq!(language_id_for_path(Path::new("a.json")), Some("json"));
        assert_eq!(language_id_for_path(Path::new("a.toml")), Some("toml"));
        assert_eq!(language_id_for_path(Path::new("a.yaml")), Some("yaml"));
        assert_eq!(language_id_for_path(Path::new("a.yml")), Some("yaml"));
        assert_eq!(language_id_for_path(Path::new("a.md")), Some("markdown"));
        assert_eq!(language_id_for_path(Path::new("a.txt")), None);
    }

    #[test]
    fn which_in_path_finds_existing_binary() {
        let tmp = crate::test_helpers::test_tempdir("djinn-lsp-which-");
        make_fake_binary(tmp.path(), "ls", "#!/bin/sh\n");
        let result = which_in_path("ls", &tmp.path().to_string_lossy());
        assert_eq!(result, Some(tmp.path().join("ls")));
    }

    #[test]
    fn which_in_path_returns_none_for_missing() {
        let result = which_in_path("definitely_not_a_real_binary_xyz", "/usr/bin");
        assert!(result.is_none());
    }

    #[test]
    fn which_in_path_scans_multiple_dirs() {
        let tmp = crate::test_helpers::test_tempdir("djinn-lsp-which-");
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        make_fake_binary(&second, "ls", "#!/bin/sh\n");
        let path = format!("{}:{}", first.display(), second.display());
        let result = which_in_path("ls", &path);
        assert_eq!(result, Some(second.join("ls")));
    }

    #[test]
    fn resolve_binary_finds_existing_on_path() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let path_dir = tmp.path().join("fakepath");
        std::fs::create_dir_all(&path_dir).unwrap();
        make_fake_binary(&path_dir, "typescript-language-server", "#!/bin/sh\n");

        let server = server_for_path(Path::new("foo.ts")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, &path_dir.to_string_lossy());

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), path_dir.join("typescript-language-server"));
    }

    #[test]
    fn resolve_binary_finds_existing_in_bin_dir() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        make_fake_binary(&bin_dir, "rust-analyzer", "#!/bin/sh\n");

        let server = server_for_path(Path::new("foo.rs")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, "");

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), bin_dir.join("rust-analyzer"));
    }

    #[test]
    fn resolve_binary_npm_not_found_errors() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");

        let server = server_for_path(Path::new("foo.ts")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, "");

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("npm not found"));
    }

    #[test]
    fn resolve_binary_go_not_found_errors() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");

        let server = server_for_path(Path::new("foo.go")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, "");

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("go not found"));
    }

    #[test]
    fn resolve_binary_rust_no_rustup_errors_with_help() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");

        let server = server_for_path(Path::new("foo.rs")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, "");

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("rust-analyzer not found"), "got: {err}");
        assert!(err.contains("rustup component add"), "got: {err}");
    }

    #[test]
    fn resolve_binary_npm_installs_successfully() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");
        let path_dir = tmp.path().join("fakepath");
        std::fs::create_dir_all(&path_dir).unwrap();

        let install_target = bin_dir.join("bin");
        let script = format!(
            "#!/bin/sh\nmkdir -p '{}'\ntouch '{}/typescript-language-server'\nchmod +x '{}/typescript-language-server'\n",
            install_target.display(),
            install_target.display(),
            install_target.display(),
        );
        make_fake_binary(&path_dir, "npm", &script);

        let server = server_for_path(Path::new("foo.ts")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, &path_dir.to_string_lossy());

        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert!(result.unwrap().ends_with("typescript-language-server"));
    }

    #[test]
    fn resolve_binary_npm_failure_returns_error() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");
        let path_dir = tmp.path().join("fakepath");
        std::fs::create_dir_all(&path_dir).unwrap();

        make_fake_binary(&path_dir, "npm", "#!/bin/sh\necho 'ERR!' >&2\nexit 1\n");

        let server = server_for_path(Path::new("foo.ts")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, &path_dir.to_string_lossy());

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("npm install failed"));
    }

    #[test]
    fn resolve_binary_go_installs_successfully() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");
        let path_dir = tmp.path().join("fakepath");
        std::fs::create_dir_all(&path_dir).unwrap();

        let script = format!(
            "#!/bin/sh\nmkdir -p '{}'\ntouch '{}/gopls'\nchmod +x '{}/gopls'\n",
            bin_dir.display(),
            bin_dir.display(),
            bin_dir.display(),
        );
        make_fake_binary(&path_dir, "go", &script);

        let server = server_for_path(Path::new("foo.go")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, &path_dir.to_string_lossy());

        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert!(result.unwrap().ends_with("gopls"));
    }

    #[test]
    fn resolve_binary_rustup_installs_successfully() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");
        let path_dir = tmp.path().join("fakepath");
        std::fs::create_dir_all(&path_dir).unwrap();

        let rustup_bin_dir = tmp.path().join("rustup_bin");
        std::fs::create_dir_all(&rustup_bin_dir).unwrap();
        let script = format!(
            "#!/bin/sh\ntouch '{}/rust-analyzer'\nchmod +x '{}/rust-analyzer'\n",
            rustup_bin_dir.display(),
            rustup_bin_dir.display(),
        );
        make_fake_binary(&path_dir, "rustup", &script);

        let server = server_for_path(Path::new("foo.rs")).unwrap();
        let sys_path = format!("{}:{}", path_dir.display(), rustup_bin_dir.display());
        let result = resolve_binary_inner(&server, &bin_dir, &sys_path);

        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert!(result.unwrap().ends_with("rust-analyzer"));
    }

    #[test]
    fn resolve_binary_rustup_which_fallback_when_not_on_path() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");
        let path_dir = tmp.path().join("fakepath");
        std::fs::create_dir_all(&path_dir).unwrap();

        let toolchain_bin = tmp.path().join("toolchain_bin");
        std::fs::create_dir_all(&toolchain_bin).unwrap();
        make_fake_binary(&toolchain_bin, "rust-analyzer", "#!/bin/sh\n");

        let ra_path = toolchain_bin.join("rust-analyzer");
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"which\" ]; then echo '{}'; else true; fi\n",
            ra_path.display(),
        );
        make_fake_binary(&path_dir, "rustup", &script);

        let server = server_for_path(Path::new("foo.rs")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, &path_dir.to_string_lossy());

        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert_eq!(result.unwrap(), ra_path);
    }

    #[test]
    fn resolve_binary_rustup_which_finds_existing_without_install() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let path_dir = tmp.path().join("fakepath");
        std::fs::create_dir_all(&path_dir).unwrap();

        let toolchain_bin = tmp.path().join("toolchain_bin");
        std::fs::create_dir_all(&toolchain_bin).unwrap();
        let ra_path = make_fake_binary(&toolchain_bin, "rust-analyzer", "#!/bin/sh\n");

        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"which\" ]; then echo '{}'; else exit 1; fi\n",
            ra_path.display(),
        );
        make_fake_binary(&path_dir, "rustup", &script);

        let server = server_for_path(Path::new("foo.rs")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, &path_dir.to_string_lossy());

        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert_eq!(result.unwrap(), ra_path);
    }
}
