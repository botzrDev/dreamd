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

// Native `dreamd` subcommands. KEEP IN SYNC with the clap `Command` enum in
// crates/dreamd-cli/src/cli.rs — adding a `dreamd` subcommand without adding it
// here silently routes `npx dreamd-mcp <newcmd>` to `dreamd mcp <newcmd>`.
const DREAMD_SUBCOMMANDS = new Set([
  'init', 'watch', 'doctor', 'dream', 'reset', 'version', 'mcp',
]);

// A recognized first token is forwarded to `dreamd` verbatim; a bare invocation
// (no args -> args[0] === undefined) or an unrecognized token defaults to
// `dreamd mcp` so IDEs spawning `npx dreamd-mcp` get the MCP server over stdio.
function resolveDreamdArgs(args) {
  return DREAMD_SUBCOMMANDS.has(args[0]) ? args : ['mcp', ...args];
}

// Leading top-level flags the shim answers itself (no binary download). Only the
// FIRST token counts: `npx dreamd-mcp init --help` still routes to the binary.
// We handle these in the shim rather than forwarding because `dreamd mcp <flag>`
// is a clap error, and the native `dreamd --help` prints `dreamd` usage — wrong
// invocation for an npx user.
function topLevelFlag(args) {
  if (args[0] === '--version' || args[0] === '-V') return 'version';
  if (args[0] === '--help' || args[0] === '-h') return 'help';
  return null;
}

const HELP_TEXT = `dreamd-mcp — npx shim for the dreamd MCP server

Usage:
  npx dreamd-mcp [<command> [args...]]

With no command, starts the MCP server over stdio — the invocation MCP-aware
IDEs (Claude Code, Cursor, ...) spawn. A recognized command is forwarded
verbatim to the native dreamd binary:
  ${[...DREAMD_SUBCOMMANDS].sort().join(', ')}

Examples:
  npx dreamd-mcp            start the MCP server (IDE invocation)
  npx dreamd-mcp init       scaffold .agent/ into the current project
  npx dreamd-mcp watch      start the shared daemon (one serialized writer)
  npx dreamd-mcp version    print the dreamd binary's full build info

Options:
  -V, --version            print the dreamd-mcp version
  -h, --help               print this help

Environment:
  DREAMD_BIN=<path>        dev-only: run a local build (skips sha256 verification)
`;

function getPlatformTarget() {
  const { platform, arch } = process;
  if (platform === 'linux' && arch === 'x64') return 'linux-x86_64';
  if (platform === 'darwin' && arch === 'x64') return 'darwin-x86_64';
  if (platform === 'darwin' && arch === 'arm64') return 'darwin-aarch64';
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

  // Leading --version/--help are answered here without touching the binary.
  switch (topLevelFlag(args)) {
    case 'version':
      process.stdout.write(`dreamd-mcp ${VERSION}\n`);
      return;
    case 'help':
      process.stdout.write(HELP_TEXT);
      return;
  }

  const dreamdArgs = resolveDreamdArgs(args);

  // DREAMD_BIN override: dev-only — skip download and sha256 verification
  if (process.env.DREAMD_BIN) {
    process.stderr.write(
      '[dreamd-mcp] WARNING: DREAMD_BIN set — skipping sha256 verification. ' +
      'Use only for local development builds.\n'
    );
    execFileSync(process.env.DREAMD_BIN, dreamdArgs, { stdio: 'inherit' });
    return;
  }

  const target = getPlatformTarget();
  if (!target) {
    process.stderr.write(
      `No prebuilt binary for ${process.platform}/${process.arch}. Install via:\n` +
      `  cargo install --path crates/dreamd-cli\n` +
      `Then set DREAMD_BIN=/path/to/dreamd and re-run.\n`
    );
    process.exit(1);
  }

  const binaryEntry = MANIFEST.binaries[target];
  if (!binaryEntry) {
    process.stderr.write(
      `No prebuilt binary for ${target}. Install via:\n` +
      `  cargo install --path crates/dreamd-cli\n` +
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

if (require.main === module) {
  main().catch((err) => {
    process.stderr.write(`[dreamd-mcp] Fatal error: ${err.message}\n`);
    process.exit(1);
  });
}

module.exports = { resolveDreamdArgs, topLevelFlag };
