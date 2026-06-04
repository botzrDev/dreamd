#!/usr/bin/env node
'use strict';

const https = require('https');
const fs = require('fs');
const path = require('path');
const os = require('os');
const crypto = require('crypto');
const { execFileSync, spawnSync } = require('child_process');
const zlib = require('zlib');

const VERSION = '0.1.0-rc.1';
const MANIFEST = require('../manifest.json');

function getPlatformTarget() {
  const { platform, arch } = process;
  if (platform === 'linux' && arch === 'x64') return 'linux-x86_64';
  if (platform === 'linux' && arch === 'arm64') return 'linux-aarch64';
  if (platform === 'darwin' && arch === 'x64') return 'darwin-x86_64';
  if (platform === 'darwin' && arch === 'arm64') return 'darwin-aarch64';
  if (platform === 'win32' && arch === 'x64') return 'windows-x86_64';
  return null;
}

function getCacheDir() {
  if (process.platform === 'win32') {
    const localAppData = process.env.LOCALAPPDATA || os.homedir();
    return path.join(localAppData, 'dreamd-mcp', 'cache', VERSION);
  }
  return path.join(os.homedir(), '.cache', 'dreamd-mcp', VERSION);
}

function getBinaryName() {
  return process.platform === 'win32' ? 'dreamd.exe' : 'dreamd';
}

function sha256File(filePath) {
  const hash = crypto.createHash('sha256');
  hash.update(fs.readFileSync(filePath));
  return hash.digest('hex');
}

function verifyBinary(binaryPath, expectedSha256) {
  if (expectedSha256 === '0000000000000000000000000000000000000000000000000000000000000000') {
    // Pre-release placeholder — verification intentionally skipped.
    // Replace with real hashes in manifest.json when release binaries are cut (v0.1 launch).
    return;
  }
  const actual = sha256File(binaryPath);
  if (actual !== expectedSha256) {
    process.stderr.write(`[dreamd-mcp] sha256 mismatch!\n  expected: ${expectedSha256}\n  actual:   ${actual}\n`);
    process.stderr.write('[dreamd-mcp] Cached binary may be corrupt. Delete ~/.cache/dreamd-mcp and retry.\n');
    process.exit(1);
  }
}

function downloadFile(url, destPath) {
  return new Promise((resolve, reject) => {
    const file = fs.createWriteStream(destPath);
    function get(u) {
      https.get(u, (res) => {
        // GitHub Releases redirects the download to S3; follow one hop.
        if (res.statusCode === 301 || res.statusCode === 302) {
          return get(res.headers.location);
        }
        if (res.statusCode !== 200) {
          return reject(new Error(`HTTP ${res.statusCode} fetching ${u}`));
        }
        res.pipe(file);
        file.on('finish', () => file.close(resolve));
        file.on('error', reject);
      }).on('error', reject);
    }
    get(url);
  });
}

function extractTarGz(archivePath, destDir) {
  const result = spawnSync('tar', ['-xzf', archivePath, '-C', destDir], { stdio: 'inherit' });
  if (result.status !== 0) {
    throw new Error(`tar extraction failed with status ${result.status}`);
  }
}

async function ensureBinary(target, binaryPath, expectedSha256) {
  const cacheDir = path.dirname(binaryPath);
  fs.mkdirSync(cacheDir, { recursive: true });

  const archiveName = `${target}.tar.gz`;
  const archivePath = path.join(cacheDir, archiveName);
  const downloadUrl = `https://github.com/botzrDev/dreamd/releases/download/v${VERSION}/${archiveName}`;

  process.stderr.write(`[dreamd-mcp] Downloading dreamd v${VERSION} for ${target}...\n`);
  await downloadFile(downloadUrl, archivePath);

  process.stderr.write('[dreamd-mcp] Extracting...\n');
  extractTarGz(archivePath, cacheDir);
  fs.unlinkSync(archivePath);

  if (!fs.existsSync(binaryPath)) {
    throw new Error(`Binary not found at ${binaryPath} after extraction`);
  }
  fs.chmodSync(binaryPath, 0o755);
}

async function main() {
  const args = process.argv.slice(2);
  const dreamdArgs = args[0] === 'init' ? args : ['mcp', ...args];

  // DREAMD_BIN override: skip download, exec directly
  if (process.env.DREAMD_BIN) {
    const overridePath = process.env.DREAMD_BIN;
    const target = getPlatformTarget();
    if (target && MANIFEST.binaries[target]) {
      const expected = MANIFEST.binaries[target].sha256;
      if (expected !== '0000000000000000000000000000000000000000000000000000000000000000') {
        process.stderr.write('[dreamd-mcp] WARNING: DREAMD_BIN set — skipping sha256 verification for custom build\n');
      }
    }
    execFileSync(overridePath, dreamdArgs, { stdio: 'inherit' });
    return;
  }

  const target = getPlatformTarget();
  if (!target) {
    process.stderr.write(
      `No prebuilt binary for ${process.platform}/${process.arch}. Install via:\n` +
      `  cargo install dreamd\n` +
      `Then set DREAMD_BIN=/path/to/dreamd and re-run.\n`
    );
    process.exit(1);
  }

  const binaryEntry = MANIFEST.binaries[target];
  if (!binaryEntry) {
    process.stderr.write(
      `No prebuilt binary for ${target}. Install via:\n` +
      `  cargo install dreamd\n` +
      `Then set DREAMD_BIN=/path/to/dreamd and re-run.\n`
    );
    process.exit(1);
  }

  const cacheDir = getCacheDir();
  const binaryName = getBinaryName();
  const binaryPath = path.join(cacheDir, binaryName);

  if (!fs.existsSync(binaryPath)) {
    await ensureBinary(target, binaryPath, binaryEntry.sha256);
  }

  // Verify sha256 before every exec (not just on download)
  verifyBinary(binaryPath, binaryEntry.sha256);

  execFileSync(binaryPath, dreamdArgs, { stdio: 'inherit' });
}

main().catch((err) => {
  process.stderr.write(`[dreamd-mcp] Fatal error: ${err.message}\n`);
  process.exit(1);
});
