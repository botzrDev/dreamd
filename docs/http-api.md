# HTTP API reference

dreamd exposes a small REST API over a **Unix domain socket** at `~/.agent/dreamd.sock` (override with `DREAMD_SOCK`). There is no TCP listener in v0.1.

All routes live under `/api/v1`. Every request requires an `X-Agent-Root` header. On Unix, the daemon also enforces **peer UID matching** via `SO_PEERCRED` / `getpeereid` — only the user who started the daemon may connect.

**Canonical source:** `crates/dreamd-core/src/server/http.rs`

---

## Transport

| Property | Value |
|---|---|
| Socket path | `~/.agent/dreamd.sock` (or `$DREAMD_SOCK`) |
| Permissions | `0600` (owner read/write only) |
| Protocol | HTTP/1.1 over UDS |
| Host header | Use `localhost` (required by HTTP clients; not used for routing) |

### curl example (socket smoke test)

```bash
# Replace with your project's absolute path (the directory containing .agent/, not .agent/ itself)
PROJECT=/home/you/your-project

curl --unix-socket ~/.agent/dreamd.sock \
  -H "X-Agent-Root: $PROJECT" \
  "http://localhost/api/v1/recall?q=axum&k=5"
```

The project path must be registered in `~/.agent/registry.toml` (done automatically by `dreamd init`). Paths are canonicalized before lookup — use the same absolute path the registry stores.

---

## Middleware

Requests pass through two middleware layers (outermost first):

### `peer_uid_middleware` (Unix only)

Compares the connecting process UID (injected at accept time from `SO_PEERCRED`) to the daemon owner's UID.

| Condition | Status | Body |
|---|---|---|
| Peer UID matches daemon UID | Pass through | — |
| Peer UID present but mismatched | `403 Forbidden` | `{"error":"forbidden: peer UID does not match daemon owner"}` |
| No peer UID extension | `403 Forbidden` | `{"error":"forbidden: peer UID not available"}` |

### `agent_root_middleware`

Validates `X-Agent-Root` and resolves the project via `~/.agent/registry.toml`.

| Condition | Status | Body |
|---|---|---|
| Header missing or non-UTF-8 | `400 Bad Request` | `{"error":"missing or non-UTF-8 X-Agent-Root header"}` |
| Path not in registry | `404 Not Found` | `{"error":"agent root not registered"}` |
| Registry read/parse failure | `500 Internal Server Error` | `{"error":"registry read failed"}` |
| Registered project found | Pass through | Injects `ProjectEntry` into request extensions |

**Header:** `X-Agent-Root: /absolute/path/to/project`

Value is the **project root** (parent of `.agent/`), not the `.agent/` directory itself.

---

## Endpoints

### `POST /api/v1/learn`

Append one episodic learning. The coordinator mints the event ID, stamps `schema_version`, redacts content (if enabled), and durably writes to `AGENT_LEARNINGS.jsonl`.

#### Request headers

| Header | Required | Description |
|---|---|---|
| `X-Agent-Root` | Yes | Absolute project root path (see middleware) |
| `Content-Type` | Yes | `application/json` |
| `X-Client-Dedup-Key` | No | Idempotency key (see below) |

#### Request body (`AgentLearning`)

| Field | Type | Required | Notes |
|---|---|---|---|
| `schema_version` | string | Yes | Client may send any value; server overwrites with `"1.0.0"` |
| `id` | string | Yes | Placeholder accepted; server overwrites with daemon-minted `evt_<ULID>` |
| `timestamp` | string (RFC 3339) | Yes | ISO 8601 UTC timestamp |
| `pain` | number | Yes | `0.0`–`10.0` inclusive |
| `importance` | number | Yes | `0.0`–`10.0` inclusive |
| `pinned` | boolean | No | Default `false`; reserved for v0.2 |
| `skill_action` | string | Yes | Clustering key; normalized and validated (see below) |
| `source_harness` | string | Yes | Provenance tag, e.g. `"cursor"`, `"claude-code"` |
| `content` | string | Yes | Free-text body; max ~4 KiB serialized line (413 if exceeded) |

**`skill_action` rules:** Trimmed, lowercased, whitespace collapsed to `_`, segments joined by `::`. Each segment must match `[a-z0-9_]+`. Max 256 bytes. Examples: `rust::error_handling`, `rust::cargo::test`. Rejects `.`, `/`, `-`, and empty segments.

#### Response

