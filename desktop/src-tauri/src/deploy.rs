//! Remote deployment of djinn-server to SSH hosts.
//!
//! Uploads the server binary via `scp` and makes it executable.

use crate::ssh_hosts::SshHost;
use crate::ssh_tunnel::ssh_exec;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

/// Result of a deployment operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployResult {
    /// The version string reported by the deployed binary.
    pub version: String,
    /// Remote architecture (e.g. "x86_64", "aarch64").
    pub arch: String,
}

/// Download the djinn-server binary for the given OS/arch from the latest
/// GitHub release whose tag starts with `server-v`.
async fn download_server_binary(os: &str, arch: &str) -> Result<PathBuf, String> {
    let client = reqwest::Client::new();
    let releases: Vec<GitHubRelease> = client
        .get("https://api.github.com/repos/djinnos/djinn/releases?per_page=20")
        .header("User-Agent", "djinn-desktop")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch GitHub releases: {e}"))?
        .json()
        .await
        .map_err(|e| format!("Failed to parse GitHub releases: {e}"))?;

    let release = releases
        .iter()
        .find(|r| r.tag_name.starts_with("server-v"))
        .ok_or_else(|| "No server-v* release found on GitHub".to_string())?;

    // Normalize arch
    let norm_arch = match arch.to_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => "x64",
        "aarch64" | "arm64" => "arm64",
        other => return Err(format!("Unsupported architecture: {other}")),
    };

    // Normalize OS
    let norm_os = if os.eq_ignore_ascii_case("linux") {
        "linux"
    } else if os.eq_ignore_ascii_case("darwin") {
        "macos"
    } else {
        return Err(format!("Unsupported OS: {os}"));
    };

    let asset_name = format!("djinn-server-{norm_os}-{norm_arch}");
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| {
            format!(
                "No asset named '{asset_name}' in release {}",
                release.tag_name
            )
        })?;

    let home = dirs::home_dir().ok_or_else(|| "Cannot determine home directory".to_string())?;
    let download_dir = home.join(".djinn").join("downloads");
    std::fs::create_dir_all(&download_dir)
        .map_err(|e| format!("Failed to create downloads directory: {e}"))?;

    let dest = download_dir.join(&asset_name);

    log::info!(
        "Downloading {} from {}",
        asset_name,
        asset.browser_download_url
    );

    let bytes = client
        .get(&asset.browser_download_url)
        .header("User-Agent", "djinn-desktop")
        .send()
        .await
        .map_err(|e| format!("Failed to download asset: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("Failed to read asset bytes: {e}"))?;

    std::fs::write(&dest, &bytes)
        .map_err(|e| format!("Failed to write downloaded binary: {e}"))?;

    log::info!("Downloaded {} to {}", asset_name, dest.display());
    Ok(dest)
}

/// Deploy djinn-server to a remote host.
///
/// Steps:
/// 1. Detect remote architecture via `uname -m`.
/// 2. Create the remote directory `~/.djinn/bin/`.
/// 3. Upload the binary via `scp`.
/// 4. Make it executable.
/// 5. Verify by running `--version`.
pub async fn deploy_to_host(host: &SshHost) -> Result<DeployResult, String> {
    // 1. Detect remote architecture.
    log::info!("Deploy step 1/7: detecting remote architecture for {}", host.label);
    let arch = ssh_exec(host, "uname -m")
        .map_err(|e| format!("Failed to detect remote architecture: {e}"))?;
    let arch = arch.trim().to_string();
    log::info!("Remote architecture for {}: {}", host.label, arch);

    // 2. Detect remote OS.
    log::info!("Deploy step 2/7: detecting remote OS for {}", host.label);
    let os = ssh_exec(host, "uname -s")
        .map_err(|e| format!("Failed to detect remote OS: {e}"))?;
    let os = os.trim().to_string();
    log::info!("Remote OS for {}: {}", host.label, os);

    // 3. Create remote directory.
    log::info!("Deploy step 3/7: creating remote directory");
    ssh_exec(host, "mkdir -p ~/.djinn/bin")
        .map_err(|e| format!("Failed to create remote directory: {e}"))?;

    // 4. Try downloading from GitHub releases, fall back to local binary.
    log::info!("Deploy step 4/7: obtaining djinn-server binary");
    let local_binary = match download_server_binary(&os, &arch).await {
        Ok(path) => {
            log::info!("Downloaded binary from GitHub: {}", path.display());
            path
        }
        Err(e) => {
            log::warn!("Failed to download from GitHub releases: {e}; falling back to local binary");
            find_local_server_binary()?
        }
    };
    log::info!("Using binary: {}", local_binary.display());

    // 5. Upload via scp.
    log::info!("Deploy step 5/7: uploading binary via scp");
    let remote_path = "~/.djinn/bin/djinn-server";
    scp_upload(host, &local_binary, remote_path)
        .map_err(|e| format!("Failed to upload binary: {e}"))?;

    // 6. Make executable.
    log::info!("Deploy step 6/7: setting executable permission");
    ssh_exec(host, "chmod +x ~/.djinn/bin/djinn-server")
        .map_err(|e| format!("Failed to chmod: {e}"))?;

    // 7. Check for missing shared libraries.
    log::info!("Deploy step 7/8: checking shared library dependencies");
    let ldd_output = ssh_exec(host, "ldd ~/.djinn/bin/djinn-server 2>&1 || true")
        .unwrap_or_default();
    let missing_libs: Vec<&str> = ldd_output
        .lines()
        .filter(|l| l.contains("not found"))
        .collect();

    if !missing_libs.is_empty() {
        let libs_list = missing_libs
            .iter()
            .filter_map(|l| l.split_whitespace().next())
            .map(|s| s.trim_start_matches('\t'))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "Missing shared libraries on remote: {libs_list}\n\n\
             Either install them on the remote (Ubuntu/Debian):\n\
             sudo apt-get install -y libgit2-dev libssl-dev libssh2-1-dev\n\n\
             Or build a portable binary with vendored libs:\n\
             cargo build -p djinn-server --release --features vendored\n\
             Then set DJINN_SERVER_BIN to the release binary path and redeploy."
        ));
    }

    // 8. Verify.
    log::info!("Deploy step 8/8: verifying installation");
    let version = ssh_exec(host, "~/.djinn/bin/djinn-server --version")
        .map_err(|e| format!("Binary uploaded but failed to run: {e}"))?;
    let version = version.trim().to_string();
    log::info!("Deployed djinn-server to {}: {}", host.label, version);

    Ok(DeployResult { version, arch })
}

