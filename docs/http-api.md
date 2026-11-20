# HTTP API reference

dreamd exposes a small REST API. The transport depends on the platform. On Unix it is a **Unix domain socket**, one per user, at `~/.agent/dreamd.sock` unless `DREAMD_SOCK` names another absolute path; `dreamd watch` and every client resolve it the same way. There is no TCP listener. On Windows there is no peer credential to read off a socket, so `dreamd watch` listens on **loopback TCP** instead — `127.0.0.1` on an OS-chosen ephemeral port, published in `~/.agent/server.json` (AILAB-192). Loopback by default: `dreamd watch --insecure` (with `--bind`) is the only way to listen beyond localhost, it exists only on the TCP listener, and `server.json` still names a loopback host (AILAB-197).

All routes live under `/api/v1`. Every request requires an `X-Agent-Root` header. Authentication is whatever the transport can prove: on Unix the daemon enforces **peer UID matching** via `SO_PEERCRED` / `getpeereid` — only the user who started the daemon may connect — and on Windows it enforces an `Authorization: Bearer` token read from `~/.agent/auth.json`. The two are mutually exclusive; neither platform mounts both.

**Canonical source:** `crates/dreamd-core/src/server/http/` (`router.rs`, `handlers/`)

---

## Transport

### Unix (Linux, macOS)

| Property | Value |
|---|---|
| Socket path | `~/.agent/dreamd.sock`, or `$DREAMD_SOCK` when set. `dreamd watch` binds it and clients connect to it. One per user; `X-Agent-Root` picks the project |
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

`~/.agent/server.json` is a discardable address hint, not memory state, so it is written with a plain `std::fs::write` rather than `io::write_atomic`. A hard-killed daemon can therefore leave a stale file behind: liveness is "the file parses **and** `127.0.0.1:<port>` accepts a connection", never "the file exists". A `server.json` naming a non-loopback host is refused rather than connected to.

`~/.agent/auth.json` is minted by `dreamd service install` and rotated by `dreamd service install --force` (AILAB-203). `dreamd watch` only ever reads it: a missing or malformed file makes the daemon exit 1 pointing at `dreamd service install`, and `watch` never writes or rotates the token itself.

**What answers on Windows.** `dreamd watch` opens the Tantivy index. `POST /api/v1/learn` appends with `OpenOptions` + `write_all` + `sync_data`. `POST /api/v1/dream` runs the cycle. `GET /api/v1/recall` reads the index. `GET /api/v1/health` compares the JSONL tail to `index_progress.json` and returns `200` when that read succeeds. `stale` is true when the log has events past the watermark, including when the watermark file is absent and the log is not empty. A valid bearer token buys you past the auth layer. A `200` from health is that watermark comparison.

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

**Request ID.** Every response from a request that passed the authentication layer and the version check carries an `x-request-id` header. If the request sent its own `x-request-id` the daemon echoes that value; otherwise it generates a UUID. The same id is the `request_id` field on that request's `http request` line in `~/.agent/dreamd.log`. A request rejected by the authentication layer (`403` / `401`) or by the version check (`426`) gets no `x-request-id` and no `http request` line.

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

### `version_middleware`

The version check (BZR-1519). It runs after the authentication layer and before `agent_root_middleware`. A dreamd client sends `x-dreamd-version` with its own version on every request. The versions are compared as strings.

| Condition | Status | Body |
|---|---|---|
| No `x-dreamd-version` header (curl, or a client from a release without this check) | Pass through | |
| Header equal to the daemon's version | Pass through | |
| Any other value, empty included | `426 Upgrade Required` | `` {"error":"version mismatch: client <client>, daemon <daemon>. Stop the other daemon and start this one with `dreamd watch`, or point DREAMD_SOCK at a daemon of this version."} `` |

dreamd's own clients (`dreamd mcp` in daemon mode and the `dreamd dream` proxy) also ask first: once per process, before their first call, they send `GET /api/v1/health` with no `X-Agent-Root`, which the daemon answers `400` without opening a project. A missing or different `x-dreamd-version` on that answer stops the call before anything is sent. Only a pass is remembered, so a client probes again after a failure. When the client is the older side, its sentence says so and tells the user to update the client, not to restart the daemon.

