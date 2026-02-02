#!/usr/bin/env node

const https = require('https');
const fs = require('fs');
const path = require('path');
const { execSync } = require('child_process');
const zlib = require('zlib');
const tar = require('tar');

const REPO = 'djinnos/djinn';
const BINARY_NAME = 'djinn';

function getPlatform() {
  const platform = process.platform;
  switch (platform) {
    case 'darwin': return 'darwin';
    case 'linux': return 'linux';
    case 'win32': return 'windows';
    default: throw new Error(`Unsupported platform: ${platform}`);
  }
}

function getArch() {
  const arch = process.arch;
  switch (arch) {
    case 'x64': return 'amd64';
    case 'arm64': return 'arm64';
    default: throw new Error(`Unsupported architecture: ${arch}`);
  }
}

async function getLatestVersion() {
  return new Promise((resolve, reject) => {
    const options = {
      hostname: 'api.github.com',
      path: `/repos/${REPO}/releases/latest`,
      headers: { 'User-Agent': 'djinn-npm-installer' }
    };

    https.get(options, (res) => {
      let data = '';
      res.on('data', chunk => data += chunk);
      res.on('end', () => {
        try {
          const release = JSON.parse(data);
          const version = release.tag_name.replace(/^v/, '');
          resolve(version);
        } catch (e) {
          reject(new Error('Failed to parse release info'));
        }
      });
    }).on('error', reject);
  });
}

async function downloadFile(url, dest) {
  return new Promise((resolve, reject) => {
    const file = fs.createWriteStream(dest);
    
    const request = (url) => {
      https.get(url, { headers: { 'User-Agent': 'djinn-npm-installer' } }, (res) => {
        if (res.statusCode === 302 || res.statusCode === 301) {
          request(res.headers.location);
          return;
        }
        
        if (res.statusCode !== 200) {
          reject(new Error(`Download failed with status ${res.statusCode}`));
          return;
        }
        
        res.pipe(file);
        file.on('finish', () => {
          file.close();
          resolve();
        });
      }).on('error', (err) => {
        fs.unlink(dest, () => {});
        reject(err);
      });
    };
    
    request(url);
  });
}

async function extractTarGz(archivePath, destDir) {
  return new Promise((resolve, reject) => {
    fs.createReadStream(archivePath)
      .pipe(zlib.createGunzip())
      .pipe(tar.extract({ cwd: destDir }))
      .on('finish', resolve)
      .on('error', reject);
  });
}

async function extractZip(archivePath, destDir) {
  // For Windows, use PowerShell to extract
  try {
    execSync(`powershell -command "Expand-Archive -Path '${archivePath}' -DestinationPath '${destDir}' -Force"`, {
      stdio: 'pipe'
    });
  } catch (e) {
    throw new Error('Failed to extract zip archive');
  }
}

async function main() {
  try {
    const platform = getPlatform();
    const arch = getArch();
    
    console.log(`Detected platform: ${platform}/${arch}`);
    
    const version = await getLatestVersion();
    console.log(`Installing djinn v${version}...`);
    
    const ext = platform === 'windows' ? 'zip' : 'tar.gz';
    const archiveName = `${BINARY_NAME}_${version}_${platform}_${arch}.${ext}`;
    const downloadUrl = `https://github.com/${REPO}/releases/download/v${version}/${archiveName}`;
    
    const binDir = path.join(__dirname, '..', 'bin');
    const tmpDir = path.join(__dirname, '..', '.tmp');
    const archivePath = path.join(tmpDir, archiveName);
    
    // Create directories
    fs.mkdirSync(binDir, { recursive: true });
    fs.mkdirSync(tmpDir, { recursive: true });
    
    console.log(`Downloading from ${downloadUrl}...`);
    await downloadFile(downloadUrl, archivePath);
    
    console.log('Extracting...');
    if (ext === 'zip') {
      await extractZip(archivePath, tmpDir);
    } else {
      await extractTarGz(archivePath, tmpDir);
    }
    
    // Move binary to bin directory
    const binaryExt = platform === 'windows' ? '.exe' : '';
    const srcBinary = path.join(tmpDir, `${BINARY_NAME}${binaryExt}`);
    const destBinary = path.join(binDir, `${BINARY_NAME}${binaryExt}`);
    
    fs.copyFileSync(srcBinary, destBinary);
    fs.chmodSync(destBinary, 0o755);
    
    // Cleanup
    fs.rmSync(tmpDir, { recursive: true, force: true });
    
    console.log(`Successfully installed djinn v${version}`);
  } catch (error) {
    console.error('Installation failed:', error.message);
    process.exit(1);
  }
}

main();