| Status | Body | When |
|---|---|---|
| `201 Created` | `{"id":"evt_…","timestamp":"…","deduplicated":false}` | New durable write |
| `201 Created` | `{"id":"evt_…","timestamp":"…","deduplicated":true}` | Duplicate `X-Client-Dedup-Key` within this project |
| `400 Bad Request` | `{"error":"…"}` | Invalid `skill_action`, score out of range, missing header |
| `403 Forbidden` | `{"error":"…"}` | UID mismatch |
| `404 Not Found` | `{"error":"…"}` | Unregistered project |
| `413 Payload Too Large` | `{"error":"payload too large"}` | Serialized line exceeds cap |
| `503 Service Unavailable` | `{"error":"coordinator busy, retry"}` | Coordinator channel full; includes `Retry-After: 1` |
| `500 Internal Server Error` | `{"error":"…"}` | Coordinator or routing failure |

#### curl example

```bash
PROJECT=/home/you/your-project

curl --unix-socket ~/.agent/dreamd.sock \
  -X POST \
  -H "X-Agent-Root: $PROJECT" \
  -H "Content-Type: application/json" \
  -H "X-Client-Dedup-Key: axum_unwrap_in_handler" \
  -d '{
    "schema_version": "1.0.0",
    "id": "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
    "timestamp": "2026-06-23T12:00:00Z",
    "pain": 7.0,
    "importance": 8.0,
    "skill_action": "rust::error_handling::axum_rejection",
    "source_harness": "cursor",
    "content": "Route handlers must return impl IntoResponse; unwrapping panics."
  }' \
  http://localhost/api/v1/learn
```

#### Idempotency (`X-Client-Dedup-Key`)

Optional opaque string. When present, a second `POST /learn` with the **same key and same project** returns the original `id` with `"deduplicated": true` instead of appending a duplicate line. Keys are scoped per coordinator (per project) — the same key on two different projects creates two distinct records.

---

### `GET /api/v1/recall`

BM25 lexical search with query-time salience scoring. Returns ranked episodic matches from the Tantivy index.

#### Request headers

| Header | Required | Description |
|---|---|---|
| `X-Agent-Root` | Yes | Absolute project root path |

#### Query parameters

| Param | Required | Default | Description |
|---|---|---|---|
| `q` | Yes | — | Search query string |
| `k` | No | `5` | Maximum results to return |

#### Response (`200 OK`)

```json
{
  "results": [
    {
      "score": 0.42,
      "bm25": 1.8,
      "salience": 0.42,
      "source": "episodic",
      "content": "Route handlers must return impl IntoResponse…",
      "metadata": {
        "timestamp_sec": 1719144000,
        "pain": 7.0,
        "importance": 8.0,
        "recurrence": 3,
        "skill_action": "rust::error_handling::axum_rejection",
        "source_harness": "cursor"
      }
    }
  ]
}
```

| Field | Description |
|---|---|
| `score` | Combined ranking score used for ordering |
| `bm25` | Raw BM25 relevance |
| `salience` | Query-time salience (see formula below) |
| `source` | Index layer (`"episodic"`, `"semantic"`, etc.) |
| `content` | Matched text |
| `metadata.recurrence` | Cluster recurrence count from index fast field (see below) |
| `metadata.skill_action` | Hierarchical cluster key of the matched learning (e.g. `rust::error_handling::axum_rejection`) |
| `metadata.source_harness` | Harness that authored the learning (e.g. `"cursor"`, `"claude-code"`) — makes recall cross-harness-attributable |

**Salience formula** (computed at query time, not stored):

```
BM25 × exp(-age_days / 14) × (pain / 10) × (importance / 10) × (1 + ln(1 + recurrence))
```

#### Error responses

| Status | When |
|---|---|
| `400 Bad Request` | Missing `q` parameter |
| `403 Forbidden` | UID mismatch |
| `404 Not Found` | Unregistered project |
| `500 Internal Server Error` | Index open or search failure |

Empty index returns `200` with `"results": []`.

#### curl example

```bash
curl --unix-socket ~/.agent/dreamd.sock \
  -H "X-Agent-Root: $PROJECT" \
  "http://localhost/api/v1/recall?q=axum+unwrap&k=3"
```

