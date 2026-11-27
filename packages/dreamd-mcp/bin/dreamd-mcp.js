#!/usr/bin/env node
'use strict';

// dreamd-mcp — npx download/verify/exec shim for the native `dreamd` binary.
// Flow: resolve platform → cache hit or GitHub release download → sha256 verify
// → run as a child that gets the shim's signals and whose exit status the shim
// keeps (DRM-123). Env overrides: DREAMD_BIN (+ DREAMD_BIN_ALLOW_UNVERIFIED).
// The shim sets DREAMD_LAUNCHER=npx for the binary it runs. Exports below are
// for unit tests only; IDEs spawn this file as the MCP entrypoint.

const https = require('https');
const fs = require('fs');
const path = require('path');
const os = require('os');
const crypto = require('crypto');
const { execFileSync, spawn, spawnSync } = require('child_process');
const zlib = require('zlib');

// Derived from package.json — the single source of truth for the version. This
// drives the GitHub release tag (`v${VERSION}`), the download URL, the per-version
// cache dir, and `--version` output, so all of them track package.json with no
// hand-sync on a release cut. (package.json ships in every installed npm package
// regardless of the `files` allowlist, so this require is always resolvable.)
const VERSION = require('../package.json').version;
const MANIFEST = require('../manifest.json');

// Bound redirect-following to limit redirect-loop DoS during asset download.
const MAX_REDIRECTS = 5;

// Hosts a release download is allowed to redirect to. Captured empirically
// 2026-06-24: github.com 302-redirects release assets to
// release-assets.githubusercontent.com (a githubusercontent.com subdomain,
// Azure-blob-backed). The binary-sha check (verifyBinary) is the real integrity
// backstop; this allowlist is defense-in-depth against redirect-loop DoS, an
// http:// downgrade, and an arbitrary-host fetch if github.com ever returned a
// poisoned Location. If GitHub changes its asset host, update this list.
function isAllowedRedirectHost(hostname) {
  return (
    hostname === 'github.com' ||
    hostname.endsWith('.github.com') ||
    hostname.endsWith('.githubusercontent.com')
  );
}

// Resolve a Location (absolute or relative) against the current URL and validate
// it. Returns the validated absolute URL string, or throws with a clear reason.
function validateRedirect(location, currentUrl) {
  if (!location) throw new Error('redirect response had no Location header');
  let next;
  try {
    next = new URL(location, currentUrl);
  } catch {
    throw new Error(`invalid redirect target: ${location}`);
  }
  if (next.protocol !== 'https:') {
    throw new Error(`refusing non-https redirect to ${next.protocol}//${next.host}`);
  }
  if (!isAllowedRedirectHost(next.hostname)) {
    throw new Error(`refusing redirect to untrusted host: ${next.hostname}`);
  }
  return next.toString();
}

// Native `dreamd` subcommands. Keep in sync with the clap `Command` enum in
// crates/dreamd-cli/src/cli.rs — adding a `dreamd` subcommand without adding it
// here silently routes `npx dreamd-mcp <newcmd>` to `dreamd mcp <newcmd>`.
const DREAMD_SUBCOMMANDS = new Set([
  'init', 'setup', 'watch', 'doctor', 'dream', 'reset', 'service', 'uninstall', 'update',
  'version', 'mcp', 'blame', 'salience-drift', 'memory', 'forget',
  'vectors', 'archive', 'migrate', 'recall', 'score', 'status',
]);

// A recognized first token is forwarded to `dreamd` verbatim; a bare invocation
// (no args -> args[0] === undefined) or an unrecognized token defaults to
// `dreamd mcp` so IDEs spawning `npx dreamd-mcp` get the MCP server over stdio.
function resolveDreamdArgs(args) {
  return DREAMD_SUBCOMMANDS.has(args[0]) ? args : ['mcp', ...args];
}

// The environment the shim runs `dreamd` with. `DREAMD_LAUNCHER=npx` tells
// `dreamd setup` to print `npx -y dreamd-mcp <command>` in its hints, because
// an npx install puts no `dreamd` on PATH (BZR-1469).
function childEnv(env) {
  return { ...env, DREAMD_LAUNCHER: 'npx' };
}

