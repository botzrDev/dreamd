'use strict';
const test = require('node:test');
const assert = require('node:assert');
const manifest = require('../manifest.json');

const PLATFORMS = ['linux-x86_64', 'darwin-x86_64', 'darwin-aarch64'];
const SHA256_RE = /^[a-f0-9]{64}$/;

test('manifest version matches package.json', () => {
  const { version } = require('../package.json');
  assert.strictEqual(manifest.version, version);
});

test('every shim platform has a real sha256 (no placeholders)', () => {
  for (const platform of PLATFORMS) {
    const entry = manifest.binaries[platform];
    assert.ok(entry, `missing binaries.${platform}`);
    assert.match(
      entry.sha256,
      SHA256_RE,
      `binaries.${platform}.sha256 must be a 64-char hex digest, got ${JSON.stringify(entry.sha256)}`,
    );
    assert.ok(
      !entry.sha256.startsWith('PENDING'),
      `binaries.${platform}.sha256 is still a placeholder`,
    );
  }
});
