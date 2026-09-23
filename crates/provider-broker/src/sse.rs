//! The bounded usage observer (design §7.1, §7.3).
//!
//! For a streaming response the adapter forwards the provider's SSE bytes unchanged (after
//! redaction); this observer watches them on the way past and extracts the final `usage` object the
//! child requested through `stream_options.include_usage`. Its line buffer is bounded: an oversized
//! or malformed event makes usage *unknown* rather than allocating without limit, and the broker may
//! then terminate the stream as a protocol error when forwarding it would violate the response-size
//! or redaction contract.
//!
//! For a non-streaming response, [`SseUsageObserver::observe_json`] reads the top-level `usage`
//! object directly (design §7.3).
//!
//! The parsed [`UsageObservation`] value lives in the always-compiled core ([`crate::usage`]); only
//! the parser is part of this `loopback`-gated adapter.
//!
//! Usage values must be finite non-negative integers; a present-but-invalid value makes the whole
//! observation unknown (design §7.3). The last `usage` object in the stream is judged as a whole —
//! fields from an earlier object never mix into a later one.
//!
//! This observer only *measures*. The ledger/budget slice (PB3) owns settlement; §7.3's rule that a
//! syntactically valid provider report can never re-open admission is why nothing here releases a
//! reservation.

use crate::usage::UsageObservation;

/// The maximum bytes held for one SSE line before it is treated as malformed (design §7.1's 64 KiB
/// maximum SSE event).
pub const MAX_SSE_LINE_BYTES: usize = 64 * 1024;

/// The maximum bytes of the raw `usage` member a non-streaming body may carry before the observer
/// treats it as malformed. A usage object is a handful of integers; refusing to parse anything
/// larger keeps a bounded body from amplifying into a much larger `serde_json::Value` tree (a 16 MiB
/// `{"usage":[0,0,…]}` body would otherwise peak at hundreds of MiB of RSS).
pub const MAX_USAGE_JSON_BYTES: usize = 4 * 1024;

/// The one field read from a non-streaming JSON response body. Deserializing into this typed
/// envelope extracts the raw `usage` member without copying the body and skips (and does not
/// allocate) every other choice/message field, so the parse itself cannot amplify a bounded body.
/// The raw member is only parsed into a map once it is within [`MAX_USAGE_JSON_BYTES`].
#[derive(serde::Deserialize)]
struct NonStreamingBody<'a> {
    #[serde(default, borrow)]
    usage: Option<&'a serde_json::value::RawValue>,
}

/// A bounded SSE line observer.
#[derive(Debug)]
pub struct SseUsageObserver {
    buffer: Vec<u8>,
    overflowed: bool,
    observation: UsageObservation,
}