Every response from this layer and the layers inside it, the `426` included, carries `x-dreamd-version` with the daemon's version. A `403` / `401` from the authentication layer does not. A dreamd client treats a `426`, a different version, or a missing header on any status but `401` and `403` as a version mismatch. It builds its own sentence from the daemon's `x-dreamd-version` and does not show the `426` body. Under the npm shim (`DREAMD_LAUNCHER=npx`) that sentence names `npx -y dreamd-mcp update`, then `npx -y dreamd-mcp watch`.

### `agent_root_middleware`

Validates `X-Agent-Root` and resolves the project via `~/.agent/registry.toml`.

| Condition | Status | Body |
|---|---|---|
| Header missing | `400 Bad Request` | `{"error":"X-Agent-Root header missing: send the absolute path of the folder that holds .agent/"}` |
| Header bytes not UTF-8 | `400 Bad Request` | `{"error":"X-Agent-Root is not UTF-8: send the absolute path of the folder that holds .agent/"}` |
| Header empty or a relative path. Nothing is resolved against the daemon's working directory | `400 Bad Request` | `{"error":"X-Agent-Root must be the absolute path of the folder that holds .agent/; got \".\""}`, ending in the value sent |
| Path not in registry | `404 Not Found` | `{"error":"agent root not registered"}`. A clone or a worktree is a new path: run `dreamd init` in that folder once, and the next request goes through with no restart |
| Registry read/parse failure | `500 Internal Server Error` | `{"error":"registry read failed"}` |
| Registered project found | Pass through | Injects `ProjectEntry` into request extensions |

**Header:** `X-Agent-Root: /absolute/path/to/project`

Value is the **project root** (parent of `.agent/`), not the `.agent/` directory itself.

The value is the path's UTF-8 text sent as raw bytes, with no percent-encoding, so `-H "X-Agent-Root: $PWD"` works for any UTF-8 path, accented letters included. A path that is not valid UTF-8 is not supported.

---

## Endpoints

### `POST /api/v1/learn`

Append one episodic learning. The coordinator mints the event ID, stamps `schema_version`, redacts content (if enabled), and durably writes to `AGENT_LEARNINGS.jsonl`.

#### Request headers

