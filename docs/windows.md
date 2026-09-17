# Windows support

dreamd's durability guarantees lean on POSIX file semantics. Episodic appends
are `OpenOptions` + `write_all` + `sync_data` on the JSONL, which works
anywhere. Everything that *replaces* a file — the dream cycle's `LESSONS.md`
rewrite, the pruned JSONL, the recurrence sidecar, the Tantivy index manifest —
goes through `io::write_atomic`, which writes a temporary file and atomically
renames it over the target (`rename(2)`), so a crash mid-write can never leave a
half-written file in the store. That is what `io::write_atomic` provides on
Linux and macOS.

**Windows durable writes are not in v0.1.1.** The atomic-write path
returns [`std::io::ErrorKind::Unsupported`] on Windows rather than silently
falling back to a non-atomic write that could corrupt the store on a crash.

Native Windows was out of scope for v0.1. v0.1.1 added compile, `dreamd service
install` (logon task + `auth.json`), and `dreamd watch` over loopback TCP with
a bearer token. `io::write_atomic` is still `ErrorKind::Unsupported`, so the
dream cycle and the Tantivy index do not work there. For a store you can
consolidate and search, use WSL2 or a Linux/macOS host.

The Unix transport stays Unix-only: `~/.agent/dreamd.sock`, the `SO_PEERCRED`
peer-UID check, and the `dreamd dream` UDS proxy are all `#[cfg(unix)]`. Windows
serves the *same* axum router over a different transport rather than borrowing
that one. The Windows CI jobs remain informational (`continue-on-error`) — they
are not a merge gate.

## `dreamd watch` on Windows: loopback TCP + bearer

AILAB-192 gives `dreamd watch` a Windows topology. It:

1. **Requires `%USERPROFILE%\.agent\auth.json`** before it binds anything.
   Missing or malformed is a start failure — exit **1**, pointing you at
   `dreamd service install`. `watch` reads that file and never writes it: the
   token is minted by `dreamd service install` and rotated by
   `dreamd service install --force`, never at watch start.
2. **Binds `127.0.0.1:0`** — loopback, OS-chosen ephemeral port — and refuses
   to serve if the address it gets back is not loopback. `--bind <IP[:port]>`
   picks another address (AILAB-197); a non-loopback one is refused before it
   binds unless `--insecure` is also passed, which logs a warning on every
   start and still requires the bearer token below.
3. **Publishes the bound address** to `%USERPROFILE%\.agent\server.json` as
   `{"host":"127.0.0.1","port":<port>}` (a loopback host even for an
   `--insecure` wildcard bind), using a plain `std::fs::write` rather
   than the atomic-replace path — the file is a discardable address hint, not
   memory state — and unlinks it again on every shutdown path, the way the Unix
   bind unlinks its socket.
4. **Requires `Authorization: Bearer <64 lowercase hex>`** on every request,
   matching `auth.json`. The file is re-read per request and compared in
   constant time, so a `service install --force` rotation `401`s the next
   request without a restart. A missing or wrong token is **401**; `403` is the
   Unix peer-UID verdict and that layer is not mounted here.
5. **Shuts down on Ctrl-C.** There is no `SIGTERM` on Windows, so Ctrl-C is the
   whole signal story; the drain then runs exactly as it does on Unix.
6. **Never opens the Tantivy index.** `TantivyIndexHandle::open` writes its
   first `index_manifest.json` through `io::write_atomic`, so a Windows watch
   that opened the index could not boot at all.

That last point is what you feel in practice. `POST /api/v1/learn` works — the
JSONL append never touches `write_atomic` — so learnings do land durably in
`.agent/episodic/AGENT_LEARNINGS.jsonl`. `POST /api/v1/dream` stays mounted and
returns **500** naming the reason (`dream cycle is unavailable on this platform:
atomic file replacement is unsupported`), because consolidation, decay, the
`LESSONS.md` rewrite and the recurrence sidecar all replace files atomically.
`GET /api/v1/recall` and `GET /api/v1/health` are index-backed and can fail for
the same reason. A valid token gets you past the auth layer; it is not a promise
of a `200`.