impl Default for SseUsageObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl SseUsageObserver {
    /// A fresh observer.
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            overflowed: false,
            observation: UsageObservation::default(),
        }
    }

    /// Feed one upstream chunk. Complete `data:` lines are parsed immediately; a partial trailing
    /// line is retained. An oversized line discards the buffer and marks usage malformed.
    pub fn observe(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.buffer);
                self.observe_line(&line);
                continue;
            }
            if self.buffer.len() >= MAX_SSE_LINE_BYTES {
                // Drop the oversized line rather than growing the buffer; usage becomes unknown.
                self.buffer.clear();
                self.overflowed = true;
                self.observation.malformed_events =
                    self.observation.malformed_events.saturating_add(1);
                continue;
            }
            self.buffer.push(byte);
        }
    }

    /// End the stream, parsing any final line that lacked a trailing newline.
    pub fn finish(&mut self) {
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            self.observe_line(&line);
        }
    }

    /// The observation so far.
    pub fn observation(&self) -> UsageObservation {
        self.observation
    }

    /// Whether the observer ever overflowed a line.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Parse a non-streaming response body once: return whether it is a JSON object at all, and read
    /// its top-level `usage` object (design §7.3). The typed envelope borrows the raw `usage` member
    /// and skips every unknown field without building a `serde_json::Value` tree; the member is
    /// parsed only within [`MAX_USAGE_JSON_BYTES`], so a bounded body cannot amplify into far more
    /// memory than its byte budget charges (a 16 MiB `{"usage":[0,0,...]}` body would otherwise
    /// allocate hundreds of MiB as a `Value`). The top-level object check is explicit, because serde
    /// would otherwise accept a top-level array as a struct.
    ///
    /// A body that is not a JSON object returns `false` (a successful non-streaming response must be
    /// a JSON object); a valid object whose `usage` is present but not an object still returns
    /// `true`, marking only the usage observation malformed. `"usage": null` means "no usage", not
    /// malformed.
    pub fn observe_json(&mut self, body: &[u8]) -> bool {
        // A successful non-streaming response must be a JSON *object*. The derived struct would also
        // accept a top-level array (serde treats a sequence as a struct), so the object-only check
        // is explicit: an array must not reach the child as a 200.
        if trim_ascii_whitespace(body).first() != Some(&b'{') {
            self.mark_malformed();
            return false;
        }
        let parsed: NonStreamingBody = match serde_json::from_slice(body) {
            Ok(parsed) => parsed,
            Err(_) => {
                self.mark_malformed();
                return false;
            }
        };
        let Some(usage) = parsed.usage else {
            return true;
        };
        let usage = usage.get();
        if usage.trim() == "null" {
            return true;
        }
        // The raw member is borrowed from the bounded body (no copy). Only a member within the cap
        // is parsed into a tree, so an adversarial body cannot amplify its byte budget.
        if usage.len() > MAX_USAGE_JSON_BYTES {
            self.mark_malformed();
            return true;
        }
        let usage: serde_json::Map<String, serde_json::Value> = match serde_json::from_str(usage) {
            Ok(usage) => usage,
            Err(_) => {
                self.mark_malformed();
                return true;
            }
        };
        self.apply_usage(&usage);
        true
    }

    fn mark_malformed(&mut self) {
        self.observation.malformed_events = self.observation.malformed_events.saturating_add(1);
    }

    fn observe_line(&mut self, line: &[u8]) {
        let line = strip_carriage_return(line);
        let Some(payload) = line.strip_prefix(b"data:") else {
            // Comments (`:`), `event:`, `id:`, blank keep-alive lines carry no usage.
            return;
        };
        let payload = trim_ascii_whitespace(payload);
        if payload.is_empty() || payload == b"[DONE]" {
            return;
        }
        let value: serde_json::Value = match serde_json::from_slice(payload) {
            Ok(value) => value,
            Err(_) => {
                self.mark_malformed();
                return;
            }
        };
        let Some(usage) = value.get("usage") else {
            return;
        };
        if usage.is_null() {
            return;
        }
        match usage.as_object() {
            Some(usage) => self.apply_usage(usage),
            None => self.mark_malformed(),
        }
    }

    fn apply_usage(&mut self, usage: &serde_json::Map<String, serde_json::Value>) {
        let input = field_u64(usage, "prompt_tokens");
        let output = field_u64(usage, "completion_tokens");
        let total = field_u64(usage, "total_tokens");
        let cached = match usage.get("prompt_tokens_details") {
            None => Parsed::Absent,
            Some(details) => match details.as_object() {
                Some(details) => field_u64(details, "cached_tokens"),
                None => Parsed::Invalid,
            },
        };

        // A present-but-invalid value makes the whole observation unknown and keeps the full
        // reservation (design §7.3).
        if let Parsed::Invalid = input {
            self.invalidate_usage();
            return;
        }
        if let Parsed::Invalid = output {
            self.invalidate_usage();
            return;
        }
        if let Parsed::Invalid = total {
            self.invalidate_usage();
            return;
        }
        if let Parsed::Invalid = cached {
            self.invalidate_usage();
            return;
        }

        // The last usage object is judged as a whole: replace every field rather than merging
        // stale values from an earlier object.
        self.observation.input_tokens = input.value();
        self.observation.output_tokens = output.value();
        self.observation.total_tokens = total.value();
        self.observation.cached_tokens = cached.value();
        self.observation.complete = input.is_valid() && output.is_valid() && total.is_valid();
        self.observation.inconsistent = match (total, input, output) {
            (Parsed::Valid(total), Parsed::Valid(input), Parsed::Valid(output)) => {
                match input.checked_add(output) {
                    Some(sum) => total != sum,
                    None => true,
                }
            }
            _ => false,
        };
    }

    /// Mark the current usage observation as unknown and clear its values.
    fn invalidate_usage(&mut self) {
        let malformed_events = self.observation.malformed_events.saturating_add(1);
        self.observation = UsageObservation {
            malformed_events,
            ..UsageObservation::default()
        };
    }
}