// Leading top-level flags the shim answers itself (no binary download). Only the
// first token counts: `npx dreamd-mcp init --help` still routes to the binary.
// The shim handles these rather than forwarding them because `dreamd mcp <flag>`
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
  DREAMD_BIN=<path>                 dev-only: run a local build (skips sha256 verification)
  DREAMD_BIN_ALLOW_UNVERIFIED=1     required alongside DREAMD_BIN to confirm the bypass
`;

function getPlatformTarget() {
  // Prebuilt targets are linux-x86_64 and darwin; the shim has no prebuilt
  // Windows binary. Cache helpers below still tolerate win32 paths defensively.
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
    function get(u, depth) {
      https.get(u, (res) => {
        if (res.statusCode === 301 || res.statusCode === 302) {
          // Only follow 301/302 (GitHub asset hops); drain body before recurse.
          res.resume();
          if (depth >= MAX_REDIRECTS) {
            return reject(new Error(`too many redirects (>${MAX_REDIRECTS}) fetching release asset`));
          }
          let nextUrl;
          try {
            nextUrl = validateRedirect(res.headers.location, u);
          } catch (err) {
            return reject(err);
          }
          return get(nextUrl, depth + 1);
        }
        if (res.statusCode !== 200) {
          res.resume();
          return reject(new Error(`HTTP ${res.statusCode} fetching ${u}`));
        }
        res.pipe(file);
        file.on('finish', () => file.close(resolve));
        file.on('error', reject);
      }).on('error', reject);
    }
    get(url, 0);
  });
}

function extractTarGz(archivePath, destDir) {
  const result = spawnSync('tar', ['-xzf', archivePath, '-C', destDir], { stdio: 'inherit' });
  if (result.status !== 0) {
    throw new Error(`tar extraction failed with status ${result.status}`);
  }
}

// Reject absolute paths and any `..` traversal segment in a tar entry name.
function isSafeTarEntry(name) {
  if (!name) return false;
  if (name.startsWith('/') || path.isAbsolute(name)) return false;
  return !name.split(/[\\/]/).some((part) => part === '..');
}

// List the archive (argv form, no shell) and reject any unsafe entry name
// before a single byte is unpacked.
function assertSafeArchive(archivePath) {
  const listing = execFileSync('tar', ['-tzf', archivePath], { encoding: 'utf8' });
  for (const raw of listing.split('\n')) {
    const name = raw.trim();
    if (name && !isSafeTarEntry(name)) {
      throw new Error(`unsafe tar entry rejected: ${name}`);
    }
  }
}

async function ensureBinary(target, binaryPath, expectedSha256) {
  const cacheDir = path.dirname(binaryPath);
  fs.mkdirSync(cacheDir, { recursive: true });

  // Isolated, per-invocation extraction dir — nothing escapes into the cache.
  const work = fs.mkdtempSync(path.join(cacheDir, '.extract-'));
  const archiveName = `${target}.tar.gz`;
  const archivePath = path.join(work, archiveName);
  const downloadUrl =
    `https://github.com/botzrDev/dreamd/releases/download/v${VERSION}/${archiveName}`;

  try {
    process.stderr.write(`[dreamd-mcp] Downloading dreamd v${VERSION} for ${target}...\n`);
    await downloadFile(downloadUrl, archivePath);

    assertSafeArchive(archivePath);

    process.stderr.write('[dreamd-mcp] Extracting...\n');
    extractTarGz(archivePath, work);

    const extracted = path.join(work, getBinaryName());
    if (!fs.existsSync(extracted)) {
      throw new Error(`Binary not found at ${extracted} after extraction`);
    }
    // Must be a real file (not a symlink escaping the dir) contained in `work`.
    if (!fs.lstatSync(extracted).isFile()) {
      throw new Error(`extracted ${getBinaryName()} is not a regular file`);
    }
    const realExtracted = fs.realpathSync(extracted);
    const realWork = fs.realpathSync(work);
    if (!realExtracted.startsWith(realWork + path.sep)) {
      throw new Error('extracted binary escaped the extraction dir');
    }

    // Verify before promoting into the cache. Throw (not exit) so `finally`
    // still cleans up; the exec-gate verify at the call site stays for cache hits.
    if (sha256File(extracted) !== expectedSha256) {
      throw new Error(
        `sha256 mismatch for downloaded binary\n  expected: ${expectedSha256}\n  actual:   ${sha256File(extracted)}`,
      );
    }

    fs.chmodSync(extracted, 0o755);
    fs.renameSync(extracted, binaryPath);
  } finally {
    fs.rmSync(work, { recursive: true, force: true });
  }
}

