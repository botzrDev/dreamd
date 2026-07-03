# Windows support

dreamd's durability guarantee relies on POSIX atomic-rename semantics
(`rename(2)`): each learning is written to a temporary file and atomically
renamed over the target, so a crash mid-write can never leave a partially
written JSONL line in the store. This is what `io::write_atomic` provides on
Linux and macOS.

**Windows durable writes land in v0.1.1.** Until then, the atomic-write path
returns [`std::io::ErrorKind::Unsupported`] on Windows rather than silently
falling back to a non-atomic write that could corrupt the store on a crash.
The daemon, MCP server, and read paths are otherwise portable; only the
crash-safe write path is gated.

Windows lifecycle support (service install, atomic writes) is tracked in the
v0.1.1 milestone.
