'use strict';
const test = require('node:test');
const assert = require('node:assert');
const {
  isAllowedRedirectHost,
  validateRedirect,
  isSafeTarEntry,
} = require('../bin/dreamd-mcp.js');

// --- HIGH-a: redirect host allowlist ---
test('accepts github.com and its real release-asset host', () => {
  assert.ok(isAllowedRedirectHost('github.com'));
  assert.ok(isAllowedRedirectHost('release-assets.githubusercontent.com')); // observed 2026-06-24
  assert.ok(isAllowedRedirectHost('objects.githubusercontent.com'));
});
test('rejects look-alike and foreign hosts (suffix match, not substring)', () => {
  assert.ok(!isAllowedRedirectHost('github.com.attacker.com'));
  assert.ok(!isAllowedRedirectHost('notgithub.com'));
  assert.ok(!isAllowedRedirectHost('evil.com'));
  assert.ok(!isAllowedRedirectHost('githubusercontent.com.evil.io'));
});
test('validateRedirect accepts a normal github -> githubusercontent hop', () => {
  const ok = validateRedirect(
    'https://release-assets.githubusercontent.com/x/y',
    'https://github.com/botzrDev/dreamd/releases/download/v0.1.0-rc.2/linux-x86_64.tar.gz',
  );
  assert.match(ok, /^https:\/\/release-assets\.githubusercontent\.com\//);
});
test('validateRedirect rejects http downgrade and untrusted host', () => {
  assert.throws(() => validateRedirect('http://github.com/x', 'https://github.com/a'), /non-https/);
  assert.throws(() => validateRedirect('https://evil.com/x', 'https://github.com/a'), /untrusted host/);
});
test('validateRedirect rejects a missing Location', () => {
  assert.throws(() => validateRedirect(undefined, 'https://github.com/a'), /no Location/);
});

// --- MED-1: tar entry safety ---
test('isSafeTarEntry accepts the expected binary name', () => {
  assert.ok(isSafeTarEntry('dreamd'));
  assert.ok(isSafeTarEntry('dreamd.exe'));
});
test('isSafeTarEntry rejects traversal and absolute paths', () => {
  assert.ok(!isSafeTarEntry('../dreamd'));
  assert.ok(!isSafeTarEntry('a/../../etc/passwd'));
  assert.ok(!isSafeTarEntry('/etc/passwd'));
  assert.ok(!isSafeTarEntry('foo/../../bar'));
});

// --- MED-1: cache-hit verify-before-exec invariant (regression) ---
const { spawnSync } = require('node:child_process');
const os = require('node:os');
const fs = require('node:fs');
const path = require('node:path');

test('cache-hit path verifies sha before exec (poisoned cache exits non-zero)', (t) => {
  // Resolve the platform target the shim would use; skip on unsupported arch.
  const platform = process.platform;
  const arch = process.arch;
  const target =
    platform === 'linux' && arch === 'x64' ? 'linux-x86_64'
    : platform === 'darwin' && arch === 'x64' ? 'darwin-x86_64'
    : platform === 'darwin' && arch === 'arm64' ? 'darwin-aarch64'
    : null;
  if (!target) { t.skip('no prebuilt target for this platform'); return; }

  const fakeHome = fs.mkdtempSync(path.join(os.tmpdir(), 'dreamd-shim-test-'));
  try {
    // Mirror getCacheDir(): ~/.cache/dreamd-mcp/<VERSION>/<binaryName>
    const { version } = require('../package.json');
    const binName = platform === 'win32' ? 'dreamd.exe' : 'dreamd';
    const cacheDir = path.join(fakeHome, '.cache', 'dreamd-mcp', version);
    fs.mkdirSync(cacheDir, { recursive: true });
    fs.writeFileSync(path.join(cacheDir, binName), 'not the real binary'); // wrong sha

    const res = spawnSync(process.execPath, [path.join(__dirname, '..', 'bin', 'dreamd-mcp.js'), 'version'], {
      env: { ...process.env, HOME: fakeHome, DREAMD_BIN: '', DREAMD_BIN_ALLOW_UNVERIFIED: '' },
      encoding: 'utf8',
    });
    assert.notStrictEqual(res.status, 0);
    assert.match(res.stderr, /sha256 mismatch/i);
  } finally {
    fs.rmSync(fakeHome, { recursive: true, force: true });
  }
});