**Read-after-write:** Newly appended learnings become searchable within one index commit cycle (5 seconds in v0.1). If the coordinator → indexer channel saturates (`try_send` drops) or the daemon crashes between JSONL `sync_data` and the next Tantivy commit, recall may lag until startup replay. See [`GET /api/v1/health`](#get-apiv1health).

---

### `GET /api/v1/health`

Report whether the on-disk Tantivy watermark (`index_progress.json`) has caught up to the JSONL tail for the resolved project. JSONL is the source of truth; this endpoint surfaces recall freshness without opening a search.

#### Request headers

| Header | Required |
|---|---|
| `X-Agent-Root` | Yes |

#### Response (`200`)

```json
{
  "index": {
    "stale": false,
    "jsonl_tail_id": "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
    "last_indexed_id": "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
    "unindexed_count": 0
  }
}
```

| Field | Meaning |
|---|---|
| `stale` | `true` when JSONL has events strictly after `last_indexed_id` |
| `jsonl_tail_id` | `id` of the last well-formed JSONL record, if any |
| `last_indexed_id` | Watermark from `index_progress.json`, if present |
| `unindexed_count` | JSONL events after the watermark |

`stale: true` is normal for up to one commit cadence (5 s) after a live append. Persistent staleness indicates channel saturation or a crash gap; restart replay heals it.

#### curl example

```bash
curl --unix-socket ~/.agent/dreamd.sock \
  -H "X-Agent-Root: $PROJECT" \
  http://localhost/api/v1/health
```

---

### `POST /api/v1/dream`

Run a full deterministic dream cycle for the resolved project: consolidate episodic learnings into `LESSONS.md`, apply decay/pruning, update recurrence sidecar.

#### Request headers

| Header | Required |
|---|---|
| `X-Agent-Root` | Yes |

No request body.

#### Response

| Status | Body | When |
|---|---|---|
| `200 OK` | `{"status":"ok"}` | Cycle completed |
| `409 Conflict` | `{"error":"dream cycle in progress"}` | WAL shows `in_progress` |
| `403 Forbidden` | `{"error":"…"}` | UID mismatch |
| `404 Not Found` | `{"error":"…"}` | Unregistered project |
| `503 Service Unavailable` | `{"error":"coordinator busy, retry"}` | `Retry-After: 1` |
| `500 Internal Server Error` | `{"error":"…"}` | Coordinator, WAL, or indexer failure |

#### curl example

```bash
curl --unix-socket ~/.agent/dreamd.sock \
  -X POST \
  -H "X-Agent-Root: $PROJECT" \
  http://localhost/api/v1/dream
```

---

### `GET /api/v1/preferences`

Returns the contents of `.agent/personal/PREFERENCES.md` for the resolved project.

#### Request headers

| Header | Required |
|---|---|
| `X-Agent-Root` | Yes |

#### Response (`200 OK`)

```json
{
  "body": "# My Preferences\n\nI prefer concise answers.\n",
  "last_modified": "2026-06-20T14:30:00+00:00"
}
```

| Field | Description |
|---|---|
| `body` | File contents (empty string if file absent) |
| `last_modified` | RFC 3339 timestamp, or `null` if file absent |

#### Truncation

Files larger than **16 KiB** are truncated. Response includes:

| Header | Value |
|---|---|
| `X-Dreamd-Truncated` | `true` |
| `X-Dreamd-Original-Size` | Full byte count before truncation |

#### curl example

```bash
curl --unix-socket ~/.agent/dreamd.sock \
  -H "X-Agent-Root: $PROJECT" \
  http://localhost/api/v1/preferences
```

---

## Recurrence sidecar

`metadata.recurrence` in recall results comes from the Tantivy index fast field, not from the JSONL line at query time.

After each dream cycle, the coordinator writes `.agent/semantic/recurrence_counts.json` mapping `skill_action` → count. The indexer applies this sidecar by re-indexing affected documents with the authoritative recurrence value. Until the next dream cycle (or indexer sidecar apply), recurrence may reflect the value at index time.

See `crates/dreamd-core/src/server/tantivy_handle.rs` (`apply_recurrence_sidecar`) and [architecture/durability.md](./architecture/durability.md).

---

## Error body shape

All error responses use a JSON object:

```json
{ "error": "human-readable message" }
```

Axum JSON deserialization failures (malformed body, wrong content type) return standard Axum status codes (`415 Unsupported Media Type`, etc.) without the dreamd error envelope.

---

## See also

- [configuration.md](./configuration.md) — `redaction` and other daemon settings
- [../SPEC.md](../SPEC.md) — on-disk schema and dream-cycle contract
- [../SECURITY.md](../SECURITY.md) — socket auth threat model
- [../ARCHITECTURE.md](../ARCHITECTURE.md) — coordinator routing and index pinning
