//! Typed catalog refusals. Every variant is closed and carries **no** credential, endpoint body, or
//! provider response — a failure a caller can act on is a value, and a failure nobody can act on is
//! a bounded, redacted code, never a panic and never a raw upstream string.

/// Why a catalog refresh failed. These are the *observable, non-secret* reasons a `GET
/// /api/v1/providers/{id}/models` reports; the wire spelling of each is
/// [`crate::catalog::CatalogErrorCode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    /// The provider's protocol has no catalog adapter in v1 (e.g. a future/unknown protocol). The
    /// provider is still usable by manual model entry — catalog failure never disables dispatch.
    Unsupported,
    /// The provider has no usable credential right now (absent, denied, malformed, owner
    /// unavailable/unauthorized). Carried so the response can say *why* without a second read.
    NoCredential,
    /// The stored credential's binding does not match the current canonical endpoint. No provider I/O
    /// is performed in this case.
    BindingMismatch,
    /// The provider did not answer within the bounded adapter deadline.
    Timeout,
    /// The transport failed (connect/reset) after the request was admitted.
    Transport,
    /// The provider answered with a non-success status. Only the numeric status crosses the boundary.
    Status(u16),
    /// The provider body was not a well-formed model list.
    Malformed,
    /// The provider body exceeded the 8 MiB v1 catalog cap.
    BodyTooLarge,
    /// The operator-configured endpoint was refused by the broker's fixed-endpoint normalization
    /// (never a secret: the reason is a closed [`rhapsody_provider_broker::EndpointError`] spelling).
    Endpoint,
    /// A refresh was requested while another refresh for the same provider was already in flight.
    InFlight,
}

impl CatalogError {
    /// The bounded, non-secret wire code for this failure.
    pub fn code(&self) -> crate::catalog::CatalogErrorCode {
        use crate::catalog::CatalogErrorCode as C;
        match self {
            CatalogError::Unsupported => C::Unsupported,
            CatalogError::NoCredential => C::NoCredential,
            CatalogError::BindingMismatch => C::BindingMismatch,
            CatalogError::Timeout => C::Timeout,
            CatalogError::Transport => C::Transport,
            CatalogError::Status(_) => C::Status,
            CatalogError::Malformed => C::Malformed,
            CatalogError::BodyTooLarge => C::BodyTooLarge,
            CatalogError::Endpoint => C::Endpoint,
            CatalogError::InFlight => C::InFlight,
        }
    }

    /// A closed, actionable, operator-facing message. Never echoes an endpoint body or a credential.
    pub fn message(&self) -> String {
        match self {
            CatalogError::Unsupported => "this provider has no model catalog adapter".to_string(),
            CatalogError::NoCredential => {
                "no usable credential is configured for this provider".to_string()
            }
            CatalogError::BindingMismatch => {
                "the stored credential no longer matches the configured endpoint; rebind it in the \
                 desktop app"
                    .to_string()
            }
            CatalogError::Timeout => {
                "the provider did not answer before the catalog deadline".to_string()
            }
            CatalogError::Transport => "the provider could not be reached".to_string(),
            CatalogError::Status(status) => {
                format!("the provider answered with status {status}")
            }
            CatalogError::Malformed => "the provider returned a malformed model list".to_string(),
            CatalogError::BodyTooLarge => {
                "the provider model list exceeded the 8 MiB cap".to_string()
            }
            CatalogError::Endpoint => "the configured provider endpoint is not usable".to_string(),
            CatalogError::InFlight => {
                "a catalog refresh is already running for this provider".to_string()
            }
        }
    }
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for CatalogError {}
