# Windows support

Learn, the dream cycle, recall, and `dreamd watch` run on native Windows.
The daemon is loopback TCP with a bearer token, not a Unix socket. There is
no prebuilt `dreamd-mcp` binary, so you build `dreamd` yourself or take a
release asset. The directory flush is the Unix call order and is not a
power-loss proof.

Episodic appends are `OpenOptions` + `write_all` + `sync_data` on the JSONL.
A replace writes the new bytes in full, then renames them over the target, so
a crash mid-write does not leave a half-written file in the store. The dream
cycle stages `LESSONS.md`, the pruned JSONL, the recurrence sidecar, and the
decay archive under `.agent/.dreamd/stage/` and renames them in once its redo
WAL commits. The index manifest and the other sidecars go through
`io::write_atomic`.

**The replace sequence is the Unix one.** [`io::write_atomic`] writes a temp
file, flushes those bytes, renames, then flushes the parent directory. On
Windows that parent is opened for write with `FILE_FLAG_BACKUP_SEMANTICS`,
then `FlushFileBuffers` (`File::sync_all`). A failure before the rename leaves
the destination untouched. A parent-flush error after the rename is still
returned, so that replace fails. Tantivy documents a directory-handle flush
as a no-op on local disks and an error on virtual drives. Learn does not use
this path, so it still runs when the flush fails, including on a ReFS Dev
Drive. An SMB share is judged by the flush result.

The log's identity on Windows is a volume serial and a file index (`file-id`;
`GetFileInformationByHandle` stays in that crate). Unix uses device and inode.
A dream cycle compares that identity before and after the model call. When it
matches, the model lesson is kept. When the log was replaced during the call,
the cycle re-reads it and writes the deterministic lesson.

`registry.toml` is the exception. `dreamd init` registers a project on Windows
by locking `registry.toml.lock` with `std::fs::File::lock` and replacing
`registry.toml` through a temporary file and `rename`. That writer is not
`io::write_atomic`, the file is not chmod `0600`, and a successful registration
does not flush the parent directory. A second `dreamd init`, when `.agent/` already exists, leaves the store as-is.
If the root is missing from the registry, that init registers it. Learn answers
`404` (`agent root not registered`) until the root is in the registry.

Native Windows was out of scope for v0.1. v0.1.1 added compile, `dreamd service
install` (logon task + `auth.json`), and `dreamd watch` over loopback TCP with
a bearer token. `io::write_atomic` runs there. The dream cycle runs there: the
log's identity is a volume serial and a file index, so a model lesson is kept
when the log did not change during the call. `dreamd watch` opens the Tantivy
index and live appends are indexed. Tantivy does not delete a segment file
that is still mapped; the file stays in the index directory until that mapping
is gone. `dreamd doctor --repair` removes the directory and rebuilds it from
the log, and it refuses while `dreamd watch` is live.

**There is no prebuilt Windows binary on the npm path.** The `dreamd-mcp` shim
ships Linux x86_64 and macOS binaries only; on native Windows
`npx -y dreamd-mcp …` exits 1 and points you at WSL2 before it runs anything.
Everything below assumes a `dreamd.exe` you obtained yourself: build it from
source (`cargo install --path crates/dreamd-cli`), or take the
`windows-x86_64.tar.gz` asset from a
[GitHub release](https://github.com/botzrDev/dreamd/releases). The release job
that builds that asset is allowed to fail, so a given release may not carry it;
v1.0.0 does.

The Unix transport stays Unix-only: `~/.agent/dreamd.sock`, the `SO_PEERCRED`
peer-UID check, and the `dreamd dream` UDS proxy. Windows serves the same
axum router over loopback TCP. `dreamd dream` on Windows runs in-process; it
does not proxy over a socket. The Windows test matrix stays
`continue-on-error`. A separate job, `windows-replace-probe`, is a merge gate
and runs only the production parent-open test.

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
6. **Opens the Tantivy index** and wires it to the coordinator, the same
   hand-off as the Unix daemon. A segment file that is still mapped is left
   in the index directory until the mapping goes. `dreamd doctor --repair`
   deletes that directory after the daemon has stopped and rebuilds the index
   from the log.

`POST /api/v1/learn` appends to `.agent/episodic/AGENT_LEARNINGS.jsonl`.
`POST /api/v1/dream` and `dreamd dream` run the cycle. A model lesson is kept
when the log's file identity did not change during the model call. `dreamd
archive`, `dreamd forget`, and `dreamd reset` replace files through
`write_atomic`. `registry.toml` stays on the temp-file rename with no parent
flush. `GET /api/v1/recall` reads the index `watch` opened. `GET /api/v1/health` is
the on-disk watermark comparison described under
[Index files that stay mapped](#index-files-that-stay-mapped). A valid token
gets you past the auth layer. A `200` from health is that comparison.

See [http-api.md](./http-api.md) for the transport tables, the exact middleware
verdicts, and a PowerShell smoke test that reads the port and the token back out
of the two files.

## `dreamd mcp` on Windows

`dreamd mcp` serves the **Remote** backend over that same loopback TCP plus
bearer when `server.json` is live — it parses, and `127.0.0.1:<port>` accepts a
connection — and exits **2** otherwise. There is no in-process server.
`dreamd watch` holds the project writer lock and the index; a second writer
inside `mcp` is not started. When there is no daemon, the error says to start
`dreamd watch` before `dreamd mcp`.
Start `dreamd watch` first (or let the logon task start it), then point your harness
at that binary — `"command": "dreamd", "args": ["mcp"]` — rather than
at `npx -y dreamd-mcp`, which has no Windows binary to run. (The shim's
dev override, `DREAMD_BIN` plus `DREAMD_BIN_ALLOW_UNVERIFIED=1`, is checked
before the platform refusal, so it also works if you would rather keep the npx
form.)

## Which commands see the daemon

`dreamd status`, `dreamd archive`, `dreamd forget`, and `dreamd memory checkout`
/ `bisect` probe the live daemon the Windows way: read `server.json`, then try
a TCP connect to `127.0.0.1:<port>`. So `status` reports a running Windows
daemon, and `archive`, `forget`, `checkout`, and `bisect` refuse to rewrite a
store underneath one. A `server.json` left behind after the process is gone
does not count as live. On Linux and macOS, `checkout` and `bisect` still
refuse when the socket file exists, including a stale socket.

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

## Index files that stay mapped

Tantivy deletes a segment file after it is no longer part of a commit. On
Windows that delete fails while the file is still mapped. The failure is
expected: Tantivy retries later, and the index directory holds the file until
the mapping is gone. The directory can grow across commits for that reason.

`dreamd doctor --repair` removes `.agent/.dreamd/index` and rebuilds it from
the episodic log. It refuses while `dreamd watch` is live (a loopback connect
to `server.json`), because the daemon is the process holding those mappings.
Stop the daemon, then run the repair. The log is not a repair target.

`GET /api/v1/health` on Windows does not read the in-memory marks the Unix
route uses. It compares the JSONL tail with `index_progress.json`. `stale` is
true when the log has events past that watermark. `dreamd doctor`'s
`index_freshness` line is the set difference between the log and the committed
index.

## Still open

There is no prebuilt `dreamd-mcp` Windows binary. The directory flush is not
a power-loss proof. `/health` stays on the on-disk watermark. `registry.toml`
stays on the temp-file rename with no parent flush.
