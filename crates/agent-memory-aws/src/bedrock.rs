//! [`EmbeddingProvider`] backed by Amazon Bedrock Titan Text Embeddings V2.

use agent_memory_core::{Embedding, EmbeddingProvider, MemoryError};
use async_trait::async_trait;
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::primitives::Blob;

use crate::error::EmbeddingError;

/// Titan Text Embeddings V2. Its valid output sizes are 256, 512 and 1024.
pub const TITAN_EMBED_TEXT_V2: &str = "amazon.titan-embed-text-v2:0";

#[derive(Debug, Clone)]
pub struct BedrockEmbedder {
    client: Client,
    model_id: String,
    dimensions: usize,
}

impl BedrockEmbedder {
    pub fn new(client: Client, model_id: impl Into<String>, dimensions: usize) -> Self {
        Self {
            client,
            model_id: model_id.into(),
            dimensions,
        }
    }

    /// Titan V2 at the dimension the vector index in this project uses.
    pub fn titan_v2(client: Client, dimensions: usize) -> Self {
        Self::new(client, TITAN_EMBED_TEXT_V2, dimensions)
    }
}

#[async_trait]
impl EmbeddingProvider for BedrockEmbedder {
    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn embed(&self, text: &str) -> Result<Embedding, MemoryError> {
        // `normalize: true` gives unit-length vectors. With COSINE that changes
        // nothing about ranking, but it keeps the stored data compatible with a
        // DOT_PRODUCT index, where unnormalised vectors would let sheer
        // magnitude outrank actual relevance.
        let body = serde_json::json!({
            "inputText": text,
            "dimensions": self.dimensions,
            "normalize": true,
        });

        let response = self
            .client
            .invoke_model()
            .model_id(&self.model_id)
            .content_type("application/json")
            .accept("application/json")
            .body(Blob::new(body.to_string().into_bytes()))
            .send()
            .await
            .map_err(|error| {
                MemoryError::embedding_provider(EmbeddingError::InvokeModel {
                    model_id: self.model_id.clone(),
                    source: Box::new(error),
                })
            })?;

        let parsed: TitanResponse =
            serde_json::from_slice(response.body().as_ref()).map_err(|error| {
                MemoryError::embedding_provider(EmbeddingError::MalformedBody(error))
            })?;

        let Some(values) = parsed.embedding else {
            return Err(MemoryError::embedding_provider(
                EmbeddingError::MissingEmbedding,
            ));
        };

        // Truncate to f32 here, on the way in. The vector index stores f32, so
        // keeping f64 in the base table would leave the table and the index
        // holding different numbers for the same memory.
        let values: Vec<f32> = values.into_iter().map(|value| value as f32).collect();

        Embedding::with_dimensions(values, self.dimensions)
    }
}

#[derive(Debug, serde::Deserialize)]
struct TitanResponse {
    embedding: Option<Vec<f64>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_titan_response_is_parsed_into_its_embedding() {
        let parsed: TitanResponse =
            serde_json::from_str(r#"{"embedding":[0.1,-0.2],"inputTextTokenCount":7}"#)
                .expect("parses");
        assert_eq!(parsed.embedding, Some(vec![0.1, -0.2]));
    }

    #[test]
    fn a_response_without_an_embedding_is_detected_rather_than_defaulted() {
        let parsed: TitanResponse =
            serde_json::from_str(r#"{"inputTextTokenCount":7}"#).expect("parses");
        assert!(parsed.embedding.is_none());
    }

    #[test]
    fn f64_values_narrow_to_the_f32_precision_the_index_stores() {
        // 0.1 has no exact binary representation, so the f64 and f32 forms
        // differ. Narrowing on write is what keeps the base table and the
        // vector index in agreement.
        let narrowed = 0.1_f64 as f32;
        assert_eq!(narrowed.to_string(), "0.1");
        assert_ne!(f64::from(narrowed), 0.1_f64);
    }
}
