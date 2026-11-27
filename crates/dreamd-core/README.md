# dreamd-core

The engine behind the [`dreamd`](https://crates.io/crates/dreamd) binary: the episodic log, the Tantivy BM25 index, salience, the dream cycle, and the MCP and HTTP servers.

Install the binary if you want agent memory:

```bash
cargo install dreamd --version ^1.3
```

Depend on this crate to link that engine into another Rust process. Pin the version you built against. Public modules are the engine itself, and minor releases have moved them.

```toml
[dependencies]
dreamd-core = "=1.4.1"
```

On-disk record types live in [`dreamd-protocol`](https://crates.io/crates/dreamd-protocol), which is the smaller crate to depend on when you only need to parse JSONL.

`cargo install dreamd-core` is the wrong command. The helper bins in this package exist so the workspace tests can spawn them.

Rust 1.95.0 or newer. The optional `vectors` feature fetches a prebuilt ONNX Runtime at compile time and adds a command that downloads an embedding model at run time. It does not change recall ranking. The default build does not include it.
