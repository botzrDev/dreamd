# Optional vector backend (`vectors` feature)

**Status: a compile-time seam only; vector recall is not implemented.** The `vectors` Cargo feature is off by default. The default `dreamd` binary does not link `fastembed`, and the release artifacts are built without the feature. With `--features vectors`, `dreamd-core` compiles a `vectors` module that names the embedding model (`BAAI/bge-small-en-v1.5`, `fastembed::EmbeddingModel::BGESmallENV15`) and does nothing else: it does not download the model, does not embed text, and nothing in the binary calls it. This page does not add `dreamd vectors enable` or a model download; those are a later ticket (BZR-181), as is hybrid ranking (BZR-182). Recall is still BM25 × salience. `embedding` ledger lines are still not written (see [`provenance.md`](./provenance.md)).

## Building with the feature

```bash
cargo build -p dreamd --features vectors
```

`dreamd` forwards the feature to `dreamd-core`. `fastembed`'s default features stay on, so the build fetches a prebuilt ONNX Runtime library. CI compiles, lints, and tests the feature because clippy and tests run with `--all-features`. The NFR-2 size gate builds the default features only, so it measures a binary without the embedder.