| Header | Required | Description |
|---|---|---|
| `X-Agent-Root` | Yes | Absolute project root path (see middleware) |
| `Content-Type` | Yes | `application/json` |
| `X-Client-Dedup-Key` | No | Retry key, read when the body has no `client_dedup_key` (see [Idempotency](#idempotency-x-client-dedup-key)) |

#### Request body (`LearnRequest`)

| Field | Type | Required | Notes |
|---|---|---|---|
| `pain` | number | Yes | A JSON number, checked against `0.0`–`10.0` inclusive as sent, then stored as the nearest 32-bit float. `10.0000001` is refused, not rounded to `10.0` first |
| `importance` | number | Yes | Same as `pain` |
| `pinned` | boolean | No | Default `false`. Dream-cycle pin union keeps pinned events through decay; `dreamd archive --force-unpin` clears pins. |
| `skill_action` | string | Yes | Clustering key; normalized and validated (see below) |
| `source_harness` | string | Yes | Provenance tag, e.g. `"cursor"`, `"claude-code"` |
| `content` | string | Yes | Free-text body; max ~4 KiB serialized line (413 if exceeded) |
| `client_dedup_key` | string | No | Retry key (see [Idempotency](#idempotency-x-client-dedup-key)). Any characters, non-ASCII and newlines included. When the `X-Client-Dedup-Key` header is also sent, the two must be equal |

**Daemon-owned fields:** `id`, `schema_version`, and `timestamp` are **not part of the request body**. The daemon mints the `evt_<ULID>` id and stamps the schema version and the UTC persistence timestamp on every durable write, so there is nothing useful a client can send. A body that still includes them — earlier versions of this document told you to — is **accepted and those values ignored**, never rejected. All three come back in the response.

**`skill_action` rules:** Trimmed, lowercased, whitespace collapsed to `_`, segments joined by `::`. Each segment must match `[a-z0-9_]+`. Max 256 bytes. Examples: `rust::error_handling`, `rust::cargo::test`. Rejects `.`, `/`, `-`, and empty segments.

**Check order:** `skill_action`, then `pain`, then `importance`, then redaction of `content`, then the size cap (`413`). The first failure is the answer, so a body with a bad `skill_action` and a bad `pain` gets the `skill_action` message. Before these the daemon reads the project's config (`500` if it does not load). In-process `dreamd mcp` runs the same function in the same order, so `append_node` answers alike in both modes.

#### Response

| Status | Body | When |
|---|---|---|
| `201 Created` | `{"id":"evt_…","timestamp":"…","deduplicated":false}` | New durable write |
| `201 Created` | `{"id":"evt_…","timestamp":"…","deduplicated":true}` | Same key and same learning within this project's coordinator |
| `400 Bad Request` | `{"error":"…"}` | Invalid `skill_action`, score out of range, an `X-Agent-Root` the middleware refuses |
| `400 Bad Request` | `{"error":"X-Client-Dedup-Key is not UTF-8"}` | The header's bytes are not UTF-8. Nothing is written |
| `400 Bad Request` | `{"error":"client_dedup_key in the body and X-Client-Dedup-Key differ"}` | The body and the header both carry a key, and they differ. Nothing is written |
| `400` / `415` / `422` | plain text, not JSON | Body rejected before the handler runs: malformed JSON (`400`), `Content-Type` not `application/json` (`415`), a required field missing or of the wrong type (`422`). See [Error body shape](#error-body-shape) |
| `403 Forbidden` | `{"error":"…"}` | UID mismatch |
| `404 Not Found` | `{"error":"…"}` | Unregistered project |
| `413 Payload Too Large` | `{"error":"payload too large"}` | Serialized line exceeds cap |
| `500 Internal Server Error` | `{"error":"config load failed: …"}` | The project's `.agent/.dreamd/config.toml` (or the user-level config) does not parse. Nothing is written. The daemon reads the project's config on every learn, so fixing the file fixes the next request |
| `503 Service Unavailable` | `{"error":"coordinator busy, retry"}` | Coordinator channel full; includes `Retry-After: 1` |
| `500 Internal Server Error` | `{"error":"…"}` | Coordinator or routing failure |

If the project's `.agent/.dreamd/` was deleted or replaced since the daemon opened it, for example by `git clean -fdX`, the daemon opens the project again and the request goes through. `learn`, `recall` and `dream` do this. While another dreamd command holds the new store's writer lock, they answer `500` `project writer lock is held by pid N (…)`. See [troubleshooting](troubleshooting.md#git-clean--fdx-deleted-agentdreamd).

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

Optional string, for retrying one write. The key is the body's `client_dedup_key`, or, when the body has none, the `X-Client-Dedup-Key` header, read as UTF-8 from its bytes; daemon-mode MCP sends it in the body. A second `POST /learn` with the **same key and the same learning** (`content` after redaction, `skill_action`, `source_harness`, `pain`, `importance` and `pinned`) in the same project returns the original `id` with `"deduplicated": true` and writes nothing. The same key with a different learning is stored as a new record with its own id, and a retry of either learning then returns that learning's id. Derive the key from the content, or use one per write attempt.

Keys are scoped per coordinator (per project) — the same key on two different projects creates two distinct records. They live in memory only: a daemon restart, or the daemon evicting an idle project's coordinator, forgets every key. An empty or all-whitespace key counts as no key.

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
| `q` | Yes | — | Any text. dreamd splits it on whitespace and into words with the content field's tokenizer. A piece that splits into several words, such as `parse_config()` or `README.md`, matches as a phrase. Quotes, colons, `AND`/`OR`/`NOT`, `+`, `-`, `*` and parentheses are not operators. Text with no words matches nothing. At most 16384 bytes once decoded; a longer `q` is a `414` |
| `k` | No | `5` (`DEFAULT_RECALL_K`) | Maximum results: a whole number from 0 to 100 (`MAX_RECALL_K`). Default `5`. A larger `k` is a `400` `{"error":"k must be at most 100"}`; `0` returns no results |
| `explain` | No | omitted | Exact value `1` adds a `citations` array (see below). Any other value, including absent, omits it. HTTP-only — MCP `search_nodes` has no equivalent parameter. |
| `exclude` | No | none | Repeatable (`exclude=evt_a&exclude=evt_b`). Drops the document with that exact `event_id` before the top-`k` cut, so up to `k` of the remaining hits come back. Nothing is deleted. There is no cluster parameter. HTTP-only. See [`docs/observability.md`](observability.md#counterfactual-recall---without--exclude-bzr-194). |

#### Response (`200 OK`)

```json
{
  "results": [
    {
      "id": "evt_01J1Z7Q8K4M2N6P3R5S7T9V0WX",
      "score": 0.77,
      "bm25": 1.8,
      "salience": 0.43,
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
| `id` | The document's stored id: an event's `evt_…` id, or `lsn_` followed by the exemplar event's id for a lesson |
| `score` | Combined ranking score used for ordering: `bm25 × salience` |
| `bm25` | Raw BM25 relevance |
| `salience` | Query-time salience multiplier — the formula below without its BM25 term |
| `source` | Index layer: `"episodic"` (a raw event) or `"semantic"` (a `LESSONS.md` lesson). There are no other values |
| `content` | Matched text |
| `metadata.timestamp_sec` | Unix seconds: the event's timestamp, or for a lesson the time it was consolidated |
| `metadata.pain`, `metadata.importance` | Stored `0.0`–`10.0` scores |
| `metadata.recurrence` | The raw count: live episodic events whose `skill_action` equals the hit's or extends it with `::`, computed from the index per searcher (see below). The formula applies `d` to it |
| `metadata.skill_action` | Hierarchical cluster key of the matched learning (e.g. `rust::error_handling::axum_rejection`) |
| `metadata.source_harness` | Harness that authored the learning (e.g. `"cursor"`, `"claude-code"`) — makes recall cross-harness-attributable |

An event's `id` is what `dreamd forget`, `dreamd recall --without` and `exclude=` take. A lesson's `lsn_…` id works with `exclude=` and `--without`; `dreamd forget` refuses it.

**Score formula** (computed at query time, not stored):

```
BM25 × exp(-age_days / 14) × (pain / 10) × (importance / 10) × (1 + ln(1 + d(recurrence)))

d(0) = 0
d(r) = 1.5 - 0.5 / r    for r >= 1
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
      "score": 0.77,
      "bm25_component": 1.8,
      "salience_component": 0.43,
      "layer": "episodic",
      "snippet": "Route handlers must return impl IntoResponse…",
      "age_days": 12.4,
      "pain": 7.0,
      "importance": 8.0,
      "recurrence": 1.3333333333333333
    }
  ]
}
```

#### Error responses

| Status | When |
|---|---|
| `400 Bad Request` | Missing `q`, or a `k` that is not an unsigned integer; or an `X-Agent-Root` the middleware refuses |
| `400 Bad Request` | `k` over 100: `{"error":"k must be at most 100"}` |
| `403 Forbidden` | UID mismatch |
| `404 Not Found` | Unregistered project |
| `414 URI Too Long` | `q` is over 16384 bytes once decoded: `{"error":"query exceeds 16384 bytes; pass a shorter query"}`. A request target over 65,534 bytes is refused by hyper before routing, with an empty body |
| `500 Internal Server Error` | Index open or search failure |

All of these use the `{"error":"…"}` envelope, except hyper's 414, which has an empty body. An empty index, an empty `q`, or a query with no match returns `200` with `{"results":[]}` (plus `"citations":[]` under `explain=1`).

#### curl example

```bash
curl --unix-socket ~/.agent/dreamd.sock \
  -H "X-Agent-Root: $PROJECT" \
  "http://localhost/api/v1/recall?q=route+handlers&k=3"
```

The same call with citations, and with one event left out of the ranking:

```bash
curl --unix-socket ~/.agent/dreamd.sock \
  -H "X-Agent-Root: $PROJECT" \
  --get \
  --data-urlencode "q=route handlers" \
  --data-urlencode "k=3" \
  --data-urlencode "explain=1" \
  --data-urlencode "exclude=evt_01ARZ3NDEKTSV4RRFFQ69G5FAV" \
  "http://localhost/api/v1/recall"
```

`explain=1` adds `citations` for the hits that remain after `exclude`. Repeat `--data-urlencode "exclude=…"` for another id. There is no Windows curl in this section; the Windows recall smoke test is under [Transport](#windows).

**Read-after-write:** A recall finds every learning whose `POST /api/v1/learn` returned before the recall was sent. When the index has not committed one of them, the recall asks the indexer to commit first and waits at most one second; a coverage pass or a long backlog can hold the indexer that long, and past it the recall searches the last commit. A recall with nothing pending sends no commit request and does not wait. Without a recall, appends commit on the 5-second cadence. A saturated coordinator → indexer channel no longer costs you the record — the coordinator awaits that send, so the update queues instead of being dropped and becomes searchable once the indexer drains. It does cost latency: while the coordinator is parked, in-flight learns wait in its own 256-slot channel with no per-request timeout, and only sustained overload past that inbox trips the 100 ms `COORDINATOR_SEND_TIMEOUT` into a `503` (HTTP only — the in-process `dreamd mcp` path has no such timeout and waits). If the daemon crashes between JSONL `sync_data` and the next Tantivy commit, recall may lag until the next coverage pass (the next open, or a resync). See [`GET /api/v1/health`](#get-apiv1health).

---

### `GET /api/v1/health`

Report whether the index has caught up to the log for the resolved project. The values come from the daemon's own coordinator and index, kept in memory, so the endpoint reads no file. The first call for a project the daemon has not loaded loads it, as recall does, so a registered root without `.agent/` gets `500` here too. JSONL is the source of truth; this endpoint surfaces recall freshness without opening a search.

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
| `stale` | `true` when `unindexed_count` is above 0 |
| `jsonl_tail_id` | The largest id in the log as the daemon last read or wrote it, or `null` on an empty log |
| `last_indexed_id` | The largest event id the index has committed, or `null` if nothing has been indexed |
| `unindexed_count` | Appends this daemon made that the index has not committed yet |

`stale: true` is normal after a live append until the next commit: the 5 s cadence, or a recall, whichever comes first. Channel saturation stretches it further — a queued append is in the JSONL and not yet committed, so it reads as `stale` until the indexer drains the backlog. A change to the log made outside the daemon (a `git checkout`, a hand edit) does not show here: `dreamd doctor`'s `index_freshness` line compares the log with the committed index, and the daemon's next coverage pass heals the difference.

On Windows health compares the JSONL tail with `index_progress.json` on disk (see [Transport → Windows](#windows)). The index is open; this route does not read the Unix in-memory marks.

#### curl example

```bash
curl --unix-socket ~/.agent/dreamd.sock \
  -H "X-Agent-Root: $PROJECT" \
  http://localhost/api/v1/health
```

---

### `POST /api/v1/dream`

Run a full dream cycle for the resolved project: consolidate episodic learnings into `LESSONS.md`, apply decay/pruning, update recurrence sidecar.

On Windows the route runs the same cycle. The daemon is loopback TCP with a bearer token, and there is no Unix-socket proxy. A cycle whose log did not change during the model call keeps that body.

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
| `200 OK` | `{"status":"ok","autobiography":{…},"notices":[…]}` | Cycle completed. `autobiography` and `notices` are described below |
| `409 Conflict` | `{"error":"dream cycle in progress"}` | A cycle is open in the project's coordinator, or `.agent/.dreamd/state.json` records the last cycle as `in_progress` |
| `400 Bad Request` | `{"error":"…"}` | An `X-Agent-Root` the middleware refuses |
| `403 Forbidden` | `{"error":"…"}` | UID mismatch |
| `404 Not Found` | `{"error":"…"}` | Unregistered project |
| `503 Service Unavailable` | `{"error":"coordinator busy, retry"}` | `Retry-After: 1` |
| `500 Internal Server Error` | `{"error":"…"}` | Coordinator, WAL, or indexer failure |

The 200 body's `autobiography` object reports the git autobiography commit (BZR-1467). Its `outcome` is one of:

| `outcome` | Other keys | Meaning |
|---|---|---|
| `committed` | `commit` (the new commit's id) | The cycle committed the episodic log, and `LESSONS.md` when that file exists |
| `unchanged` | — | Both files already match HEAD, so no commit was made |
| `no_repo` | — | No git repo at or above the project root |
| `skipped` | `reason`, `files` | The commit was held. `user_dirty_tree`: the listed files hold changes no dream cycle wrote; commit them with git and the next cycle commits again. `no_baseline`: dreamd has no record yet of what its last cycle wrote, and the next cycle commits; when `files` lists `LESSONS.md`, the commit waits until you commit those files with git. Git's state can also hold the commit, and then `files` is empty: `detached_head` (HEAD is a checked-out tag or commit; check out a branch), `merge_in_progress`, `rebase_in_progress`, `apply_in_progress` (a `git am` or `git rebase --apply`), `cherry_pick_in_progress` and `revert_in_progress` (finish or abort it), and `bisect_in_progress` (run `git bisect reset`). The cycle's changes stay in the working tree, and the next cycle on a branch commits them |

```json
{"status":"ok","autobiography":{"outcome":"skipped","reason":"user_dirty_tree","files":[".agent/semantic/LESSONS.md"]}}
```

The key is absent when the autobiography phase did not run or failed (a failure is logged at ERROR, and the cycle still succeeds). A daemon before 1.3.0 answers `{"status":"ok"}`. Key order is not part of the contract.

`lesson_held` is present when the cycle kept `LESSONS.md` as it was, because the file held bytes no dream cycle wrote: `{"reason":"user_dirty_tree","file":".agent/semantic/LESSONS.md"}`, with `reason` `user_dirty_tree` or `no_baseline` as for `autobiography`. That cycle made no model call and did not rewrite or remove the file; pins and decay still ran on the log. The hold lasts until the file is committed with git or deleted. The first `notices` line then starts `lesson kept:`, and the body has no copy line and no no-key line. The key is absent when the lesson was not held, and a daemon before 1.4.0 never sends it.

`notices` is an array of strings, present only when it is non-empty. A deterministic lesson says it is a copy of the representative event, not a summary. When the caller did not send `x-dreamd-no-llm: 1` and the daemon has no key for the vendor its model goes to, one line names the variable and the `secrets.toml` key it looked for, and says that a key in the caller's shell is not used. When the lesson cites events, one line names the ids that were pinned.

#### curl example

```bash
curl --unix-socket ~/.agent/dreamd.sock \
  -X POST \
  -H "X-Agent-Root: $PROJECT" \
  http://localhost/api/v1/dream
```

---

### `GET /api/v1/preferences`

Returns the contents of `.agent/personal/PREFERENCES.md` for the resolved project. The file is served only when it resolves to a regular file inside `.agent/personal/`, the rule the dream cycle uses for `personal/`. A symlink within `personal/` is followed; one that leads outside it is refused. The daemon reads the file off its async workers, at most 16 KiB plus one byte, with a 5 s deadline.

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
| `body` | File contents as UTF-8, at most 16 KiB (empty string if file absent) |
| `last_modified` | RFC 3339 UTC timestamp (file mtime, `Z` suffix), or `null` if file absent |

A missing file, or a symlink whose target is missing, is `200` with an empty body.

#### Errors

Each is `500` with `{"error":"…"}`:

| `error` | Cause |
|---|---|
| `preferences read failed: .agent/personal/PREFERENCES.md resolves outside .agent/personal/, so it is not served` | The path, through a symlink, leads outside `.agent/personal/` |
| `preferences read failed: .agent/personal/PREFERENCES.md is not a regular file` | A FIFO, a device or a folder |
| `preferences read timed out after 5 s` | The read did not finish in 5 s, for example on a stuck mount |
| `preferences read failed: …` | The file exists but could not be opened or read |

#### Truncation

The body is valid UTF-8 of at most **16 KiB** (16,384 bytes), cut at a character boundary. Response headers:

| Header | Value |
|---|---|
| `X-Dreamd-Truncated` | `true` when the body was cut at the cap |
| `X-Dreamd-Original-Size` | With `X-Dreamd-Truncated`: the file's size on disk, in bytes |
| `X-Dreamd-Lossy` | `true` when bytes that are not UTF-8 were replaced with U+FFFD |

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
| `400 Bad Request` | An `X-Agent-Root` the middleware refuses |
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

A stub. No schema migration is available over HTTP yet. The route sits behind the same middleware as every other `/api/v1` route: `peer_uid_middleware` on Unix (403 on missing or mismatched peer UID), `bearer_auth_middleware` on Windows (401), then `agent_root_middleware` (400 for a missing, non-UTF-8 or relative `X-Agent-Root`, 404 for an unregistered root). Only a request that passes all of them reaches the stub. The request body is ignored, so no `Content-Type` is required.

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

## Recurrence and the sidecar

`metadata.recurrence` in recall results is computed from the index, once per searcher: the number of live episodic events whose `skill_action` equals the hit's or extends it with `::`, all-time. An event counts itself, so it is at least 1. A lesson hit gets the count of its `cluster_key`. A new event raises the count of every event in its key's subtree at the next index commit. `citations[].recurrence` is `d` of this count.

`.agent/semantic/recurrence_counts.json` is the dream cycle's promotion report: one count per promoted key, the events that key claimed. Recall does not read it; `dreamd doctor --cluster-health` and the provenance `recurrence` edges do.

See `crates/dreamd-core/src/recurrence.rs` and [architecture/durability.md](./architecture/durability.md).

---

## Error body shape

Error responses produced by dreamd's middleware and handlers use a JSON object:

```json
{ "error": "human-readable message" }
```

Four kinds of response come from the framework (axum or hyper) instead and do **not** use that envelope:

| Status | Body | When |
|---|---|---|
| `400` / `415` / `422` | `text/plain` message | `POST /api/v1/learn` body rejected by the JSON extractor: unparseable JSON (`400`), `Content-Type` not `application/json` (`415`), missing or mistyped field (`422`) |
| `404 Not Found` | empty | A path that is not one of the routes on this page. The middleware runs first, so such a request without a valid `X-Agent-Root` gets that header's `400` / `404` JSON error instead |
| `405 Method Not Allowed` | empty, with an `Allow` header | A known path with the wrong method (e.g. `GET /api/v1/migrate`) |
| `414 URI Too Long` | empty | A request target over 65,534 bytes. hyper refuses it before any middleware or route runs |

---

## MCP tool errors

`dreamd mcp` returns a rejected `search_nodes` or `append_node` call as a JSON-RPC error with one of these `message` strings. *Daemon mode* is `dreamd mcp` while `dreamd watch` is running: the MCP server forwards each call to this API. *In-process* is `dreamd mcp` with no daemon. A row applies to both unless it says otherwise, and its `message` is the same in both modes: an `append_node` argument is checked once, by the process that owns the store, in the [check order](#post-apiv1learn) of `POST /learn`.

| Code | `message` | Cause |
|---|---|---|
| `-32602` | `failed to deserialize parameters: …` | An argument is missing or has the wrong type. rmcp rejects the call before dreamd sees it |
| `-32602` | `invalid skill_action: segment contains characters outside [a-z0-9_]` | A `::` segment of `append_node`'s `skill_action` has a character other than a lowercase letter, a digit or `_`. The other reasons after `invalid skill_action: ` are `empty after normalization`, `exceeds 256 bytes` and `empty segment (stray ':' or leading/trailing '::')` |
| `-32602` | `pain must be in 0.0..=10.0` | `pain` is outside 0–10 |
| `-32602` | `importance must be in 0.0..=10.0` | `importance` is outside 0–10 |
| `-32602` | `learning exceeds 4096 bytes as a JSONL line; shorten content` | The record, serialized as one JSONL line, is over 4096 bytes |
| `-32602` | `query exceeds 16384 bytes; pass a shorter query` | `search_nodes`' `query` is over 16384 bytes |
| `-32602` | `k must be at most 100` | `search_nodes`' `k` is over 100. In daemon mode the MCP server refuses it before calling the daemon |
| `-32602` | the daemon's `error` string, or `daemon rejected the request (HTTP 400)` | Daemon mode only: any other `400` from the daemon |
| `-32600` | `` this folder is not set up for dreamd: there is no .agent/ store in <folder> or a folder above it. Run `dreamd setup` in the project folder, then restart this MCP server. `` | Both tools, in both modes: no project store at or above the folder `dreamd mcp` started in (`--project-root` when given). `~/.agent/` is the daemon home and never counts as one. Nothing is written and no daemon is called. Under the npm shim the command is `npx -y dreamd-mcp setup`. The server keeps this answer until the harness restarts it |
| `-32600` | `` the store in <folder> is not registered with the running daemon. Run `dreamd init` in <folder> once: it registers the existing store without changing its files (a clone or a worktree is not registered until then), and the next call works without a restart. `` | Daemon mode only: the daemon answered `404`, because the store folder is not in `~/.agent/registry.toml`. A fresh clone or worktree is a new path, so it is not registered until `init` runs there. Under the npm shim the command is `npx -y dreamd-mcp init`. See [GUIDE §9](../GUIDE.md#9-clones-and-worktrees) |
| `-32600` | `config load failed: …` | `append_node`, both modes: the project's config does not parse. dreamd reads it on every append, so an edit made while the server runs applies to the next call. Nothing is written. Fix the file; the call's arguments are not the problem |
| `-32600` | the daemon's `X-Agent-Root …` string | Daemon mode only: the daemon refused the project path the MCP server sent. This version's `dreamd mcp` always sends an absolute UTF-8 path, so this means a bug or a client from another version |
| `-32600` | `recall failed: …` | In-process only: the index could not be searched |
| `-32600` | `index open failed: …` | In-process only: the index could not be opened |
| `-32600` | `` <root>/.agent/.dreamd was deleted or replaced while this dreamd process (mcp) was using it, for example by `git clean -fdX`. It stopped writing to the store. Reload your MCP harness so it starts dreamd again. `` | In-process only: `.agent/.dreamd/` was deleted or replaced while the session ran. Every later call gets the same answer. In daemon mode the daemon opens the project again and the call goes through; see [`POST /learn`](#post-apiv1learn) |
| `-32603` | `daemon error 500 Internal Server Error: {"error":"…"}` | Daemon mode only: the daemon could not open or search the index |
| `-32603` | `daemon error 503 Service Unavailable: {"error":"coordinator busy, retry"}` | Daemon mode only: the daemon's append queue is full. Retry |
| `-32603` | `daemon connect failed: …` | Daemon mode only: the daemon stopped after the session started. The other stages are `handshake`, `request` and `response read` |
| `-32603` | `` version mismatch: client …, daemon …. Stop the other daemon and start this one with `dreamd watch`, or point DREAMD_SOCK at a daemon of this version. `` | Daemon mode only: the daemon runs another dreamd version. `daemon unknown` in place of a version means it sent none. Under the npm shim the fix names `npx -y dreamd-mcp update`, then `npx -y dreamd-mcp watch`, then a harness reload |
| `-32603` | `` the daemon at <socket> did not answer within 30 s. Run `dreamd status`; if it says `not responding`, stop that `dreamd watch` and start it again. `` | Daemon mode only: the daemon accepted the connection but did not finish answering within the [client deadline](#client-deadlines). For `append_node` the message goes on: `The append may have been written. Retry with the same client_dedup_key so it cannot be stored twice.` |

### Client deadlines

Every call a dreamd client makes to the daemon has one deadline, which covers the connect, the HTTP handshake, the request and the whole response. When it runs out, the client closes the connection and reports that the daemon did not answer, naming its socket. It never falls back to an in-process store, because the daemon still holds the store's writer lock.

| Call | Deadline |
|---|---|
| `search_nodes`, `append_node` (daemon mode) | 30 s |
| `dreamd dream`, proxied to the daemon | 120 s: the model's whole retry budget (3 attempts × 30 s), plus 30 s for the cycle |
| The version check, `GET /api/v1/health`, before a process's first call: `dreamd mcp`'s first tool call and every proxied `dreamd dream` | 30 s. A daemon that never answers therefore fails a proxied `dreamd dream` after 30 s, before the cycle starts |
| `dreamd status` | 2 s, for its one `GET /api/v1/health` with no `X-Agent-Root`. The daemon answers that `400` without opening a project. Any answer is `daemon: running`; none is `daemon: not responding`, and `status` exits 1 |

`DREAMD_CLIENT_TIMEOUT`, a whole number of seconds (1 or more), replaces the 30 s and 120 s deadlines; `status` keeps its 2 s. A timed-out `append_node` is `-32603` and may have been written: the daemon may have stored it before the deadline ran out. Retry it with the same `client_dedup_key`, and the daemon will not store it twice.

---

## See also

- [configuration.md](./configuration.md) — `redaction` and other daemon settings
- [../SPEC.md](../SPEC.md) — on-disk schema and dream-cycle contract
- [../SECURITY.md](../SECURITY.md) — socket auth threat model
- [../ARCHITECTURE.md](../ARCHITECTURE.md) — coordinator routing and index pinning
