//! Typed Linear adapter error categories — parity port of
//! `internal/tracker/linear/errors.go` (upstream §11.4).
//!
//! Go exposes each category as an `errors.New("linear_...")` sentinel and wraps it with
//! `fmt.Errorf("%w: <detail>", sentinel, …)`; callers test membership with `errors.Is`. Rhapsody
//! keeps the same failure surface as a value type: a [`LinearErrorKind`] (the sentinel identity,
//! matched with `matches!`) plus the wrapped `context` detail. `Display` reproduces the Go error
//! text (`"<kind>"` bare, or `"<kind>: <detail>"` wrapped), so the sentinel strings stay
//! byte-identical to the Go daemon's.
//!
//! This covers 8 of `errors.go`'s 9 sentinels. The 9th, `ErrLinearStateNotFound`, is the shared
//! by-type-move error both trackers return, so it lives at the contract level as
//! [`TrackerError::StateNotFound`](crate::TrackerError::StateNotFound) (added in T2) — not here.

use std::fmt;

/// One Linear adapter error category. Each variant mirrors a sentinel in `errors.go`; its
/// [`as_str`](LinearErrorKind::as_str) is byte-identical to the Go `errors.New` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinearErrorKind {
    /// `linear_api_request` — transport failure (request build / send / body read).
    ApiRequest,
    /// `linear_api_status` — non-200 HTTP response.
    ApiStatus,
    /// `linear_graphql_errors` — top-level GraphQL `errors` array present.
    GraphqlErrors,
    /// `linear_unknown_payload` — undecodable body / empty `data`.
    UnknownPayload,
    /// `linear_missing_end_cursor` — pagination integrity (a `hasNextPage` with no cursor).
    MissingCursor,
    /// `linear_move_rejected` — `issueUpdate` returned `success: false`.
    MoveRejected,
    /// `linear_milestone_not_found` — configured milestone name/id absent from the project.
    MilestoneNotFound,
    /// `linear_viewer_unresolved` — the `viewer` query returned no user id for the API key.
    ViewerUnresolved,
}

impl LinearErrorKind {
    /// The sentinel string, byte-identical to `errors.go`'s `errors.New(…)` message.
    pub const fn as_str(self) -> &'static str {
        match self {
            LinearErrorKind::ApiRequest => "linear_api_request",
            LinearErrorKind::ApiStatus => "linear_api_status",
            LinearErrorKind::GraphqlErrors => "linear_graphql_errors",
            LinearErrorKind::UnknownPayload => "linear_unknown_payload",
            LinearErrorKind::MissingCursor => "linear_missing_end_cursor",
            LinearErrorKind::MoveRejected => "linear_move_rejected",
            LinearErrorKind::MilestoneNotFound => "linear_milestone_not_found",
            LinearErrorKind::ViewerUnresolved => "linear_viewer_unresolved",
        }
    }
}

impl fmt::Display for LinearErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A Linear adapter error: a [`LinearErrorKind`] category plus the `fmt.Errorf` `context` detail
/// Go appends after the sentinel (empty when the sentinel is returned bare). Mirrors Go's wrapped
/// `fmt.Errorf("%w: …", ErrLinear…, …)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinearError {
    pub kind: LinearErrorKind,
    pub context: String,
}

impl LinearError {
    /// A wrapped error: `<kind>: <context>` (mirrors `fmt.Errorf("%w: …", sentinel, …)`).
    pub fn new(kind: LinearErrorKind, context: impl Into<String>) -> Self {
        Self {
            kind,
            context: context.into(),
        }
    }

    /// A bare sentinel with no detail (mirrors returning the `errors.New` value directly, e.g.
    /// `ErrLinearStateNotFound`).
    pub fn bare(kind: LinearErrorKind) -> Self {
        Self {
            kind,
            context: String::new(),
        }
    }
}

impl fmt::Display for LinearError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.context.is_empty() {
            write!(f, "{}", self.kind.as_str())
        } else {
            write!(f, "{}: {}", self.kind.as_str(), self.context)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The sentinel strings are byte-identical to internal/tracker/linear/errors.go's
    // errors.New(...) messages — a caller resolving these by text (logs, cross-language stub)
    // depends on the exact bytes.
    #[test]
    fn sentinel_strings_match_go() {
        assert_eq!(LinearErrorKind::ApiRequest.as_str(), "linear_api_request");
        assert_eq!(LinearErrorKind::ApiStatus.as_str(), "linear_api_status");
        assert_eq!(
            LinearErrorKind::GraphqlErrors.as_str(),
            "linear_graphql_errors"
        );
        assert_eq!(
            LinearErrorKind::UnknownPayload.as_str(),
            "linear_unknown_payload"
        );
        assert_eq!(
            LinearErrorKind::MissingCursor.as_str(),
            "linear_missing_end_cursor"
        );
        assert_eq!(
            LinearErrorKind::MoveRejected.as_str(),
            "linear_move_rejected"
        );
        assert_eq!(
            LinearErrorKind::MilestoneNotFound.as_str(),
            "linear_milestone_not_found"
        );
        assert_eq!(
            LinearErrorKind::ViewerUnresolved.as_str(),
            "linear_viewer_unresolved"
        );
    }

    // Display reproduces Go's error text: bare sentinel vs `fmt.Errorf("%w: …")` wrapping.
    #[test]
    fn display_bare_and_wrapped() {
        assert_eq!(
            LinearError::bare(LinearErrorKind::MoveRejected).to_string(),
            "linear_move_rejected"
        );
        assert_eq!(
            LinearError::new(LinearErrorKind::ApiStatus, "status 500: oops").to_string(),
            "linear_api_status: status 500: oops"
        );
    }
}
