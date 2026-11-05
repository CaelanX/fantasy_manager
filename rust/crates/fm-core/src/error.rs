//! Error convention (ARCHITECTURE.md "Errors"): library code returns `anyhow::Result<T>`;
//! errors that Python code CATCHES BY TYPE are typed (thiserror) so callers can
//! `err.downcast_ref::<ProviderError>()` exactly where Python had `except ProviderError`.

use thiserror::Error;

/// `providers.base.ProviderError` (user-facing provider failure). `Unavailable` mirrors
/// `providers.ProviderUnavailable`, `LoginFailed` mirrors `fantrax_auth.LoginFailed`.
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("{0}")]
    Message(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    LoginFailed(String),
}

/// Core-model errors (`LookupError` in `LeagueContext.my_team`, bad config values).
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("{0}")]
    Lookup(String),
    #[error("{0}")]
    Value(String),
}
