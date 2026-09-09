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
in the v0.1.1 milestone.

AILAB-174 made the crate *compile* for a Windows target — the Unix-only modules
are `#[cfg(unix)]` rather than unconditionally imported. Compiling is not
running: `dreamd watch` and `dreamd mcp` refuse rather than serve a store they
cannot durably write, exiting **2** with
`Windows is not supported in v0.1; use WSL2 or a Linux/macOS host (see docs/windows.md)`.
The daemon-socket probes in `status`, `archive`, `uninstall` and `update`
resolve to `None` there, so every command behaves as "no daemon running".