/// A usage field's parse outcome: absent, a valid non-negative integer, or present but invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parsed {
    Absent,
    Valid(u64),
    Invalid,
}

impl Parsed {
    fn is_valid(self) -> bool {
        matches!(self, Parsed::Valid(_))
    }

    fn value(self) -> Option<u64> {
        match self {
            Parsed::Valid(value) => Some(value),
            _ => None,
        }
    }
}

/// A usage field: absent, a valid non-negative integer, or present-but-invalid (design §7.3).
fn field_u64(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> Parsed {
    match map.get(key) {
        None => Parsed::Absent,
        Some(value) => match value.as_u64() {
            Some(value) => Parsed::Valid(value),
            None => Parsed::Invalid,
        },
    }
}

fn strip_carriage_return(line: &[u8]) -> &[u8] {
    match line.last() {
        Some(b'\r') => &line[..line.len() - 1],
        _ => line,
    }
}

fn trim_ascii_whitespace(mut bytes: &[u8]) -> &[u8] {
    while let Some((first, rest)) = bytes.split_first() {
        if first.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    while let Some((last, rest)) = bytes.split_last() {
        if last.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_final_usage_across_chunks() {
        let mut observer = SseUsageObserver::new();
        observer.observe(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        observer.observe(b"data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4,");
        observer
            .observe(b"\"total_tokens\":14,\"prompt_tokens_details\":{\"cached_tokens\":3}}}\n\n");
        observer.observe(b"data: [DONE]\n\n");
        observer.finish();
        let observation = observer.observation();
        assert!(observation.complete);
        assert_eq!(observation.input_tokens, Some(10));
        assert_eq!(observation.output_tokens, Some(4));
        assert_eq!(observation.cached_tokens, Some(3));
        assert_eq!(observation.total_tokens, Some(14));
        assert!(!observation.inconsistent);
        assert_eq!(observation.conservative_total(), Some(14));
        assert!(!observation.is_unknown());
    }

    #[test]
    fn malformed_event_makes_usage_unknown() {
        let mut observer = SseUsageObserver::new();
        observer.observe(b"data: {not json}\n");
        observer.finish();
        assert!(!observer.observation().complete);
        assert!(observer.observation().is_unknown());
        assert_eq!(observer.observation().malformed_events, 1);
    }

    #[test]
    fn oversized_line_is_discarded_not_allocated() {
        let mut observer = SseUsageObserver::new();
        observer.observe(&vec![b'x'; MAX_SSE_LINE_BYTES + 100]);
        assert!(observer.overflowed());
        assert!(observer.observation().is_unknown());
    }

    #[test]
    fn negative_or_fractional_usage_is_unknown() {
        let mut observer = SseUsageObserver::new();
        observer.observe(b"data: {\"usage\":{\"prompt_tokens\":-1,\"completion_tokens\":2.5,\"total_tokens\":1}}\n");
        observer.finish();
        let observation = observer.observation();
        assert!(!observation.complete);
        assert!(observation.is_unknown());
    }

    #[test]
    fn invalid_values_in_a_later_usage_object_make_the_whole_observation_unknown() {
        let mut observer = SseUsageObserver::new();
        observer.observe(
            b"data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":1,\"total_tokens\":11}}\n\n",
        );
        observer.observe(
            b"data: {\"usage\":{\"prompt_tokens\":-10,\"completion_tokens\":18446744073709551616,\"total_tokens\":1e30}}\n\n",
        );
        observer.finish();
        let observation = observer.observation();
        assert!(
            observation.is_unknown(),
            "negative/overflowing usage must be unknown, got {observation:?}"
        );
        assert!(!observation.complete);
        assert_eq!(observation.input_tokens, None);
        assert_eq!(observation.output_tokens, None);
        assert_eq!(observation.total_tokens, None);
        assert_eq!(observation.conservative_total(), None);
        assert!(observation.malformed_events >= 1);
    }

    #[test]
    fn the_last_usage_object_is_judged_as_a_whole() {
        let mut observer = SseUsageObserver::new();
        observer.observe(
            b"data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":1,\"total_tokens\":11}}\n\n",
        );
        observer.observe(b"data: {\"usage\":{\"prompt_tokens\":4,\"total_tokens\":4}}\n\n");
        observer.finish();
        let observation = observer.observation();
        // The earlier completion count must not survive next to the later input/total.
        assert_eq!(observation.input_tokens, Some(4));
        assert_eq!(observation.output_tokens, None);
        assert_eq!(observation.total_tokens, Some(4));
        assert!(!observation.complete);
    }

    #[test]
    fn reads_json_usage_from_a_non_streaming_body() {
        let mut observer = SseUsageObserver::new();
        observer.observe_json(
            br#"{"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}"#,
        );
        let observation = observer.observation();
        assert!(observation.complete);
        assert_eq!(observation.conservative_total(), Some(10));
        assert!(!observation.is_unknown());

        // A null usage object means "no usage", not malformed (usage is still unknown, but no
        // malformed event is recorded).
        let mut observer = SseUsageObserver::new();
        observer.observe_json(br#"{"choices":[],"usage":null}"#);
        assert_eq!(observer.observation().malformed_events, 0);
        assert!(!observer.observation().complete);

        // A usage value that is not an object is malformed.
        let mut observer = SseUsageObserver::new();
        observer.observe_json(br#"{"choices":[],"usage":5}"#);
        assert!(observer.observation().malformed_events >= 1);
    }

    #[test]
    fn a_non_streaming_top_level_array_is_not_a_json_object() {
        // serde would deserialize a top-level sequence into a struct, so an empty array (and any
        // other array) must be refused explicitly: it is not a JSON object and must not reach the
        // child as a 200.
        for body in [&b"[]"[..], &b"[null]"[..], &br#"[{"prompt_tokens":1}]"#[..]] {
            let mut observer = SseUsageObserver::new();
            assert!(
                !observer.observe_json(body),
                "a top-level array must not be observed as a JSON object: {}",
                String::from_utf8_lossy(body)
            );
            assert!(observer.observation().malformed_events >= 1);
        }
    }

    #[test]
    fn an_oversized_usage_member_is_malformed_without_being_parsed() {
        // A syntactically valid usage object (with a complete set of token fields) that nonetheless
        // exceeds the cap. Without the cap this parses and the observation is complete; with it the
        // member is refused as malformed and no value is read from it.
        let mut usage = String::from("{");
        for i in 0..2000 {
            usage.push_str(&format!("\"k{i}\":0,"));
        }
        usage.push_str("\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}");
        assert!(usage.len() > MAX_USAGE_JSON_BYTES);
        let body = format!("{{\"choices\":[],\"usage\":{usage}}}");

        let mut observer = SseUsageObserver::new();
        assert!(observer.observe_json(body.as_bytes()));
        let observation = observer.observation();
        assert!(!observation.complete, "oversized usage must not be parsed");
        assert_eq!(observation.input_tokens, None);
        assert!(observation.malformed_events >= 1);
    }

    #[test]
    fn disagreement_uses_the_larger_conservative_total() {
        let mut observer = SseUsageObserver::new();
        observer.observe(b"data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4,\"total_tokens\":99}}\n");
        observer.finish();
        let observation = observer.observation();
        assert!(observation.inconsistent);
        assert_eq!(observation.conservative_total(), Some(99));

        let mut observer = SseUsageObserver::new();
        observer.observe(b"data: {\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":4,\"total_tokens\":50}}\n");
        observer.finish();
        assert_eq!(observer.observation().conservative_total(), Some(104));
    }

    #[test]
    fn non_usage_events_are_ignored() {
        let mut observer = SseUsageObserver::new();
        observer.observe(b": keep-alive\n\nevent: ping\ndata: {\"choices\":[]}\n\n");
        observer.finish();
        assert_eq!(observer.observation().malformed_events, 0);
        assert!(!observer.observation().complete);
    }
}
