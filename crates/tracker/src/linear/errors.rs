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
//!
//! One variant, [`LinearErrorKind::DuplicateAttachment`], mirrors no `errors.go` sentinel and never
//! can: it classifies a refusal of `attachmentLinkGitHubPR`, an operation Symphony has no
//! counterpart for (STUDIO-875, STUDIO-904). It is additive, and unlike every other kind it is not
//! a failure at all — [`attach::link_pull_request`](super::attach::link_pull_request) consumes it as
//! a SUCCESS, because the link it wanted to write is the one already there.

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
    /// `linear_duplicate_attachment` — `attachmentLinkGitHubPR` was refused because this issue
    /// already links this pull request. **Rhapsody-only, and not a failure**: the post-condition
    /// the caller cares about is "the ticket links this pull request", and a duplicate refusal
    /// proves it. See [`classify_graphql_errors`].
    DuplicateAttachment,
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
            LinearErrorKind::DuplicateAttachment => "linear_duplicate_attachment",
        }
    }
}

/// The top-level `message` Linear answers a duplicate `attachmentLinkGitHubPR` with — its
/// developer-facing error IDENTITY, distinct from the `userPresentableMessage` it shows a person.
const DUPLICATE_ATTACHMENT_MESSAGE: &str = "Duplicate attachment for duplicate url";

/// The [`LinearErrorKind`] a top-level GraphQL `errors` array belongs to.
///
/// Almost every such array is [`GraphqlErrors`](LinearErrorKind::GraphqlErrors) — an unclassified
/// failure. The one refusal Rhapsody can name is Linear refusing `attachmentLinkGitHubPR` because
/// the same pull request is already attached to the issue, and that refusal is the answer the
/// caller WANTS: it proves the link it tried to write is present.
///
/// Three machine-readable coordinates are matched, never the prose. `extensions.code` is Linear's
/// service-layer category (`INPUT_ERROR`), the error's `path` names the mutation that was refused
/// (`attachmentLinkGitHubPR`), and the top-level `message` is the error's identity. The `message`
/// is what separates the duplicate from the rest of the `INPUT_ERROR` bucket: that code is Linear's
/// general user-error category for this mutation, not a synonym for "duplicate", so code and path
/// alone would swallow a refusal that never linked anything (a URL Linear rejects as not a pull
/// request, say). `userPresentableMessage` — the English sentence shown in Linear's UI — is
/// deliberately NOT consulted, because it is presentation text Linear rewrites freely and a
/// classifier keyed on it would stop working the day the wording changed. This is the same reason
/// `prlink`'s gate keys on the resolved pull-request number rather than on the tracker's `merged`
/// flag. If Linear ever rewords `message`, this degrades to a warning on a duplicate — noisy but
/// honest — rather than silently swallowing a real failure.
///
/// One `errors` array classifies as a unit: if any entry is the duplicate, the array is the
/// duplicate. A single-mutation request carries one error, so a mixed array is not reachable today.
pub(crate) fn classify_graphql_errors(errors: &[serde_json::Value]) -> LinearErrorKind {
    if errors.iter().any(is_duplicate_attachment) {
        LinearErrorKind::DuplicateAttachment
    } else {
        LinearErrorKind::GraphqlErrors
    }
}

