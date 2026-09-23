//! The parsed provider usage observation (design §7.3).
//!
//! This is the plain, non-secret value the usage observer extracts from a provider response and that
//! the ledger settles. It lives in the always-compiled core (not the `loopback`-gated adapter)
//! because the reservation and settlement paths own it: a PB1-only build (`default-features = false`,
//! as `rhapsody-credential-ipc` uses) still settles usage and must not pull the HTTP stack to name
//! the type. The SSE/JSON *parser* that produces it ([`SseUsageObserver`](crate::sse::SseUsageObserver))
//! stays in the PB2 adapter behind the `loopback` feature.
//!
//! Usage values must be finite non-negative integers; a present-but-invalid value makes the whole
//! observation unknown (design §7.3).

/// The provider-reported usage observed from a response. Every field is optional and only set when
/// the provider reported a syntactically valid non-negative integer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageObservation {
    /// Provider-reported input (prompt) tokens.
    pub input_tokens: Option<u64>,
    /// Provider-reported output (completion) tokens.
    pub output_tokens: Option<u64>,
    /// Provider-reported cached input tokens, when the provider reports them.
    pub cached_tokens: Option<u64>,
    /// Provider-reported total tokens.
    pub total_tokens: Option<u64>,
    /// A syntactically valid usage object with input, output and total was observed.
    pub complete: bool,
    /// The reported total disagreed with the component counts (a bounded diagnostic; settlement uses
    /// the larger conservative value).
    pub inconsistent: bool,
    /// Count of events that were malformed or oversized. Non-zero makes usage unknown.
    pub malformed_events: u64,
}

impl UsageObservation {
    /// The conservative token count for settlement: the larger of the reported total and the sum of
    /// the reported components. `None` when nothing usable was reported.
    pub fn conservative_total(&self) -> Option<u64> {
        let components = match (self.input_tokens, self.output_tokens) {
            (Some(input), Some(output)) => input.checked_add(output),
            _ => None,
        };
        match (self.total_tokens, components) {
            (Some(total), Some(sum)) => Some(total.max(sum)),
            (Some(total), None) => Some(total),
            (None, sum) => sum,
        }
    }

    /// Whether the provider never supplied usable usage: every request is conservatively unknown.
    pub fn is_unknown(&self) -> bool {
        !self.complete || self.malformed_events > 0
    }
}
