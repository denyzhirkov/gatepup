use gatepup_config::{ConfigError, ValidationError};
use gatepup_proxy::ProxyError;
use thiserror::Error;

/// Failure of a top-level use case. Wraps lower-level config errors and carries
/// the full set of validation problems so the caller can render all of them.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error("config is invalid ({} problem(s))", .0.len())]
    Invalid(Vec<ValidationError>),

    #[error("failed to serialize config: {0}")]
    Serialize(#[from] serde_json::Error),

    #[error(transparent)]
    Proxy(#[from] ProxyError),
}

impl CoreError {
    /// The individual validation problems, if this is a validation failure.
    pub fn validation_errors(&self) -> Option<&[ValidationError]> {
        match self {
            CoreError::Invalid(errors) => Some(errors),
            _ => None,
        }
    }
}