/// Whether one GraphQL error entry is Linear refusing a duplicate pull-request attachment.
fn is_duplicate_attachment(error: &serde_json::Value) -> bool {
    let code = error
        .pointer("/extensions/code")
        .and_then(serde_json::Value::as_str);
    if code != Some("INPUT_ERROR") {
        return false;
    }
    let message = error.get("message").and_then(serde_json::Value::as_str);
    if message != Some(DUPLICATE_ATTACHMENT_MESSAGE) {
        return false;
    }
    error
        .get("path")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|path| {
            path.iter()
                .any(|segment| segment.as_str() == Some("attachmentLinkGitHubPR"))
        })
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
        assert_eq!(
            LinearErrorKind::DuplicateAttachment.as_str(),
            "linear_duplicate_attachment"
        );
    }

    /// The payload is the one the live tracker answered STUDIO-902's second run with, verbatim
    /// (`~/.rhapsody/logs/rhapsodyd.2026-09-15.log`, 05:33). Its shape — `INPUT_ERROR` on the
    /// `attachmentLinkGitHubPR` path **and** this top-level `message` — is what the classifier keys
    /// on.
    const DUPLICATE_ATTACHMENT_ERRORS: &str = r#"[
      {
        "extensions": {
          "code": "INPUT_ERROR",
          "statusCode": 400,
          "type": "invalid input",
          "userError": true,
          "userPresentableMessage": "An attachment with the same URL already exists."
        },
        "locations": [{"column": 3, "line": 3}],
        "message": "Duplicate attachment for duplicate url",
        "path": ["attachmentLinkGitHubPR"]
      }
    ]"#;

    fn errors(json: &str) -> Vec<serde_json::Value> {
        serde_json::from_str(json).expect("errors payload")
    }

    #[test]
    fn a_duplicate_attachment_is_classified_and_an_ordinary_error_is_not() {
        assert_eq!(
            classify_graphql_errors(&errors(DUPLICATE_ATTACHMENT_ERRORS)),
            LinearErrorKind::DuplicateAttachment
        );
        assert_eq!(
            classify_graphql_errors(&errors(r#"[{"message":"bad query"}]"#)),
            LinearErrorKind::GraphqlErrors
        );
    }

    /// The classifier reads the SHAPE, so rewording the sentence Linear shows a person changes
    /// nothing — the mutation that drops this and returns `GraphqlErrors` reds here.
    #[test]
    fn the_wording_of_the_presentable_message_is_not_consulted() {
        let reworded: Vec<serde_json::Value> = errors(DUPLICATE_ATTACHMENT_ERRORS)
            .into_iter()
            .map(|mut e| {
                e["extensions"]["userPresentableMessage"] =
                    serde_json::json!("That pull request is already linked.");
                e
            })
            .collect();
        assert_eq!(
            classify_graphql_errors(&reworded),
            LinearErrorKind::DuplicateAttachment
        );
    }

    /// And the shape is all THREE halves: `INPUT_ERROR` on a different path, the right path with a
    /// different code, or the duplicate's message absent, is an ordinary failure — swallowing any
    /// of them would silence a real refusal.
    #[test]
    fn a_refusal_that_only_resembles_a_duplicate_is_a_failure() {
        assert_eq!(
            classify_graphql_errors(&errors(
                r#"[{"extensions":{"code":"INPUT_ERROR"},"message":"Duplicate attachment for duplicate url","path":["issueUpdate"]}]"#
            )),
            LinearErrorKind::GraphqlErrors,
            "the path must name the attachment mutation"
        );
        assert_eq!(
            classify_graphql_errors(&errors(
                r#"[{"extensions":{"code":"INTERNAL_SERVER_ERROR"},"message":"Duplicate attachment for duplicate url","path":["attachmentLinkGitHubPR"]}]"#
            )),
            LinearErrorKind::GraphqlErrors,
            "the code must be INPUT_ERROR"
        );
        assert_eq!(
            classify_graphql_errors(&errors(
                r#"[{"message":"Duplicate attachment for duplicate url","path":["attachmentLinkGitHubPR"]}]"#
            )),
            LinearErrorKind::GraphqlErrors,
            "a missing code is not INPUT_ERROR"
        );
    }

    /// The pin the STUDIO-904 review asked for: `INPUT_ERROR` is Linear's general user-error
    /// bucket for this mutation, not a synonym for "duplicate". A different refusal of the SAME
    /// mutation (the same code, the same path — only the error identity differs) must stay an
    /// error, or the daemon reports a link for a write that never landed. This is the one axis the
    /// code-and-path classifier was blind on.
    #[test]
    fn a_non_duplicate_input_error_on_the_same_path_is_still_an_error() {
        let payload = r#"[{
            "extensions": {
              "code": "INPUT_ERROR",
              "statusCode": 400,
              "type": "invalid input",
              "userError": true,
              "userPresentableMessage": "That is not a valid GitHub pull request URL."
            },
            "message": "Invalid pull request url",
            "path": ["attachmentLinkGitHubPR"]
        }]"#;
        assert_eq!(
            classify_graphql_errors(&errors(payload)),
            LinearErrorKind::GraphqlErrors,
            "a non-duplicate user error on the attachment path is a failure, not a link"
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
