# HTTP API reference

dreamd exposes a small REST API. The transport depends on the platform. On Unix it is a **Unix domain socket** at `~/.agent/dreamd.sock` (clients may override with `DREAMD_SOCK`; `dreamd watch` always binds `$HOME/.agent/dreamd.sock`), and there is no TCP listener. On Windows there is no peer credential to read off a socket, so `dreamd watch` listens on **loopback TCP** instead — `127.0.0.1` on an OS-chosen ephemeral port, published in `~/.agent/server.json` (AILAB-192). Loopback by default: `dreamd watch --insecure` (with `--bind`) is the only way to listen beyond localhost, it exists only on the TCP listener, and `server.json` still names a loopback host (AILAB-197).

All routes live under `/api/v1`. Every request requires an `X-Agent-Root` header. Authentication is whatever the transport can prove: on Unix the daemon enforces **peer UID matching** via `SO_PEERCRED` / `getpeereid` — only the user who started the daemon may connect — and on Windows it enforces an `Authorization: Bearer` token read from `~/.agent/auth.json`. The two are mutually exclusive; neither platform mounts both.

**Canonical source:** `crates/dreamd-core/src/server/http/` (`router.rs`, `handlers/`)

---

## Transport

### Unix (Linux, macOS)

| Property | Value |
|---|---|
| Socket path | `~/.agent/dreamd.sock` (or `$DREAMD_SOCK`) |
| Permissions | `0600` (owner read/write only) |
| Protocol | HTTP/1.1 over UDS |
| Host header | Use `localhost` (required by HTTP clients; not used for routing) |
| Authentication | Peer UID via `SO_PEERCRED` / `getpeereid`; mismatch is `403` |

### Windows

| Property | Value |
|---|---|
| Address | `127.0.0.1:<ephemeral>` by default — the port is picked by the OS at bind time. `dreamd watch --bind <IP[:port]>` picks another |
| Address file | `~/.agent/server.json`, written by `dreamd watch` as `{"host":"127.0.0.1","port":<port>}` and unlinked again on every shutdown path. The host is always loopback — `127.0.0.1`, or `::1` for an IPv6 bind — even when `--insecure` listens on `0.0.0.0` |
| Protocol | HTTP/1.1 over TCP |
| Host header | `127.0.0.1` or `localhost` (not used for routing) |
| Authentication | `Authorization: Bearer <64 lowercase hex>` matching `~/.agent/auth.json`; missing or wrong is `401` |
| Scope | Loopback unless `--insecure`. Without it a non-loopback `--bind` is refused before binding, and the daemon re-checks the address it bound. With it the daemon logs a warning on every start; the bearer token is still required. |

`~/.agent/server.json` is a discardable address hint, not memory state, so it is written with a plain `std::fs::write` rather than the atomic-replace path (which is still `ErrorKind::Unsupported` on Windows). A hard-killed daemon can therefore leave a stale file behind: liveness is "the file parses **and** `127.0.0.1:<port>` accepts a connection", never "the file exists". A `server.json` naming a non-loopback host is refused rather than connected to.

`~/.agent/auth.json` is minted by `dreamd service install` and rotated by `dreamd service install --force` (AILAB-203). `dreamd watch` only ever reads it: a missing or malformed file makes the daemon exit 1 pointing at `dreamd service install`, and `watch` never writes or rotates the token itself.

