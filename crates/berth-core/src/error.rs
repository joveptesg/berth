use thiserror::Error;

/// Result alias for Berth operations.
pub type Result<T, E = BerthError> = std::result::Result<T, E>;

/// Domain error shared by all Berth crates.
#[derive(Debug, Error)]
pub enum BerthError {
    /// The requested entity does not exist or is hidden by the access policy.
    #[error("not found")]
    NotFound,
    /// The request is malformed or violates a domain rule.
    #[error("{0}")]
    Validation(String),
    /// Authentication failed.
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// Unexpected failure: I/O, database, storage backend.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Authentication failures that clients must tell apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AuthError {
    /// No credentials or invalid credentials.
    #[error("authentication required")]
    Unauthenticated,
    /// The device was revoked: the client must wipe its local data and downloads.
    #[error("device revoked")]
    DeviceRevoked,
    /// The refresh token expired: the client must re-pair, downloads stay playable.
    #[error("session expired")]
    SessionExpired,
}
