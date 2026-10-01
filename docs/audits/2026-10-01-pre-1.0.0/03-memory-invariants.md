# Audit 3 — Memory invariants against the 1.0.0 claims

**Verdict: the Linux behavior the changelog claims is what these tests exercised, and that behavior passed.** This audit maps the `## [1.0.0]` highlights to the tests that ran on `bb84721`. It is not a coverage percentage. Coverage was not measured.

The highlights under test are the ones in `CHANGELOG.md` for 1.0.0: no new behavior in the tag, recall is BM25 × salience, the published binary is the default build, `dreamd vectors enable` exits 2 on that build, `rrf::fuse` is library-only, Windows learn works while `write_atomic` stays unsupported, `SPEC.md` stays v0.1, and episodic records stay `schema_version` `"1.0.0"`.

## Recall is BM25 × salience

| Evidence | What it pins | Result |
| --- | --- | --- |
| `salience.rs` unit tests inside the 557 | The product formula, including recency `exp(-age_days/14)`, zero pain or importance → 0, recurrence 0 → factor 1 | Passed |
| `salience_proptest` (5 tests, 512 cases) | Monotonic down in age; monotonic up in pain, importance, and recurrence; no NaN/Inf on the strategy's ranges | Passed, 0.02s |
| `collector.rs` tests inside the 557, plus `bm25_fastfield_integration` (3) | Query-time score from Tantivy fast fields; `recall_excluding` matches `recall` when the exclude list is empty | Passed |
| `semantic_indexing` (12) | Lesson documents rank on the same score. They carry inherited pain and importance, and their timestamp is derivation time | Passed |
| Quality suite axis 1 (audit 4) | A pain/importance 9 record outranks a pain/importance 1 record that has a higher BM25 on the same query, through MCP | Passed |

`collector.rs` `recall` does not call `rrf::fuse`. A search of `crates/` for `rrf::fuse` has no hits. `fuse` is defined only in `rrf.rs`. `download_model` is defined in `crates/dreamd-core/src/vectors.rs` and called from `crates/dreamd-cli/src/commands/vectors.rs`.

`rrf.rs` has 4 unit tests. They ran inside the 557. They check `fuse` on two id lists (`RRF_K = 60`, 1-based ranks). Nothing in the recall path calls `fuse`. There is no test whose failure would mean "recall started calling fuse." The 1.0.0 claim is true of this tree by inspection of `recall`, and the suite would stay green if a later edit wired `fuse` in without a new test.

## Default binary, vectors off

`dreamd version` on the debug binary this pass built printed `vectors: off`.

`commands/vectors.rs` `enable_without_feature_exits_2_and_names_the_feature` ran inside the CLI's 332 and passed. On a default build the command writes nothing to stdout, writes the "rebuild with --features vectors" line to stderr, and returns exit code 2.

`vectors.rs` in `dreamd-core` is `#[cfg(feature = "vectors")]`. Its one test checks that `MODEL_ID` is `BAAI/bge-small-en-v1.5` and does not call `download_model`. This pass did not compile that module. No test downloads the model or embeds text. That matches the changelog: a `--features vectors` build is not a release asset, and this pass did not pretend to test one.

## Episodic log, dream cycle, provenance, forget

| Invariant | Where it was tested | Result |
| --- | --- | --- |
| Append-only JSONL, daemon-minted ids, no torn tail | `weg12_concurrency_torture`: 1000 direct-channel appends, small payload and near-4KiB payload. `episodic.rs` tests. `episodic_scan_proptest` (512 cases): no panic, torn tail contains no newline, mid-file garbage counted malformed | Passed. Torture case 11.03s |
| WAL: store is pre-cycle or post-cycle | `wal.rs` (14 tests in the 557), `weg60_wal_recovery_startup` (1) | Passed |
| Dream cycle snapshot | `dream_cycle_snapshot` (3), `dream_cycle.rs` (26: 9 `#[test]` plus 17 `#[tokio::test]`), `consolidation.rs` (25) | Passed |
| Lesson layer is still BM25 × salience | `semantic_indexing` (12) | Passed |
| Ledger verify reads and does not rewrite | `provenance.rs` (18 tests in the 557) | Passed |
| Forget removes one live event and writes a receipt, not a signed membership proof | `forget.rs` (13), `coordinator.rs` `forget_removes_first_id_and_later_append_lands` | Passed |
| Record schema `"1.0.0"` | `dreamd-protocol` tests: JSON round trip, EventId Crockford rules, skill_action shape, legacy dotted keys. `roundtrip_proptest` 512 cases | Passed |
| SIGTERM unlinks the socket | `weg268_sigterm_socket` (1) | Passed |
| Single writer process | `weg21_writer_process` (1) | Passed |

Daemon schema in `version` output is `schema: 1.0` (daemon `STATE_SCHEMA_VERSION`), which is a different token from episodic `"1.0.0"`. Both showed up: the binary printed `schema: 1.0`, and the protocol tests round-trip `"1.0.0"`. `SPEC.md` was not modified by this commit and was not executed; the tag does not claim a spec bump.

## Windows

This host is Linux. `windows_stub_returns_unsupported` did not compile. The changelog's Windows sentence (learn over loopback; dream cycle and Tantivy stay unavailable because `write_atomic` returns `Unsupported`) was not executed here. CI's Windows test job is `continue-on-error`, so a red Windows run would not fail the workflow either.

## Holes that matter before calling the suite a proof of 1.0.0

- No test fails if `recall` starts calling `rrf::fuse`.
- No test downloads the embedding model, embeds, or ranks by vectors. The feature test only checks the model id, and it did not compile.
- No test on this host runs `write_atomic` on Windows.
- Property tests ran at 512 cases, not the nightly 1024.
- `cargo test --all-features` was not run, so the `vectors` cfg arms are unexecuted on this commit in this pass.

The holes are bounded. The paths the default Linux binary actually ships — append, recall, dream cycle, provenance verify, forget, and `vectors enable` exiting 2 — passed.