**What answers on Windows.** `dreamd watch` deliberately never opens the Tantivy index there — `TantivyIndexHandle::open` writes its first `index_manifest.json` through `io::write_atomic` — so the daemon boots with no index at all. `POST /api/v1/learn` works: the JSONL append is `OpenOptions` + `write_all` + `sync_data`, not an atomic replace. Everything index-backed does not. `POST /api/v1/dream` stays mounted and returns `500` (see [below](#post-apiv1dream)), and `GET /api/v1/recall` / `GET /api/v1/health` can fail the same way. A valid bearer token buys you past the auth layer; it is not a promise of a `200`.

### curl example (Unix socket smoke test)

```bash
# Replace with your project's absolute path (the directory containing .agent/, not .agent/ itself)
PROJECT=/home/you/your-project

curl --unix-socket ~/.agent/dreamd.sock \
  -H "X-Agent-Root: $PROJECT" \
  "http://localhost/api/v1/recall?q=axum&k=5"
```

The project path must be registered in `~/.agent/registry.toml` (done automatically by `dreamd init`). Paths are canonicalized before lookup — use the same absolute path the registry stores.

### curl example (Windows loopback smoke test)

```powershell
# The port is ephemeral, so read it back out of server.json each time
$srv  = Get-Content $env:USERPROFILE\.agent\server.json | ConvertFrom-Json
$tok  = (Get-Content $env:USERPROFILE\.agent\auth.json | ConvertFrom-Json).token
$proj = "C:\Users\you\your-project"

curl.exe -H "X-Agent-Root: $proj" -H "Authorization: Bearer $tok" `
  "http://$($srv.host):$($srv.port)/api/v1/recall?q=axum&k=5"
```

---

## Middleware

Requests pass through two middleware layers (outermost first). The outer one is the platform's authentication layer — `peer_uid_middleware` on Unix, `bearer_auth_middleware` on Windows — and the inner one, `agent_root_middleware`, is the same on both.

**Request ID.** Every response from a request that passed the authentication layer carries an `x-request-id` header. If the request sent its own `x-request-id` the daemon echoes that value; otherwise it generates a UUID. The same id is the `request_id` field on that request's `http request` line in `~/.agent/dreamd.log`. A request rejected by the authentication layer (`403` / `401`) gets no `x-request-id`.

### `peer_uid_middleware` (Unix only)

Compares the connecting process UID (injected at accept time from `SO_PEERCRED`) to the daemon owner's UID.

| Condition | Status | Body |
|---|---|---|
| Peer UID matches daemon UID | Pass through | — |
| Peer UID present but mismatched | `403 Forbidden` | `{"error":"forbidden: peer UID does not match daemon owner"}` |
| No peer UID extension | `403 Forbidden` | `{"error":"forbidden: peer UID not available"}` |

### `bearer_auth_middleware` (Windows only)

Compares the `Authorization` header against the token in `~/.agent/auth.json`. The file is re-read on **every** request and the two decoded 32-byte secrets are compared in constant time, so a `dreamd service install --force` rotation mid-session `401`s the next request without restarting the daemon.

| Condition | Status | Body |
|---|---|---|
| `Authorization: Bearer <token>` matches `auth.json` | Pass through | — |
| Header missing, or not valid ASCII | `401 Unauthorized` | `{"error":"…"}` |
| Header is not `Bearer <credential>` — wrong scheme, no credential, bare token | `401 Unauthorized` | `{"error":"…"}` |
| Credential present but does not match | `401 Unauthorized` | `{"error":"…"}` |
| `auth.json` missing, unreadable, or malformed | `401 Unauthorized` | `{"error":"…"}` |

The verdict is always **401, never 403**: `403` is the Unix peer-UID answer and that layer is not mounted on Windows. Rejection happens outermost, before the tracing layer, so a rejected caller never reaches a handler; the daemon logs a reason category only (`missing_authorization`, `malformed_authorization`, `no_auth_file`, `unreadable_auth_file`, `token_mismatch`) and never the presented credential or the file's contents.

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

#### Request body (`AppendLearningRequest`)

| Field | Type | Required | Notes |
|---|---|---|---|
| `pain` | number | Yes | `0.0`–`10.0` inclusive |
| `importance` | number | Yes | `0.0`–`10.0` inclusive |
| `pinned` | boolean | No | Default `false`. Dream-cycle pin union keeps pinned events through decay; `dreamd archive --force-unpin` clears pins. |
| `skill_action` | string | Yes | Clustering key; normalized and validated (see below) |
| `source_harness` | string | Yes | Provenance tag, e.g. `"cursor"`, `"claude-code"` |
| `content` | string | Yes | Free-text body; max ~4 KiB serialized line (413 if exceeded) |

**Daemon-owned fields:** `id`, `schema_version`, and `timestamp` are **not part of the request body**. The daemon mints the `evt_<ULID>` id and stamps the schema version and the UTC persistence timestamp on every durable write, so there is nothing useful a client can send. A body that still includes them — earlier versions of this document told you to — is **accepted and those values ignored**, never rejected. All three come back in the response.

**`skill_action` rules:** Trimmed, lowercased, whitespace collapsed to `_`, segments joined by `::`. Each segment must match `[a-z0-9_]+`. Max 256 bytes. Examples: `rust::error_handling`, `rust::cargo::test`. Rejects `.`, `/`, `-`, and empty segments.

#### Response

| Status | Body | When |
|---|---|---|
| `201 Created` | `{"id":"evt_…","timestamp":"…","deduplicated":false}` | New durable write |
| `201 Created` | `{"id":"evt_…","timestamp":"…","deduplicated":true}` | Duplicate `X-Client-Dedup-Key` within this project |
| `400 Bad Request` | `{"error":"…"}` | Invalid `skill_action`, score out of range, missing `X-Agent-Root` |
| `400` / `415` / `422` | plain text, not JSON | Body rejected before the handler runs: malformed JSON (`400`), `Content-Type` not `application/json` (`415`), a required field missing or of the wrong type (`422`). See [Error body shape](#error-body-shape) |
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
    "pain": 7.0,
    "importance": 8.0,
    "skill_action": "rust::error_handling::axum_rejection",
    "source_harness": "cursor",
    "content": "Route handlers must return impl IntoResponse; unwrapping panics."
  }' \
  http://localhost/api/v1/learn
```

**Windows:** available. The durable append is `OpenOptions` + `write_all` + `sync_data` on `AGENT_LEARNINGS.jsonl`, which needs no atomic rename, so `201` on Windows means the same durable line it means on Unix. What is missing is the index update behind it — see [Transport → Windows](#windows).

#### Idempotency (`X-Client-Dedup-Key`)

Optional opaque string. When present, a second `POST /learn` with the **same key and same project** returns the original `id` with `"deduplicated": true` instead of appending a duplicate line. Keys are scoped per coordinator (per project) — the same key on two different projects creates two distinct records.

---

### `GET /api/v1/recall`

BM25 lexical search with query-time salience scoring. Returns ranked matches from the Tantivy index: raw events (`source: "episodic"`) and dream-cycle lessons from `LESSONS.md` (`source: "semantic"`), ranked together. The query is matched against the `content` text only, on whole lowercased words with no stemming (`unwrap` does not match `unwrapping`); several words are OR-ed.

#### Request headers

| Header | Required | Description |
|---|---|---|
| `X-Agent-Root` | Yes | Absolute project root path |

#### Query parameters

| Param | Required | Default | Description |
|---|---|---|---|
| `q` | Yes | — | Search query string. Parsed by Tantivy's query parser, so `"`, `(`, `:` and `AND`/`OR` are query syntax; a query it cannot parse — `rust::error` is one, because of the `:` — is a `500` (see below). Send plain words |
| `k` | No | `5` (`DEFAULT_RECALL_K`) | Maximum results to return. Unsigned integer |
| `explain` | No | omitted | Exact value `1` adds a `citations` array (see below). Any other value, including absent, omits it. HTTP-only — MCP `search_nodes` has no equivalent parameter. |
| `exclude` | No | none | Repeatable (`exclude=evt_a&exclude=evt_b`). Drops the document with that exact `event_id` before the top-`k` cut, so up to `k` of the remaining hits come back. Nothing is deleted. There is no cluster parameter. HTTP-only. See [`docs/observability.md`](observability.md#counterfactual-recall---without--exclude-bzr-194). |

#### Response (`200 OK`)

```json
{
  "results": [
    {
      "score": 0.99,
      "bm25": 1.8,
      "salience": 0.55,
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
| `score` | Combined ranking score used for ordering: `bm25 × salience` |
| `bm25` | Raw BM25 relevance |
| `salience` | Query-time salience multiplier — the formula below without its BM25 term |
| `source` | Index layer: `"episodic"` (a raw event) or `"semantic"` (a `LESSONS.md` lesson). There are no other values |
| `content` | Matched text |
| `metadata.timestamp_sec` | Unix seconds: the event's timestamp, or for a lesson the time it was consolidated |
| `metadata.pain`, `metadata.importance` | Stored `0.0`–`10.0` scores |
| `metadata.recurrence` | Cluster recurrence count from index fast field (see below) |
| `metadata.skill_action` | Hierarchical cluster key of the matched learning (e.g. `rust::error_handling::axum_rejection`) |
| `metadata.source_harness` | Harness that authored the learning (e.g. `"cursor"`, `"claude-code"`) — makes recall cross-harness-attributable |

Hits carry no event id; request `explain=1` for the `citations[].id` of each hit.

**Score formula** (computed at query time, not stored):

```
BM25 × exp(-age_days / 14) × (pain / 10) × (importance / 10) × (1 + ln(1 + recurrence))
```

`salience` is everything after the `BM25 ×`. The `14` is an e-folding time constant in days, not a half-life.

#### `explain=1` response (`200 OK`)

`GET /api/v1/recall?q=axum&k=5&explain=1` adds a `citations` array, one entry per `results[]` hit in the same order, with the ungrouped salience factors behind that hit's score. See [`docs/observability.md`](observability.md) for the field reference.

```json
{
  "results": [ { "...": "as above" } ],
  "citations": [
    {
      "id": "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
      "score": 0.99,
      "bm25_component": 1.8,
      "salience_component": 0.55,
      "layer": "episodic",
      "snippet": "Route handlers must return impl IntoResponse…",
      "age_days": 12.4,
      "pain": 7.0,
      "importance": 8.0,
      "recurrence": 3
    }
  ]
}
```

#### Error responses

| Status | When |
|---|---|
| `400 Bad Request` | Missing `q`, or a `k` that is not an unsigned integer; missing `X-Agent-Root` |
| `403 Forbidden` | UID mismatch |
| `404 Not Found` | Unregistered project |
| `500 Internal Server Error` | Index open or search failure. This includes a `q` the query parser rejects (for example an unbalanced `"` or `(`), which answers `{"error":"recall failed: …"}` |

All of these use the `{"error":"…"}` envelope. An empty index, an empty `q`, or a query with no match returns `200` with `{"results":[]}` (plus `"citations":[]` under `explain=1`).

#### curl example

```bash
curl --unix-socket ~/.agent/dreamd.sock \
  -H "X-Agent-Root: $PROJECT" \
  "http://localhost/api/v1/recall?q=route+handlers&k=3"
```

**Read-after-write:** Newly appended learnings become searchable within one index commit cycle (5 seconds). A saturated coordinator → indexer channel no longer costs you the record — the coordinator awaits that send, so the update queues instead of being dropped and becomes searchable once the indexer drains. It does cost latency: while the coordinator is parked, in-flight learns wait in its own 256-slot channel with no per-request timeout, and only sustained overload past that inbox trips the 100 ms `COORDINATOR_SEND_TIMEOUT` into a `503` (HTTP only — the in-process `dreamd mcp` path has no such timeout and waits). If the daemon crashes between JSONL `sync_data` and the next Tantivy commit, recall may lag until startup replay. See [`GET /api/v1/health`](#get-apiv1health).

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
    "jsonl_tail_id": "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
    "last_indexed_id": "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
    "stale": false,
    "unindexed_count": 0
  }
}
```

| Field | Meaning |
|---|---|
| `stale` | `true` when JSONL has events strictly after `last_indexed_id` |
| `jsonl_tail_id` | `id` of the last well-formed JSONL record, or `null` on an empty log |
| `last_indexed_id` | Watermark from `index_progress.json`, or `null` if nothing has been indexed |
| `unindexed_count` | JSONL events after the watermark |

`stale: true` is normal for up to one commit cadence (5 s) after a live append. Channel saturation stretches it further — a queued append is in the JSONL and not yet committed, so it reads as `stale` until the indexer drains the backlog. Staleness that outlives the backlog indicates a crash gap between JSONL `sync_data` and the next commit; that is what restart replay heals.

#### curl example

```bash
curl --unix-socket ~/.agent/dreamd.sock \
  -H "X-Agent-Root: $PROJECT" \
  http://localhost/api/v1/health
```

---

### `POST /api/v1/dream`

Run a full dream cycle for the resolved project: consolidate episodic learnings into `LESSONS.md`, apply decay/pruning, update recurrence sidecar.

**Not available on Windows.** The route stays mounted rather than vanishing from the route table, and returns `500` with `{"error":"dream cycle is unavailable on this platform: atomic file replacement is unsupported (see docs/windows.md)"}`. Consolidation, decay, the `LESSONS.md` rewrite and the recurrence sidecar all *replace* files through `io::write_atomic`, which is still `ErrorKind::Unsupported` there.

#### Request headers

| Header | Required | Effect |
|---|---|---|
| `X-Agent-Root` | Yes | Canonical project root, as registered in `registry.toml`. |
| `x-dreamd-no-llm` | No | Exactly `1` forces the deterministic lesson body even when credentials exist (AILAB-204). Absent, or any other value, allows the LLM path. |
| `x-dreamd-share-personal` | No | Exactly `1` is per-call consent to include `.agent/personal/` in the composition prompt, capped at 16 KiB (AILAB-199). Absent, or any other value, excludes the personal layer — which is the default. Ignored under `x-dreamd-no-llm: 1`, where no request is made to consent to. |

Both optional headers match **exactly** and case-sensitively: `true`, `yes`, `0`, and non-UTF-8 bytes all read as absent, so a client that guesses the wire format gets the documented default rather than a silent behavior change. They are the wire form of `dreamd dream --no-llm` and `dreamd dream --share-personal`, which proxy to the daemon rather than skipping it — the daemon is the single writer of the project's episodic log.

No request body.

#### Response

| Status | Body | When |
|---|---|---|
| `200 OK` | `{"status":"ok"}` | Cycle completed |
| `409 Conflict` | `{"error":"dream cycle in progress"}` | `.agent/.dreamd/state.json` records the last cycle as `in_progress` |
| `400 Bad Request` | `{"error":"…"}` | Missing `X-Agent-Root` |
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
  "last_modified": "2026-06-20T14:30:00.123456789Z"
}
```

| Field | Description |
|---|---|
| `body` | File contents (empty string if file absent) |
| `last_modified` | RFC 3339 UTC timestamp (file mtime, `Z` suffix), or `null` if file absent |

A file that exists but cannot be read is `500` with `{"error":"preferences read failed: …"}`.

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

### `GET /api/v1/observability/salience`

Salience distribution over every episodic document in the project index: total, mean, a ten-bucket histogram, the top clusters, and the drift against the snapshot from exactly seven UTC days earlier. No query parameters and no request body. Despite the `GET`, each call writes today's snapshot to `.agent/.dreamd/observability/salience-<UTC date>.json`. `dreamd salience-drift` is the CLI form. Field semantics are in [`docs/observability.md`](observability.md#salience-distribution-get-apiv1observabilitysalience--dreamd-salience-drift-bzr-195).

#### Request headers

| Header | Required |
|---|---|
| `X-Agent-Root` | Yes |

#### Response (`200 OK`)

```json
{
  "schema_version": "observability/1.0",
  "date": "2026-09-29",
  "total": 42,
  "mean": 0.183,
  "histogram": [20, 9, 6, 3, 2, 1, 1, 0, 0, 0],
  "top_clusters": [
    { "skill_action": "rust::tokio", "count": 4, "mean": 0.41 }
  ],
  "drift": null,
  "drift_against": "2026-09-22"
}
```

| Field | Description |
|---|---|
| `schema_version` | Always `"observability/1.0"` |
| `date` | UTC date of this report |
| `total` | Episodic documents scored (lesson documents are not counted) |
| `mean` | Mean salience (no BM25 term — there is no query) |
| `histogram` | Ten counts; index `i` covers `[i/10, (i+1)/10)`, index 9 is `[0.9, +∞)` |
| `top_clusters` | At most 5 `skill_action` clusters by mean salience, descending |
| `drift` | Today's `mean` minus the `mean` of the snapshot dated `drift_against`; `null` when that snapshot is absent |
| `drift_against` | The date seven UTC days before `date` |

#### Error responses

| Status | When |
|---|---|
| `400 Bad Request` | Missing `X-Agent-Root` |
| `403 Forbidden` | UID mismatch |
| `404 Not Found` | Unregistered project |
| `500 Internal Server Error` | Index open, scan, or snapshot write failure |

#### curl example

```bash
curl --unix-socket ~/.agent/dreamd.sock \
  -H "X-Agent-Root: $PROJECT" \
  http://localhost/api/v1/observability/salience
```

---

## Reserved endpoints

### `POST /api/v1/migrate`

A stub. No schema migration is available over HTTP yet. The route sits behind the same middleware as every other `/api/v1` route: `peer_uid_middleware` on Unix (403 on missing or mismatched peer UID), `bearer_auth_middleware` on Windows (401), then `agent_root_middleware` (400 without `X-Agent-Root`, 404 for an unregistered root). Only a request that passes all of them reaches the stub. The request body is ignored, so no `Content-Type` is required.

The handler does not run a migration, call the CLI migrator, or write `.bak` files.

#### Request headers

| Header | Required |
|---|---|
| `X-Agent-Root` | Yes |

#### Response (`501 Not Implemented`)

```json
{"current_schema_version":"1.0.0","error":"no schema migration available"}
```

`current_schema_version` is the episodic `RECORD_SCHEMA_VERSION` (`dreamd_protocol::RECORD_SCHEMA_VERSION`, `"1.0.0"`), the same token `dreamd migrate --from` / `--to` take. It is not the daemon state token `"1.0"`. This is the only error body that carries a field besides `error`.

Operators who need a migration use `dreamd migrate`; see [migrate.md](./migrate.md).

#### curl example

```bash
curl --unix-socket ~/.agent/dreamd.sock \
  -X POST \
  -H "X-Agent-Root: $PROJECT" \
  http://localhost/api/v1/migrate
```

---

## Recurrence sidecar

`metadata.recurrence` in recall results comes from the Tantivy index fast field, not from the JSONL line at query time.

After each dream cycle, the coordinator writes `.agent/semantic/recurrence_counts.json` mapping `skill_action` → count. The indexer applies this sidecar by re-indexing affected documents with the authoritative recurrence value. Until the next dream cycle (or indexer sidecar apply), recurrence may reflect the value at index time.

See `crates/dreamd-core/src/server/tantivy_handle.rs` (`apply_recurrence_sidecar`) and [architecture/durability.md](./architecture/durability.md).

---

## Error body shape

Error responses produced by dreamd's middleware and handlers use a JSON object:

```json
{ "error": "human-readable message" }
```

Three kinds of response come from the framework (axum) instead and do **not** use that envelope:

| Status | Body | When |
|---|---|---|
| `400` / `415` / `422` | `text/plain` message | `POST /api/v1/learn` body rejected by the JSON extractor: unparseable JSON (`400`), `Content-Type` not `application/json` (`415`), missing or mistyped field (`422`) |
| `404 Not Found` | empty | A path that is not one of the routes on this page. The middleware runs first, so such a request without a valid `X-Agent-Root` gets that header's `400` / `404` JSON error instead |
| `405 Method Not Allowed` | empty, with an `Allow` header | A known path with the wrong method (e.g. `GET /api/v1/migrate`) |

---

## See also

- [configuration.md](./configuration.md) — `redaction` and other daemon settings
- [../SPEC.md](../SPEC.md) — on-disk schema and dream-cycle contract
- [../SECURITY.md](../SECURITY.md) — socket auth threat model
- [../ARCHITECTURE.md](../ARCHITECTURE.md) — coordinator routing and index pinning
