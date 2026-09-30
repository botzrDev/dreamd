//! Optional embedder seam (BZR-188). Compiled only with `--features vectors`.
//! Naming the model does not download it and does not embed text.

pub const MODEL_ID: &str = "BAAI/bge-small-en-v1.5";

pub fn model() -> fastembed::EmbeddingModel {
    fastembed::EmbeddingModel::BGESmallENV15
}

/// Download `MODEL_ID` into `cache_dir` and load it. `HF_HOME`, when set,
/// wins over `cache_dir` inside fastembed. Does not embed text.
pub fn download_model(cache_dir: &std::path::Path) -> Result<(), fastembed::Error> {
    let _loaded = fastembed::TextEmbedding::try_new(
        fastembed::TextInitOptions::new(model())
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(false),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_is_bge_small_en_v15() {
        assert_eq!(model(), fastembed::EmbeddingModel::BGESmallENV15);
        assert_eq!(MODEL_ID, "BAAI/bge-small-en-v1.5");
    }
}