See [http-api.md](./http-api.md) for the transport tables, the exact middleware
verdicts, and a PowerShell smoke test that reads the port and the token back out
of the two files.

## `dreamd mcp` on Windows

`dreamd mcp` serves the **Remote** backend over that same loopback TCP plus
bearer when `server.json` is live — it parses, and `127.0.0.1:<port>` accepts a
connection — and exits **2** otherwise. There is deliberately no in-process
fallback: a coordinator booted inside `mcp` would accept `append_node` writes it
cannot durably persist, which is why AILAB-174 deleted that branch. Start
`dreamd watch` first (or let the logon task start it), then point your harness
at `npx -y dreamd-mcp`.

## Which commands see the daemon

`dreamd status` and `dreamd archive` probe the live daemon the Windows way: read
`server.json`, then try a TCP connect to `127.0.0.1:<port>`. So `status` reports
a running Windows daemon, and `archive` refuses to rewrite a store underneath
one — which is the point of that guard.

`dreamd uninstall` and `dreamd update` do **not**. Their liveness guard still
answers `false` on Windows, so both behave as "no daemon running" even while a
`watch` is serving, and neither stops it — `stop_local_servers` is a no-op there
and says so on stderr. Stop the daemon yourself (Ctrl-C in its terminal, or
`schtasks /End /TN dev.dreamd.dreamd`) before running either.

## `dreamd service install` (Task Scheduler)

AILAB-203 made `dreamd service install` work there. On Windows it writes a Task
Scheduler 1.2 XML to `%USERPROFILE%\AppData\Roaming\dreamd\dev.dreamd.dreamd.xml`
and registers it with `schtasks /Create /TN dev.dreamd.dreamd /XML <path>`: a
**logon-triggered** task whose action is foreground `dreamd watch`, with
`WorkingDirectory` set to the project root discovered from your current
directory at install time. It is a scheduled task, not a Windows Service —
no `sc.exe`, no SCM, no detached process, no PID file — and it supervises the
foreground daemon exactly as the systemd `--user` unit and the macOS
LaunchAgent do. `install` also mints the daemon-home token
`%USERPROFILE%\.agent\auth.json` — 32 cryptographically random bytes rendered
as 64 lowercase hex characters in `{"token":"…"}` — and locks it down with
`icacls <path> /inheritance:r /grant:r <your SID>:F`, so inheritance is
stripped and only your SID keeps full control. The token is never printed and
never logged; only the paths are. `--force` overwrites an existing task XML
*and* rotates the token; without it an existing XML is left untouched and
`install` exits 2. `dreamd service uninstall` deletes the task and the XML but
leaves `auth.json` alone — only `--purge`, which deletes the daemon home
`%USERPROFILE%\.agent\`, removes it.

Registering the task is still not running the daemon: `install` deliberately
does not `schtasks /Run`, so nothing starts until the next logon or an explicit
`dreamd service start`. What AILAB-192 changed is what happens when the trigger
does fire — the task's `dreamd watch` now serves, provided `auth.json` is there
for it to read. `dreamd service start` / `restart` / `status` still report what
Task Scheduler thinks, not what `watch` did: a `/Run` whose `watch` exited 1 on
a missing token can still read as `stopped`, and a task Task Scheduler calls
`Running` is not proof the API is answering. `dreamd status` is the
daemon-liveness question; `dreamd service status` is the supervisor one.

## Still open

`io::write_atomic` is still `ErrorKind::Unsupported` on Windows, and that is the
one blocker left for parity: it is why the dream cycle, `LESSONS.md`, the
recurrence sidecar and the Tantivy index are unavailable there. It remains open
v0.1.1 work.
