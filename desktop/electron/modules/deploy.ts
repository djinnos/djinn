/**
 * Remote deployment of djinn-server to SSH hosts.
 *
 * Ported from src-tauri/src/deploy.rs
 *
 * Uploads the server binary via `scp` and makes it executable.
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { execSync } from "node:child_process";
import { homedir } from "node:os";
import { sshExec, type SshHost, updateHost } from "./ssh.js";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface GitHubRelease {
  tag_name: string;
  assets: GitHubAsset[];
}

interface GitHubAsset {
  name: string;
  browser_download_url: string;
}

/** Result of a deployment operation. */
export interface DeployResult {
  /** The version string reported by the deployed binary. */
  version: string;
  /** Remote architecture (e.g. "x86_64", "aarch64"). */
  arch: string;
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const GITHUB_REPO = "djinnos/djinn";

// ---------------------------------------------------------------------------
// Download helper
// ---------------------------------------------------------------------------

/**
 * Download the djinn-server binary for the given OS/arch from the latest
 * GitHub release whose tag starts with `server-v`.
 */
async function downloadServerBinary(os: string, arch: string): Promise<string> {
  // Normalize arch
  const lowerArch = arch.toLowerCase();
  let normArch: string;
  if (["x86_64", "amd64", "x64"].includes(lowerArch)) {
    normArch = "x64";
  } else if (["aarch64", "arm64"].includes(lowerArch)) {
    normArch = "arm64";
  } else {
    throw new Error(`Unsupported architecture: ${arch}`);
  }

  // Normalize OS
  let normOs: string;
  if (os.toLowerCase() === "linux") {
    normOs = "linux";
  } else if (os.toLowerCase() === "darwin") {
    normOs = "macos";
  } else {
    throw new Error(`Unsupported OS: ${os}`);
  }

  const resp = await fetch(
    `https://api.github.com/repos/${GITHUB_REPO}/releases?per_page=20`,
    { headers: { "User-Agent": "djinn-desktop" } },
  );
  if (!resp.ok) {
    throw new Error(`Failed to fetch GitHub releases: ${resp.status}`);
  }

  const releases = (await resp.json()) as GitHubRelease[];
  const release = releases.find((r) => r.tag_name.startsWith("server-v"));
  if (!release) {
    throw new Error("No server-v* release found on GitHub");
  }

  const assetName = `djinn-server-${normOs}-${normArch}`;
  const asset = release.assets.find((a) => a.name === assetName);
  if (!asset) {
    throw new Error(
      `No asset named '${assetName}' in release ${release.tag_name}`,
    );
  }

  const downloadDir = path.join(homedir(), ".djinn", "downloads");
  fs.mkdirSync(downloadDir, { recursive: true });

  const dest = path.join(downloadDir, assetName);

  console.log(`Downloading ${assetName} from ${asset.browser_download_url}`);

  const dlResp = await fetch(asset.browser_download_url, {
    headers: { "User-Agent": "djinn-desktop" },
  });
  if (!dlResp.ok) {
    throw new Error(`Failed to download asset: ${dlResp.status}`);
  }

  const bytes = Buffer.from(await dlResp.arrayBuffer());
  fs.writeFileSync(dest, bytes);

  console.log(`Downloaded ${assetName} to ${dest}`);
  return dest;
}

// ---------------------------------------------------------------------------
// SCP upload
// ---------------------------------------------------------------------------

/** Upload a local file to the remote host via `scp`. */
function scpUpload(host: SshHost, localPath: string, remote: string): void {
  const args: string[] = [
    "-P",
    host.port.toString(),
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=accept-new",
  ];

  if (host.key_path) {
    args.push("-i", host.key_path);
  }

  args.push(localPath);
  args.push(`${host.user}@${host.hostname}:${remote}`);

  console.log(
    `scp ${localPath} -> ${host.user}@${host.hostname}:${remote}`,
  );

  try {
    execSync(`scp ${args.map(shellEscape).join(" ")}`, {
      encoding: "utf-8",
      stdio: ["pipe", "pipe", "pipe"],
    });
  } catch (err: unknown) {
    const e = err as { stderr?: string };
    const stderr = (e.stderr ?? "").trim();
    throw new Error(`scp failed: ${stderr}`);
  }
}

// ---------------------------------------------------------------------------
// Local binary fallback
// ---------------------------------------------------------------------------

/** Locate the djinn-server binary on the local machine for deployment. */
function findLocalServerBinary(): string {
  // Check DJINN_SERVER_BIN env var first.
  const envPath = process.env.DJINN_SERVER_BIN;
  if (envPath && fs.existsSync(envPath)) {
    return envPath;
  }

  // Search PATH.
  const pathVar = process.env.PATH ?? "";
  for (const dir of pathVar.split(path.delimiter)) {
    const candidate = path.join(dir, "djinn-server");
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }

  throw new Error(
    "djinn-server binary not found locally. Cannot deploy to remote host. " +
      "Set DJINN_SERVER_BIN or ensure djinn-server is in PATH.",
  );
}

// ---------------------------------------------------------------------------
// Deploy
// ---------------------------------------------------------------------------

/**
 * Deploy djinn-server to a remote host.
 *
 * Steps:
 * 1. Detect remote architecture via `uname -m`.
 * 2. Detect remote OS via `uname -s`.
 * 3. Create the remote directory `~/.djinn/bin/`.
 * 4. Download binary for remote platform from GitHub releases (or fall back to local).
 * 5. Upload the binary via `scp`.
 * 6. Make it executable.
 * 7. Check for missing shared libraries.
 * 8. Verify by running `--version`.
 */
export async function deployToHost(host: SshHost): Promise<DeployResult> {
  // 1. Detect remote architecture.
  console.log(
    `Deploy step 1/8: detecting remote architecture for ${host.label}`,
  );
  const arch = sshExec(host, "uname -m").trim();
  console.log(`Remote architecture for ${host.label}: ${arch}`);

  // 2. Detect remote OS.
  console.log(`Deploy step 2/8: detecting remote OS for ${host.label}`);
  const os = sshExec(host, "uname -s").trim();
  console.log(`Remote OS for ${host.label}: ${os}`);

  // 3. Create remote directory.
  console.log("Deploy step 3/8: creating remote directory");
  sshExec(host, "mkdir -p ~/.djinn/bin");

  // 4. Obtain binary: try GitHub releases, fall back to local.
  console.log("Deploy step 4/8: obtaining djinn-server binary");
  let localBinary: string;
  try {
    localBinary = await downloadServerBinary(os, arch);
    console.log(`Downloaded binary from GitHub: ${localBinary}`);
  } catch (e) {
    console.warn(
      `Failed to download from GitHub releases: ${e}; falling back to local binary`,
    );
    localBinary = findLocalServerBinary();
  }
  console.log(`Using binary: ${localBinary}`);

  // 5. Upload via scp.
  console.log("Deploy step 5/8: uploading binary via scp");
  const remotePath = "~/.djinn/bin/djinn-server";
  scpUpload(host, localBinary, remotePath);

  // 6. Make executable.
  console.log("Deploy step 6/8: setting executable permission");
  sshExec(host, "chmod +x ~/.djinn/bin/djinn-server");

  // 7. Check for missing shared libraries.
  console.log("Deploy step 7/8: checking shared library dependencies");
  let lddOutput: string;
  try {
    lddOutput = sshExec(host, "ldd ~/.djinn/bin/djinn-server 2>&1 || true");
  } catch {
    lddOutput = "";
  }

  const missingLibs = lddOutput
    .split("\n")
    .filter((line) => line.includes("not found"));

  if (missingLibs.length > 0) {
    const libsList = missingLibs
      .map((l) => l.trim().split(/\s+/)[0])
      .filter(Boolean)
      .join(", ");
    throw new Error(
      `Missing shared libraries on remote: ${libsList}\n\n` +
        "Either install them on the remote (Ubuntu/Debian):\n" +
        "sudo apt-get install -y libgit2-dev libssl-dev libssh2-1-dev\n\n" +
        "Or build a portable binary with vendored libs:\n" +
        "cargo build -p djinn-server --release --features vendored\n" +
        "Then set DJINN_SERVER_BIN to the release binary path and redeploy.",
    );
  }

  // 8. Verify.
  console.log("Deploy step 8/8: verifying installation");
  const version = sshExec(host, "~/.djinn/bin/djinn-server --version").trim();
  console.log(`Deployed djinn-server to ${host.label}: ${version}`);

  // Update host record with deployment info.
  const updated: SshHost = { ...host, deployed: true, server_version: version };
  updateHost(updated);

  return { version, arch };
}

/** Check the djinn-server version installed on a remote host. */
export function checkRemoteVersion(host: SshHost): string | null {
  try {
    const output = sshExec(
      host,
      "~/.djinn/bin/djinn-server --version 2>/dev/null",
    ).trim();
    return output || null;
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function shellEscape(s: string): string {
  if (/^[a-zA-Z0-9_./:@=,-]+$/.test(s)) {
    return s;
  }
  return `'${s.replace(/'/g, "'\\''")}'`;
}
