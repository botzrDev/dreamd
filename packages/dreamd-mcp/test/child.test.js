'use strict';
// DRM-123: the shim runs `dreamd` as an async child, passes SIGINT, SIGTERM
// and SIGHUP to it, waits for it, and keeps its exit status.
//
// Every test runs the shim as `node bin/dreamd-mcp.js <args>` with
// DREAMD_BIN aimed at a fake `#!/bin/sh` binary in a fresh temp dir,
// DREAMD_BIN_ALLOW_UNVERIFIED=1 and its own HOME. A fake that waits runs
// `sleep 30 & wait $!`, so its trap runs at once, and the trap kills the
// sleep. Signals go to the shim's pid alone, never to the group.
const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn, spawnSync, execFileSync } = require('node:child_process');

const { signalPlan, exitCodeFor } = require('../bin/dreamd-mcp.js');

const SHIM = path.join(__dirname, '..', 'bin', 'dreamd-mcp.js');
const POSIX_ONLY = process.platform === 'win32' ? 'POSIX signals and #!/bin/sh fakes' : false;

function tempDir(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

// A fake `dreamd` at <dir>/dreamd with `body` after the shebang.
function fake(dir, body) {
  const bin = path.join(dir, 'dreamd');
  fs.writeFileSync(bin, `#!/bin/sh\n${body}\n`, { mode: 0o755 });
  return bin;
}

function shimEnv(dir, bin, extra = {}) {
  const home = path.join(dir, 'home');
  fs.mkdirSync(home, { recursive: true });
  return {
    ...process.env,
    HOME: home,
    DREAMD_BIN: bin,
    DREAMD_BIN_ALLOW_UNVERIFIED: '1',
    ...extra,
  };
}

function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    return err.code === 'EPERM';
  }
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitFor(what, check, ms) {
  const deadline = Date.now() + ms;
  for (;;) {
    const value = check();
    if (value) return value;
    if (Date.now() > deadline) throw new Error(`timed out after ${ms} ms waiting for ${what}`);
    await sleep(20);
  }
}

// Resolve with { status, signal } when `child` exits, or reject after `ms`.
function exited(child, ms) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`shim did not exit within ${ms} ms`)), ms);
    child.on('exit', (status, signal) => {
      clearTimeout(timer);
      resolve({ status, signal });
    });
  });
}

function readPid(file) {
  try {
    const text = fs.readFileSync(file, 'utf8').trim();
    return text ? Number(text) : 0;
  } catch {
    return 0;
  }
}

function killIfAlive(pid) {
  if (pid && alive(pid)) {
    try {
      process.kill(pid, 'SIGKILL');
    } catch {
      // already gone
    }
  }
}

