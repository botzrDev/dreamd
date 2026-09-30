//! Optional embedder seam (BZR-188). Compiled only with `--features vectors`.
//! Naming the model does not download it and does not embed text.

pub const MODEL_ID: &str = "BAAI/bge-small-en-v1.5";

pub fn model() -> fastembed::EmbeddingModel {
    fastembed::EmbeddingModel::BGESmallENV15
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
