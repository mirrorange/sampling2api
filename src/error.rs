use thiserror::Error;

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("invalid Anthropic request: {0}")]
    InvalidAnthropicRequest(String),
    #[error("unsupported Anthropic feature: {0}")]
    UnsupportedAnthropicFeature(String),
}
