# dreamd-protocol

Serde types for [dreamd](https://github.com/botzrDev/dreamd)'s on-disk JSONL and HTTP API. This is the crate to depend on when you read or write a dreamd record from your own Rust code. It is `serde` and `chrono` only. Ids are minted by `dreamd-core`, not here.

```toml
[dependencies]
dreamd-protocol = "1.3"
serde_json = "1"
```

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use dreamd_protocol::{AgentLearning, EventId};

    let id = EventId::parse("evt_01ARZ3NDEKTSV4RRFFQ69G5FAV")?;
    let raw = r#"{
      "schema_version": "1.0.0",
      "id": "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
      "timestamp": "2026-05-13T08:38:00Z",
      "pain": 7.5,
      "importance": 8.0,
      "pinned": false,
      "skill_action": "rust::cargo::publish",
      "source_harness": "cursor",
      "content": "cargo publish uploads a version once"
    }"#;
    let learning: AgentLearning = serde_json::from_str(raw)?;
    assert_eq!(learning.id, id);
    Ok(())
}
```

`schema_version` on every record is `"1.0.0"`. A caller-supplied `id` is replaced when the dreamd daemon appends the line.

The installable product is the `dreamd` binary (`cargo install dreamd --version ^1.3`). This crate does not start a daemon or search an index.
