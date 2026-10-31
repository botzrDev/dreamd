'use strict';
const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');
const { DREAMD_SUBCOMMANDS, resolveDreamdArgs, topLevelFlag } = require('../bin/dreamd-mcp.js');

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
test('uninstall -> dreamd uninstall (AILAB-226: not routed to mcp)', () => {
  assert.deepStrictEqual(resolveDreamdArgs(['uninstall']), ['uninstall']);
  assert.deepStrictEqual(
    resolveDreamdArgs(['uninstall', '--keep-caches']),
    ['uninstall', '--keep-caches'],
  );
});
test('update -> dreamd update (AILAB-226: not routed to mcp)', () => {
  assert.deepStrictEqual(resolveDreamdArgs(['update']), ['update']);
  assert.deepStrictEqual(
    resolveDreamdArgs(['update', '--dry-run']),
    ['update', '--dry-run'],
  );
});
test('setup -> dreamd setup (AILAB-549: front door, not routed to mcp)', () => {
  assert.deepStrictEqual(resolveDreamdArgs(['setup']), ['setup']);
  assert.deepStrictEqual(
    resolveDreamdArgs(['setup', '--yes', '--harness', 'none']),
    ['setup', '--yes', '--harness', 'none'],
  );
});
test('service -> dreamd service (AILAB-169/AILAB-185: install/start/restart not routed to mcp)', () => {
  assert.deepStrictEqual(resolveDreamdArgs(['service', 'install']), ['service', 'install']);
  assert.deepStrictEqual(
    resolveDreamdArgs(['service', 'install', '--force']),
    ['service', 'install', '--force'],
  );
  assert.deepStrictEqual(resolveDreamdArgs(['service', 'start']), ['service', 'start']);
  assert.deepStrictEqual(resolveDreamdArgs(['service', 'restart']), ['service', 'restart']);
});
// AILAB-202: the nested `service uninstall` rides the `service` token that is
// already in DREAMD_SUBCOMMANDS — no new top-level token (`uninstall` is the
// separate top-level `dreamd uninstall` verb, listed on its own).
test('service uninstall -> dreamd service uninstall (AILAB-202: not routed to mcp)', () => {
  assert.deepStrictEqual(resolveDreamdArgs(['service', 'uninstall']), ['service', 'uninstall']);
  assert.deepStrictEqual(
    resolveDreamdArgs(['service', 'uninstall', '--purge', '--yes']),
    ['service', 'uninstall', '--purge', '--yes'],
  );
});
// AILAB-206: `--bind` is a flag, not a subcommand, so it takes the default
// `mcp` route — no `bind` / `insecure` entry in DREAMD_SUBCOMMANDS. Routing
// only: the prebuilt binary has no `mcp-http` feature and refuses it (exit 2).
test('--bind -> dreamd mcp --bind (AILAB-206: Streamable HTTP opt-in)', () => {
  assert.deepStrictEqual(
    resolveDreamdArgs(['--bind', '127.0.0.1:8080']),
    ['mcp', '--bind', '127.0.0.1:8080'],
  );
});
test('blame -> dreamd blame (BZR-193: not routed to mcp)', () => {
  assert.deepStrictEqual(resolveDreamdArgs(['blame', 'q']), ['blame', 'q']);
});
test('salience-drift -> dreamd salience-drift (BZR-195: not routed to mcp)', () => {
  assert.deepStrictEqual(resolveDreamdArgs(['salience-drift']), ['salience-drift']);
});
test('memory -> dreamd memory (BZR-150: not routed to mcp)', () => {
  assert.deepStrictEqual(resolveDreamdArgs(['memory', 'branches']), ['memory', 'branches']);
});
test('forget -> dreamd forget (BZR-151: not routed to mcp)', () => {
  assert.deepStrictEqual(
    resolveDreamdArgs(['forget', 'evt_01ARZ3NDEKTSV4RRFFQ69G5FAV']),
    ['forget', 'evt_01ARZ3NDEKTSV4RRFFQ69G5FAV'],
  );
});
test('vectors -> dreamd vectors (BZR-181: not routed to mcp)', () => {
  assert.deepStrictEqual(resolveDreamdArgs(['vectors', 'enable']), ['vectors', 'enable']);
});
for (const cmd of ['archive', 'migrate', 'recall', 'score', 'status']) {
  test(`${cmd} -> dreamd ${cmd} (not routed to mcp)`, () => {
    assert.deepStrictEqual(resolveDreamdArgs([cmd, '--help']), [cmd, '--help']);
  });
}
// The routing set must name every variant of the clap `Command` enum, or
// `npx dreamd-mcp <cmd>` silently becomes `dreamd mcp <cmd>`. Skipped when the
// Rust source is absent (a packed tarball ships only bin/).
test('DREAMD_SUBCOMMANDS matches the clap Command enum', (t) => {
  const cli = path.join(__dirname, '..', '..', '..', 'crates', 'dreamd-cli', 'src', 'cli.rs');
  if (!fs.existsSync(cli)) {
    t.skip('crates/dreamd-cli/src/cli.rs not found');
    return;
  }
  const body = fs.readFileSync(cli, 'utf8').match(/^pub enum Command \{([\s\S]*?)^\}/m);
  assert.ok(body, 'pub enum Command not found in cli.rs');
  const variants = [...body[1].matchAll(/^    ([A-Z][A-Za-z]*)\b/gm)].map((m) =>
    m[1].replace(/([a-z])([A-Z])/g, '$1-$2').toLowerCase(),
  );
  assert.ok(variants.length > 0, 'no Command variants parsed');
  assert.deepStrictEqual([...DREAMD_SUBCOMMANDS].sort(), [...new Set(variants)].sort());
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