test('the child\'s exit code passes through', () => {
  const dir = tempDir('dreamd-shim-');
  try {
    const bin = process.platform === 'win32' ? null : fake(dir, 'exit 4');
    if (!bin) return;
    const r = spawnSync(process.execPath, [SHIM, 'status'], { env: shimEnv(dir, bin), stdio: 'ignore' });
    assert.strictEqual(r.status, 4);
    assert.strictEqual(r.signal, null);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

for (const sig of ['SIGTERM', 'SIGINT', 'SIGHUP']) {
  test(`a signal to the shim reaches the child, and the shim waits (${sig})`, { skip: POSIX_ONLY }, async () => {
    const dir = tempDir('dreamd-shim-');
    const ready = path.join(dir, 'ready');
    const out = path.join(dir, 'out');
    const name = sig.slice(3);
    const bin = fake(
      dir,
      `trap 'echo ${sig} > "${out}"; kill $SPID 2>/dev/null; exit 0' ${name}\n` +
        'sleep 30 &\nSPID=$!\n' +
        `echo $$ > "${ready}"\n` +
        'wait $SPID',
    );
    let fakePid = 0;
    try {
      const shim = spawn(process.execPath, [SHIM, 'watch'], { env: shimEnv(dir, bin), stdio: 'ignore' });
      const done = exited(shim, 10000);
      fakePid = await waitFor('the fake\'s ready file', () => readPid(ready), 5000);

      process.kill(shim.pid, sig);
      const r = await Promise.race([
        done,
        sleep(5000).then(() => ({ timeout: true })),
      ]);

      assert.deepStrictEqual(r, { status: 0, signal: null }, `the shim must exit 0 within 5 s after ${sig}`);
      assert.strictEqual(fs.readFileSync(out, 'utf8').trim(), sig);
      await waitFor('the fake to be gone', () => !alive(fakePid), 2000);
    } finally {
      killIfAlive(fakePid);
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });
}

// DRM-160: npm runs the shim under `sh -c`. Where that shell dies of a
// SIGTERM it does not pass on (dash), the shim is orphaned and must stop the
// child itself. The trailing `; :` keeps sh from exec'ing the shim, so sh
// stays its parent, as under npm.
test('an orphaned shim stops its child with SIGTERM', { skip: POSIX_ONLY }, async () => {
  const dir = tempDir('dreamd-shim-');
  const ready = path.join(dir, 'ready');
  const out = path.join(dir, 'out');
  const bin = fake(
    dir,
    `trap 'echo SIGTERM > "${out}"; kill $SPID 2>/dev/null; exit 0' TERM\n` +
      'sleep 30 &\nSPID=$!\n' +
      `echo $$ > "${ready}"\n` +
      'wait $SPID',
  );
  let fakePid = 0;
  try {
    const sh = spawn('/bin/sh', ['-c', `"${process.execPath}" "${SHIM}" watch; :`], {
      env: shimEnv(dir, bin),
      stdio: 'ignore',
    });
    fakePid = await waitFor('the fake\'s ready file', () => readPid(ready), 5000);

    sh.kill('SIGKILL');
    await waitFor('the fake to get SIGTERM', () => fs.existsSync(out), 5000);
    assert.strictEqual(fs.readFileSync(out, 'utf8').trim(), 'SIGTERM');
    await waitFor('the fake to be gone', () => !alive(fakePid), 2000);
  } finally {
    killIfAlive(fakePid);
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('a child that dies of a signal takes the shim with it', { skip: POSIX_ONLY }, () => {
  for (const [body, want] of [
    ['kill -KILL $$', 'SIGKILL'],
    ['kill -TERM $$', 'SIGTERM'],
  ]) {
    const dir = tempDir('dreamd-shim-');
    try {
      const bin = fake(dir, body);
      const r = spawnSync(process.execPath, [SHIM, 'status'], { env: shimEnv(dir, bin), stdio: 'ignore' });
      assert.strictEqual(r.signal, want, `${body}: status ${r.status}`);
      assert.strictEqual(r.status, null);
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  }
});

test('a binary that cannot start exits 1 with its path', { skip: POSIX_ONLY }, () => {
  const dir = tempDir('dreamd-shim-');
  try {
    const missing = path.join(dir, 'no-such-dreamd');
    const plain = path.join(dir, 'not-executable');
    fs.writeFileSync(plain, '#!/bin/sh\nexit 0\n', { mode: 0o644 });
    for (const [bin, code] of [
      [missing, 'ENOENT'],
      [plain, 'EACCES'],
    ]) {
      const r = spawnSync(process.execPath, [SHIM, 'status'], { env: shimEnv(dir, bin), encoding: 'utf8' });
      assert.strictEqual(r.status, 1, `${code}: stderr ${r.stderr}`);
      assert.strictEqual(r.signal, null);
      assert.ok(r.stderr.includes(bin), `stderr must name ${bin}: ${r.stderr}`);
      assert.ok(r.stderr.includes(code), `stderr must name ${code}: ${r.stderr}`);
      assert.strictEqual(r.stdout, '', 'nothing on stdout');
    }
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('stdio passes through untouched', { skip: POSIX_ONLY }, () => {
  const dir = tempDir('dreamd-shim-');
  try {
    const bin = fake(dir, 'exec cat');
    const line = Buffer.from('{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}\n');
    const rest = Buffer.alloc(64 * 1024 - line.length);
    for (let i = 0; i < rest.length; i++) rest[i] = (i * 31 + 7) & 0xff;
    const input = Buffer.concat([line, rest]);
    // The bare invocation, so the shim runs `dreamd mcp`.
    const r = spawnSync(process.execPath, [SHIM], {
      env: shimEnv(dir, bin),
      input,
      stdio: ['pipe', 'pipe', 'pipe'],
      maxBuffer: 1024 * 1024,
    });
    assert.strictEqual(r.status, 0, r.stderr.toString());
    assert.strictEqual(r.stdout.length, input.length);
    assert.ok(r.stdout.equals(input), 'stdout must be the input bytes exactly');
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('exitCodeFor and signalPlan', () => {
  assert.strictEqual(exitCodeFor(4, null), 4);
  assert.strictEqual(exitCodeFor(null, 'SIGKILL'), 137);
  assert.strictEqual(exitCodeFor(null, 'SIGTERM'), 143);
  assert.strictEqual(exitCodeFor(null, 'SIGNOPE'), 1);
  assert.strictEqual(exitCodeFor(null, null), 1);
  assert.strictEqual(exitCodeFor(0, null), 0);
  for (const platform of ['linux', 'darwin']) {
    assert.deepStrictEqual(signalPlan(platform), { forward: ['SIGINT', 'SIGTERM', 'SIGHUP'], hold: [] });
  }
  assert.deepStrictEqual(signalPlan('win32'), { forward: [], hold: ['SIGINT', 'SIGBREAK'] });
});

// The real `dreamd`: DREAMD_TEST_BIN, else the workspace's debug build.
function realDreamd() {
  const candidates = [process.env.DREAMD_TEST_BIN, path.join(__dirname, '..', '..', '..', 'target', 'debug', 'dreamd')];
  return candidates.find((p) => p && fs.existsSync(p)) || null;
}

// The shim's only child, by `pgrep -P` (Linux and macOS).
function onlyChild(pid) {
  let text = '';
  try {
    text = execFileSync('pgrep', ['-P', String(pid)], { encoding: 'utf8' });
  } catch {
    return 0;
  }
  const pids = text.split('\n').map((s) => s.trim()).filter(Boolean).map(Number);
  return pids.length === 1 ? pids[0] : 0;
}

for (const sig of ['SIGTERM', 'SIGINT']) {
  const dreamd = realDreamd();
  const skip = POSIX_ONLY || (dreamd ? false : 'no dreamd: set DREAMD_TEST_BIN or build target/debug/dreamd');
  test(`a real watch stops through the shim (${sig})`, { skip }, async () => {
    // A short root, so the socket path stays under the 104-byte sun_path limit.
    const dir = fs.mkdtempSync(path.join(fs.existsSync('/tmp') ? '/tmp' : os.tmpdir(), 'dmw-'));
    const home = path.join(dir, 'h');
    const xdg = path.join(dir, 'x');
    const project = path.join(dir, 'p');
    const sock = path.join(home, 'd.sock');
    for (const d of [home, xdg, project]) fs.mkdirSync(d, { recursive: true });
    fs.writeFileSync(path.join(project, 'Cargo.toml'), '[package]\n');
    const env = {
      ...process.env,
      HOME: home,
      XDG_CONFIG_HOME: xdg,
      DREAMD_SOCK: sock,
      DREAMD_LOG: 'off',
      DREAMD_BIN: dreamd,
      DREAMD_BIN_ALLOW_UNVERIFIED: '1',
    };
    let native = 0;
    try {
      const init = spawnSync(dreamd, ['init', '--quiet'], { cwd: project, env, encoding: 'utf8' });
      assert.strictEqual(init.status, 0, `init: ${init.stderr}`);

      const shim = spawn(process.execPath, [SHIM, 'watch'], { cwd: project, env, stdio: 'ignore' });
      const done = exited(shim, 20000);
      await waitFor('the daemon socket', () => fs.existsSync(sock), 15000);
      native = await waitFor('the native dreamd pid', () => onlyChild(shim.pid), 5000);

      process.kill(shim.pid, sig);
      const r = await Promise.race([done, sleep(10000).then(() => ({ timeout: true }))]);

      assert.deepStrictEqual(r, { status: 0, signal: null }, `the shim must exit 0 within 10 s after ${sig}`);
      assert.ok(!fs.existsSync(sock), 'watch must unlink its socket');
      await waitFor('the native dreamd to be gone', () => !alive(native), 2000);
    } finally {
      killIfAlive(native);
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });
}
