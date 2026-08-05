use async_trait::async_trait;
use futures::stream::BoxStream;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::message::{CompletionRequest, StreamEvent};

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("http request failed: {0}")]
    Http(String),
    #[error("provider returned an error: {0}")]
    Api(String),
    #[error("failed to parse provider response: {0}")]
    Parse(String),
    #[error("missing API key for provider {0}")]
    MissingApiKey(String),
    #[error("request cancelled")]
    Cancelled,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Stable id, e.g. "anthropic", "openai".
    fn id(&self) -> &'static str;

    fn supports_tools(&self) -> bool {
        true
    }

    /// Stream a completion. The returned stream yields normalized `StreamEvent`s;
    /// the caller accumulates them into a `Message`.
    async fn stream_completion(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>;
}
