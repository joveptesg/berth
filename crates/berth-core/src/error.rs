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
    /// The request conflicts with the current state, e.g. the server is already set up.
    #[error("{0}")]
    Conflict(String),
    /// Authentication failed.
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// The device is authenticated but lacks the scope the request needs.
    #[error("forbidden")]
    Forbidden,
    /// Too many attempts from this client; retry later.
    #[error("too many requests")]
    RateLimited,
    /// Unexpected failure: I/O, database, storage backend.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Authentication failures that clients must tell apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AuthError {
    /// No credentials or invalid credentials; an expired access token is refreshed and retried.
    #[error("authentication required")]
    Unauthenticated,
    /// The device was revoked: the client must wipe its local data and downloads.
    #[error("device revoked")]
    DeviceRevoked,
    /// The refresh token expired or was revoked: the client must re-pair, downloads stay playable.
    #[error("session expired")]
    SessionExpired,
    /// The pairing code is past its short lifetime.
    #[error("pairing token expired")]
    PairingTokenExpired,
    /// The pairing code was already exchanged for device tokens.
    #[error("pairing token already used")]
    PairingTokenUsed,
    /// The pairing code is unknown.
    #[error("pairing token invalid")]
    PairingTokenInvalid,
    /// The login or the password is wrong, or the login is turned off.
    #[error("login failed")]
    LoginFailed,
}

impl AuthError {
    /// Machine-readable `code` of the problem response.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Unauthenticated => "unauthenticated",
            Self::DeviceRevoked => "device_revoked",
            Self::SessionExpired => "session_expired",
            Self::PairingTokenExpired => "pairing_token_expired",
            Self::PairingTokenUsed => "pairing_token_used",
            Self::PairingTokenInvalid => "pairing_token_invalid",
            Self::LoginFailed => "login_failed",
        }
    }
}
