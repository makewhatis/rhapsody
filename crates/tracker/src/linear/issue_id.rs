//! Linear's issue-id input contract. Local run keys must never reach GraphQL validation.

/// A canonical UUID or an ASCII team key followed by an issue number (case-insensitive).
pub(super) fn valid(id: &str) -> bool {
    let uuid = id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        });
    let identifier = id.split_once('-').is_some_and(|(team, number)| {
        !team.is_empty()
            && team.bytes().all(|b| b.is_ascii_alphanumeric())
            && !number.is_empty()
            && number.bytes().all(|b| b.is_ascii_digit())
    });
    let valid = uuid || identifier;
    if !valid {
        tracing::debug!(id, "dropping invalid Linear issue id");
    }
    valid
}

/// Mutations have no not-found success: refuse locally using the adapter's existing error kind.
pub(super) fn require(id: &str) -> Result<(), crate::TrackerError> {
    if valid(id) {
        Ok(())
    } else {
        Err(super::LinearError::new(super::LinearErrorKind::ApiRequest, "invalid issue id").into())
    }
}
