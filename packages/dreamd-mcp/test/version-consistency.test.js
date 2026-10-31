'use strict';
const test = require('node:test');
const assert = require('node:assert');
const { execFileSync } = require('node:child_process');
const path = require('node:path');

const pkg = require('../package.json');
const shim = path.join(__dirname, '..', 'bin', 'dreamd-mcp.js');

// The version the shim reports at runtime must equal package.json. `-V` / `--version`
// are answered by the shim itself (topLevelFlag) before any binary download, so this
// is hermetic — no network, no native binary. This guards against a hardcoded
// VERSION constant that drifts from package.json, which would make the shim
// download and run the wrong binary.
test('shim -V prints package.json version', () => {
  const out = execFileSync('node', [shim, '-V'], { encoding: 'utf8' });
  assert.strictEqual(out, `dreamd-mcp ${pkg.version}\n`);
});

test('shim --version prints package.json version', () => {
  const out = execFileSync('node', [shim, '--version'], { encoding: 'utf8' });
  assert.strictEqual(out, `dreamd-mcp ${pkg.version}\n`);
});
