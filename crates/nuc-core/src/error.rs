use thiserror::Error;

/// Shared application error type.
#[derive(Debug, Error)]
pub enum AppError {
    #[error("interface not found: {0}")]
    NotFound(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("parse error in {context}: {message}")]
    Parse { context: String, message: String },
    #[error("capability unsupported: {0}")]
    Unsupported(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, AppError>;
