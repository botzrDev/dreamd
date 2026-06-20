'use strict';
const test = require('node:test');
const assert = require('node:assert');
const { resolveDreamdArgs, topLevelFlag } = require('../bin/dreamd-mcp.js');

test('bare invocation -> dreamd mcp (IDE MCP server)', () => {
  assert.deepStrictEqual(resolveDreamdArgs([]), ['mcp']);
});
test('watch -> dreamd watch (the WEG-280 fix: shared daemon reachable)', () => {
  assert.deepStrictEqual(resolveDreamdArgs(['watch']), ['watch']);
});
test('init passes through verbatim with flags', () => {
  assert.deepStrictEqual(
    resolveDreamdArgs(['init', '--uninstall-project']),
    ['init', '--uninstall-project'],
  );
});
test('version -> dreamd version (was dreamd mcp version, invalid)', () => {
  assert.deepStrictEqual(resolveDreamdArgs(['version']), ['version']);
});
test('explicit mcp with flags -> single mcp prefix (no double mcp)', () => {
  assert.deepStrictEqual(
    resolveDreamdArgs(['mcp', '--project-root', '/x']),
    ['mcp', '--project-root', '/x'],
  );
});
test('doctor/dream/reset pass through verbatim', () => {
  assert.deepStrictEqual(resolveDreamdArgs(['doctor']), ['doctor']);
  assert.deepStrictEqual(resolveDreamdArgs(['dream', '--no-commit']), ['dream', '--no-commit']);
  assert.deepStrictEqual(resolveDreamdArgs(['reset', 'workspace', '--yes']), ['reset', 'workspace', '--yes']);
});
test('unknown first token defaults to mcp (unchanged behavior)', () => {
  assert.deepStrictEqual(resolveDreamdArgs(['bogus']), ['mcp', 'bogus']);
});

// Leading top-level flags handled by the shim itself (no binary download).
test('--version / -V are shim-handled top-level flags', () => {
  assert.strictEqual(topLevelFlag(['--version']), 'version');
  assert.strictEqual(topLevelFlag(['-V']), 'version');
});
test('--help / -h are shim-handled top-level flags', () => {
  assert.strictEqual(topLevelFlag(['--help']), 'help');
  assert.strictEqual(topLevelFlag(['-h']), 'help');
});
test('no top-level flag for bare invocation or a subcommand', () => {
  assert.strictEqual(topLevelFlag([]), null);
  assert.strictEqual(topLevelFlag(['watch']), null);
  assert.strictEqual(topLevelFlag(['init']), null);
});
test('a flag AFTER a subcommand is not a top-level flag (routes to the binary)', () => {
  assert.strictEqual(topLevelFlag(['init', '--help']), null);
  assert.strictEqual(topLevelFlag(['version', '--help']), null);
});
