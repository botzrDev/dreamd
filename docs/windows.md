# Windows support

dreamd's durability guarantee relies on POSIX atomic-rename semantics
(`rename(2)`): each learning is written to a temporary file and atomically
renamed over the target, so a crash mid-write can never leave a partially
written JSONL line in the store. This is what `io::write_atomic` provides on
Linux and macOS.

**Windows durable writes land in v0.1.1.** Until then, the atomic-write path
returns [`std::io::ErrorKind::Unsupported`] on Windows rather than silently
falling back to a non-atomic write that could corrupt the store on a crash.

Native Windows is out of scope for v0.1: `dreamd watch` and the UDS MCP Remote
path are Unix-only (`#![cfg(unix)]`). Use WSL2 or a Linux/macOS host. Windows
lifecycle support (service install, atomic writes, TCP + `auth.json`) is tracked
in the v0.1.1 milestone — `service install` and the `auth.json` token have
landed; atomic writes and TCP have not.

AILAB-174 made the crate *compile* for a Windows target — the Unix-only modules
are `#[cfg(unix)]` rather than unconditionally imported. Compiling is not
running: `dreamd watch` and `dreamd mcp` refuse rather than serve a store they
cannot durably write, exiting **2** with
`Windows is not supported in v0.1; use WSL2 or a Linux/macOS host (see docs/windows.md)`.
The daemon-socket probes in `status`, `archive`, `uninstall` and `update`
resolve to `None` there, so every command behaves as "no daemon running".

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

Registering the task is not running the daemon. `install` deliberately does not
`schtasks /Run`, and the logon trigger fires a `dreamd watch` that still exits
**2** with the sentence above; `dreamd mcp` still refuses the same way. `dreamd
service start` / `restart` / `status` report what Task Scheduler thinks, not
what `watch` did, so a successful `/Run` can still leave `status` at `stopped`.
The task and the token exist so that the run which follows the *next* logon can
be a useful one: **AILAB-192** — TCP on localhost with a bearer token read from
`auth.json` — is the daemon/API port, and AILAB-203 is the supervisor and the
credential it will consume.

`io::write_atomic` is still `ErrorKind::Unsupported` on Windows. Durable JSONL
append remains open v0.1.1 work, and it is why `watch` and `mcp` keep refusing
rather than serving a store they cannot write safely.