/// Check the djinn-server version installed on a remote host.
#[allow(dead_code)]
pub async fn check_remote_version(host: &SshHost) -> Result<Option<String>, String> {
    match ssh_exec(host, "~/.djinn/bin/djinn-server --version 2>/dev/null") {
        Ok(output) => {
            let v = output.trim().to_string();
            if v.is_empty() {
                Ok(None)
            } else {
                Ok(Some(v))
            }
        }
        Err(_) => Ok(None),
    }
}

/// Upload a local file to the remote host via `scp`.
fn scp_upload(host: &SshHost, local: &Path, remote: &str) -> Result<(), String> {
    let mut cmd = std::process::Command::new("scp");
    cmd.arg("-P").arg(host.port.to_string());
    cmd.arg("-o").arg("BatchMode=yes");
    cmd.arg("-o").arg("StrictHostKeyChecking=accept-new");

    if let Some(ref key) = host.key_path {
        cmd.arg("-i").arg(key);
    }

    cmd.arg(local.to_string_lossy().as_ref());
    cmd.arg(format!("{}@{}:{}", host.user, host.hostname, remote));

    log::info!(
        "scp {} -> {}@{}:{}",
        local.display(),
        host.user,
        host.hostname,
        remote
    );

    let output = cmd
        .output()
        .map_err(|e| format!("Failed to execute scp: {}", e))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        Err(format!("scp failed: {}", stderr.trim()))
    }
}

/// Locate the djinn-server binary on the local machine for deployment.
fn find_local_server_binary() -> Result<std::path::PathBuf, String> {
    // Check DJINN_SERVER_BIN env var first.
    if let Ok(path) = std::env::var("DJINN_SERVER_BIN") {
        let p = std::path::PathBuf::from(&path);
        if p.exists() {
            return Ok(p);
        }
    }

    // Check next to the running executable.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("djinn-server");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    // Search PATH.
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("djinn-server");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    Err(
        "djinn-server binary not found locally. Cannot deploy to remote host. \
         Set DJINN_SERVER_BIN or ensure djinn-server is in PATH."
            .to_string(),
    )
}
