# Optional vector backend (`vectors` feature)

**Status: a model download command only; vector recall is not implemented.** The `vectors` Cargo feature is off by default. The default `dreamd` binary does not link `fastembed`, and the release artifacts are built without the feature. Recall is still BM25 × salience. There is no JSONL vector index, nothing embeds text, and `embedding` ledger lines are still not written (see [`provenance.md`](./provenance.md)). `dreamd_core::rrf::fuse` fuses two best-first id lists by reciprocal rank (`1 / (RRF_K + rank)`, `RRF_K` = 60, 1-based rank), but recall does not call it: there is no dense ranking to fuse and no hybrid query mode (BZR-182).

## `dreamd vectors enable`

On the default binary the command refuses, exits 2, and creates no directory:

```text
$ dreamd vectors enable
dreamd vectors enable: this binary was built without the vectors feature; rebuild with --features vectors
```

On a `--features vectors` build it downloads `BAAI/bge-small-en-v1.5` (`fastembed::EmbeddingModel::BGESmallENV15`) into `~/.agent/models`, loads it once, and prints three lines:

```text
vectors=compiled
model=BAAI/bge-small-en-v1.5
cache=/home/you/.agent/models
```

The model weights are not in the binary; they are downloaded at run time. When `HF_HOME` is set and non-empty, fastembed downloads into that directory instead and `cache=` prints it. An unset or empty `HOME` exits 2 with `dreamd vectors enable: HOME is unset`. A download failure exits 1.

## Which binary do I have?

`dreamd --version` ends with `vectors:off` or `vectors:on`. `dreamd version` prints a `vectors:` line.

## Building with the feature

```bash
cargo build -p dreamd --features vectors
```

`dreamd` forwards the feature to `dreamd-core`. `fastembed`'s default features stay on, so the build fetches a prebuilt ONNX Runtime library. CI compiles, lints, and tests the feature because clippy and tests run with `--all-features`. The NFR-2 size gate builds the default features only, so it measures a binary without the embedder. The informational `size-report-vectors` job reports the stripped feature-build size and has no threshold.