// Which signals the shim passes to the child, and which it only holds so it
// outlives the child (DRM-123). On Windows the console already delivers
// Ctrl-C and Ctrl-Break to the child, and child.kill() is TerminateProcess,
// which would skip dreamd's shutdown.
function signalPlan(platform) {
  return platform === 'win32'
    ? { forward: [], hold: ['SIGINT', 'SIGBREAK'] }
    : { forward: ['SIGINT', 'SIGTERM', 'SIGHUP'], hold: [] };
}

// The status a shim that cannot die of `signal` exits with: the child's own
// code, else 128 + the signal's number, else 1.
function exitCodeFor(code, signal) {
  if (typeof code === 'number') return code;
  const n = signal ? os.constants.signals[signal] : undefined;
  return typeof n === 'number' ? 128 + n : 1;
}

// How often the shim checks whether its parent is gone (DRM-160).
const ORPHAN_POLL_MS = 500;

// Run the native binary and settle when it is gone. Never rejects. The child
// shares the shim's stdio, so the JSON-RPC bytes of `dreamd mcp` pass straight
// through; the shim itself writes nothing to stdout.
function runChild(bin, args, env) {
  return new Promise((resolve) => {
    const plan = signalPlan(process.platform);
    const listeners = [];
    let done = false;
    let child;
    const removeListeners = () => {
      for (const [sig, fn] of listeners) process.removeListener(sig, fn);
      listeners.length = 0;
    };
    let orphanTimer = null;
    const settle = () => {
      if (done) return false;
      done = true;
      removeListeners();
      if (orphanTimer) clearInterval(orphanTimer);
      return true;
    };
    try {
      child = spawn(bin, args, { stdio: 'inherit', env });
    } catch (err) {
      // A synchronous throw (an invalid argument) is a spawn failure too.
      settle();
      process.stderr.write(`[dreamd-mcp] could not start ${bin}: ${err.code}: ${err.message}\n`);
      process.exitCode = 1;
      resolve();
      return;
    }
    for (const sig of plan.forward) {
      const fn = () => {
        try {
          child.kill(sig);
        } catch {
          // The child is already gone; its 'exit' settles.
        }
      };
      process.on(sig, fn);
      listeners.push([sig, fn]);
    }
    for (const sig of plan.hold) {
      const fn = () => {};
      process.on(sig, fn);
      listeners.push([sig, fn]);
    }
    // The orphan rule (DRM-160). Under `npx`, the shim's parent is npm's
    // `sh -c`. Where /bin/sh is dash, a SIGTERM to `npm exec` kills that
    // shell and is never passed on, so the shim and the native `watch` kept
    // running. When the parent the shim started under is gone, the shim is
    // reparented: it then stops the child as if it had got SIGTERM. For
    // `dreamd mcp` this changes nothing, since stdin EOF already ends it.
    if (process.platform !== 'win32') {
      const parent = process.ppid;
      orphanTimer = setInterval(() => {
        if (process.ppid === parent) return;
        clearInterval(orphanTimer);
        try {
          child.kill('SIGTERM');
        } catch {
          // The child is already gone; its 'exit' settles.
        }
      }, ORPHAN_POLL_MS);
      // The child keeps the shim alive; the timer must not.
      orphanTimer.unref();
    }
    child.on('error', (err) => {
      if (!settle()) return;
      process.stderr.write(`[dreamd-mcp] could not start ${bin}: ${err.code}: ${err.message}\n`);
      process.exitCode = 1;
      resolve();
    });
    child.on('exit', (code, signal) => {
      if (!settle()) return;
      if (signal && process.platform !== 'win32') {
        // Die of the same signal, so a shell or supervisor sees what the
        // child gave the shim. The listeners are gone, so the default action
        // runs.
        try {
          process.kill(process.pid, signal);
        } catch {
          // Fall through to the exit status below.
        }
      }
      process.exitCode = exitCodeFor(code, signal);
      resolve();
    });
  });
}

