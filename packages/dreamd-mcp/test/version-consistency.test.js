'use strict';
const test = require('node:test');
const assert = require('node:assert');
const { execFileSync } = require('node:child_process');
const path = require('node:path');

const pkg = require('../package.json');
const shim = path.join(__dirname, '..', 'bin', 'dreamd-mcp.js');

// The version the shim reports at runtime must equal package.json. `-V` / `--version`
// are answered by the shim itself (topLevelFlag) BEFORE any binary download, so this
// is hermetic — no network, no native binary. This guards against re-introducing a
// hardcoded VERSION constant that silently drifts from package.json — the exact
// rc.3/rc.4 drift that shipped a package which downloaded and ran the wrong binary.
test('shim -V prints package.json version', () => {
  const out = execFileSync('node', [shim, '-V'], { encoding: 'utf8' });
  assert.strictEqual(out, `dreamd-mcp ${pkg.version}\n`);
});

test('shim --version prints package.json version', () => {
  const out = execFileSync('node', [shim, '--version'], { encoding: 'utf8' });
  assert.strictEqual(out, `dreamd-mcp ${pkg.version}\n`);
});
