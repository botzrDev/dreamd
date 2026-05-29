---
last_updated: "2026-05-29T12:00:00Z"
prompt_version: "deterministic-only"
cluster_key: "rust::clippy"
---
<!-- dreamd:lesson id="evt_00000000000000000000000011" cluster="rust::clippy" -->
clippy::needless_borrow fires when you pass an already-referenced value with an extra & (for example &&str into a fn taking &str). Drop the extra borrow level — the compiler does not need it and it obscures the call site.
<!-- /dreamd:lesson -->