async function main() {
  const args = process.argv.slice(2);

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
    if (process.env.DREAMD_BIN_ALLOW_UNVERIFIED !== '1') {
      process.stderr.write(
        '[dreamd-mcp] DREAMD_BIN is set but DREAMD_BIN_ALLOW_UNVERIFIED=1 is not.\n' +
        '[dreamd-mcp] DREAMD_BIN runs an unverified local binary (no sha256 check) and is dev-only.\n' +
        '[dreamd-mcp] Re-run with DREAMD_BIN_ALLOW_UNVERIFIED=1 to confirm you accept this.\n',
      );
      process.exit(1);
    }
    process.stderr.write(
      '[dreamd-mcp] WARNING: DREAMD_BIN set — skipping sha256 verification. ' +
      'Use only for local development builds.\n',
    );
    await runChild(process.env.DREAMD_BIN, dreamdArgs, childEnv(process.env));
    return;
  }

  const target = getPlatformTarget();
  if (!target) {
    if (process.platform === 'win32') {
      process.stderr.write(
        `This npm package has no Windows binary.\n` +
          `A dreamd.exe built from source, or taken from a GitHub release windows-x86_64.tar.gz when that release has one, runs learn, the dream cycle, and the Tantivy index over loopback. Start dreamd watch before dreamd mcp.\n` +
          `For npx -y dreamd-mcp, use WSL2 or a Linux/macOS host.\n` +
          `Prebuilt shim binaries ship for linux-x64, darwin-x64, and darwin-arm64 only.\n`
      );
    } else {
      process.stderr.write(
        `No prebuilt binary for ${process.platform}/${process.arch}.\n` +
          `Supported platforms: linux-x64, darwin-x64, darwin-arm64.\n` +
          `On a supported OS with a source checkout, you can build with:\n` +
          `  cargo install --path crates/dreamd-cli\n` +
          `Then set DREAMD_BIN=/path/to/dreamd and DREAMD_BIN_ALLOW_UNVERIFIED=1 and re-run.\n`
      );
    }
    process.exit(1);
  }

  const binaryEntry = MANIFEST.binaries[target];
  if (!binaryEntry) {
    process.stderr.write(
      `No prebuilt binary for ${target} in this package's manifest.\n` +
        `Supported platforms: linux-x64, darwin-x64, darwin-arm64.\n` +
        `On a supported OS with a source checkout, you can build with:\n` +
        `  cargo install --path crates/dreamd-cli\n` +
        `Then set DREAMD_BIN=/path/to/dreamd and DREAMD_BIN_ALLOW_UNVERIFIED=1 and re-run.\n`
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

  await runChild(binaryPath, dreamdArgs, childEnv(process.env));
}

if (require.main === module) {
  // No child run throws (runChild never rejects), so a throw here is a
  // download or verify step's, and it is fatal.
  main().catch((err) => {
    process.stderr.write(`[dreamd-mcp] Fatal error: ${err.message}\n`);
    process.exit(1);
  });
}

module.exports = {
  DREAMD_SUBCOMMANDS,
  resolveDreamdArgs,
  childEnv,
  topLevelFlag,
  isAllowedRedirectHost,
  validateRedirect,
  isSafeTarEntry,
  signalPlan,
  exitCodeFor,
  runChild,
};
