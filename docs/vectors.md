# Optional vector backend (`vectors` feature)

**Status: a model download command only; vector recall is not implemented. `dreamd vectors enable` and the `vectors` feature are deprecated in 1.1.0 and will be removed in 2.0 (BZR-1245); recall never used the model.** The `vectors` Cargo feature is off by default. The default `dreamd` binary does not link `fastembed`, and the release artifacts are built without the feature. Recall is still BM25 × salience. There is no JSONL vector index, nothing embeds text, and `embedding` ledger lines are still not written (see [`provenance.md`](./provenance.md)). There is no dense ranking to fuse and no hybrid query mode.

## `dreamd vectors enable`

Every build first prints a one-line deprecation warning on stderr. On the default binary the command then refuses with one more line, exits 2, and creates no directory:

```text
$ dreamd vectors enable
dreamd: warning — `dreamd vectors enable` and the `vectors` cargo feature are deprecated and will be removed in dreamd 2.0; recall never used the model
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

`dreamd --version` prints one line whose last field is `vectors:off` or `vectors:on`, for example `dreamd 1.0.0 (<commit> build:<date> target:<triple> schema:1.0 vectors:off)`. `dreamd version` prints a `vectors: off` or `vectors: on` line.

## Building with the feature

```bash
cargo build -p dreamd --features vectors
```

`dreamd` forwards the feature to `dreamd-core`. `fastembed`'s default features stay on, so the build fetches a prebuilt ONNX Runtime library. CI compiles, lints, and tests the feature because clippy and tests run with `--all-features`. The NFR-2 size gate builds the default features only, so it measures a binary without the embedder. The informational `size-report-vectors` job reports the stripped feature-build size and has no threshold.
