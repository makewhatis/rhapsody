//! opencode JSONL line parsing (STUDIO-902). Rhapsody-only — no Go counterpart.
//!
//! [`classify`] maps one `opencode run --format json` line to the same normalized [`crate::Event`]
//! vocabulary the claude backend produces. Built against the committed spike captures in
//! `harness/harness-spike/opencode/`, which are the measured record of this CLI's stream (design
//! record `~/.rhapsody/docs/STUDIO-869-harness-spike-findings.md` §4, `[RAN]`).
//!
//! Three shapes make this parser structurally unlike `crate::claude::parse`, all measured:
//!
//! * **There is no terminal result event.** The stream ends after a `step_finish` whose
//!   `part.reason == "stop"`, and the process exits 0. `step_finish{reason:"stop"}` + EOF is the
//!   terminal condition; nothing analogous to claude's `result` line ever arrives.
//! * **Usage is per STEP and must be summed.** Each `step_finish` reports the tokens for its own
//!   API call (the `input` count RESETS every step), so a turn total is the sum across steps, not
//!   the last step's figures. [`Classified::step_usage`] is deliberately named for that: the runner
//!   accumulates it. Reading the last `step_finish` as the turn total under-reports a 4-step turn
//!   by roughly a factor of four.
//! * **There is no `system/init`.** Every line carries `sessionID`, but no line announces the
//!   session, so the SESSION_STARTED event is synthesized by the runner on the first id it sees
//!   (design §6.1's "`SessionEstablished`, emitted once per process").

use serde::Deserialize;

use crate::{EVENT_NOTIFICATION, EVENT_TURN_FAILED, Event, Usage};
use chrono::Utc;

/// Bounds the assistant text surfaced on [`crate::Event::message`]. Matches the claude parser's
/// cap so the two backends put comparably-sized payloads on the same normalized event.
const MAX_MESSAGE_LEN: usize = 2048;

/// One classified opencode JSONL line.
///
/// `session_id` is surfaced on EVERY line that carries one, including lines that are not surfaced
/// as events (`ok == false`), for the same reason the claude parser does it: the runner seeds its
/// resume id from whichever line arrives first, and opencode has no dedicated announcement line to
/// wait for.
#[derive(Debug, Clone, Default)]
pub struct Classified {
    pub event: Event,
    pub session_id: String,
    /// `step_finish` with `part.reason == "stop"` — the ONLY terminal condition (see the module
    /// doc). Intermediate steps carry `reason: "tool-calls"`.
    pub terminal: bool,
    /// `false` for blank lines, non-JSON, and line types we don't surface as events.
    pub ok: bool,
    /// This step's usage, from a `step_finish`. The runner SUMS these across the turn; it is not a
    /// running total. `None` on every other line type.
    pub step_usage: Option<Usage>,
    /// The prose of a `text` line. The runner keeps the LAST one as the turn's `result_text` (the
    /// `HANDOFF:` marker the orchestrator looks for is in the agent's final message).
    pub text: String,
    /// An in-band `error` line. Deliberately does NOT set `terminal`: the measured 401 capture is a
    /// single `error` line that also ends the stream, so treating it as terminal and treating it as
    /// sticky-then-EOF are indistinguishable on the evidence — and only the sticky reading stays
    /// correct if opencode ever emits a non-terminal error the way codex does (design §7.1, where
    /// treating codex's non-terminal `error` as terminal "aborts 100% of turns").
    pub failure: Option<Failure>,
}

/// An in-band `error` event. opencode is the only measured harness that hands the daemon an HTTP
/// `statusCode` and an `isRetryable` boolean directly, so the retry classifier can use them rather
/// than re-deriving retryability from message text (design §7.1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Failure {
    pub name: String,
    pub message: String,
    pub status_code: i64,
    pub retryable: bool,
}

impl Failure {
    /// The one-line rendering used for the failed turn's error text.
    pub fn summary(&self) -> String {
        let name = if self.name.is_empty() {
            "error"
        } else {
            self.name.as_str()
        };
        let mut s = format!("{name}: {}", self.message);
        if self.status_code != 0 {
            s.push_str(&format!(" (status {}", self.status_code));
            s.push_str(if self.retryable { ", retryable)" } else { ")" });
        }
        s
    }
}

/// A lenient view of one opencode JSONL line. Every field is optional via the container-level
/// `#[serde(default)]`, so an absent field contributes its zero value rather than failing the
/// decode — the same defensive posture the claude parser takes.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawLine {
    #[serde(rename = "type")]
    r#type: String,
    #[serde(rename = "sessionID")]
    session_id: String,
    part: Option<RawPart>,
    error: Option<RawError>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawPart {
    /// `step_finish` only: `"stop"` terminates the turn, `"tool-calls"` does not.
    reason: String,
    /// `text` only.
    text: String,
    /// `tool_use` only: the tool's name in opencode's own spelling (`symphony_symphony_state`).
    tool: String,
    tokens: Option<RawTokens>,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(default)]
struct RawTokens {
    /// opencode's own per-step total. Not read as the turn total (see the module doc); the
    /// `usage_total_matches_opencodes_own_arithmetic` test uses it to PROVE the field mapping in
    /// [`usage_from_tokens`] reproduces the harness's own sum.
    total: i64,
    input: i64,
    output: i64,
    reasoning: i64,
    cache: RawCache,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(default)]
struct RawCache {
    write: i64,
    read: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawError {
    name: String,
    data: RawErrorData,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawErrorData {
    message: String,
    #[serde(rename = "statusCode")]
    status_code: i64,
    #[serde(rename = "isRetryable")]
    is_retryable: bool,
}

/// Maps one opencode JSONL line to a normalized event.
pub fn classify(line: &[u8]) -> Classified {
    if String::from_utf8_lossy(line).trim().is_empty() {
        return Classified::default();
    }
    let r: RawLine = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(_) => return Classified::default(),
    };
    let now = Some(Utc::now());
    let part = r.part.unwrap_or_default();

    match r.r#type.as_str() {
        "text" => {
            let text = truncate(part.text.trim(), MAX_MESSAGE_LEN);
            Classified {
                event: Event {
                    event_type: EVENT_NOTIFICATION.to_string(),
                    timestamp: now,
                    message: text.clone(),
                    ..Default::default()
                },
                session_id: r.session_id,
                ok: !text.is_empty(),
                text: part.text,
                ..Default::default()
            }
        }
        "step_finish" => {
            let usage = part.tokens.map(|t| usage_from_tokens(&t));
            Classified {
                // A step boundary is surfaced as a usage-bearing notification, the mirror of the
                // claude parser surfacing each assistant message's own `message.usage` as a live
                // in-turn estimate. The authoritative per-turn total is the runner's SUM.
                event: Event {
                    event_type: EVENT_NOTIFICATION.to_string(),
                    timestamp: now,
                    usage,
                    ..Default::default()
                },
                session_id: r.session_id,
                terminal: part.reason == "stop",
                ok: true,
                step_usage: usage,
                ..Default::default()
            }
        }
        "error" => {
            let e = r.error.unwrap_or_default();
            let failure = Failure {
                name: e.name,
                message: e.data.message,
                status_code: e.data.status_code,
                retryable: e.data.is_retryable,
            };
            Classified {
                event: Event {
                    event_type: EVENT_TURN_FAILED.to_string(),
                    timestamp: now,
                    message: failure.summary(),
                    ..Default::default()
                },
                session_id: r.session_id,
                ok: true,
                failure: Some(failure),
                ..Default::default()
            }
        }
        // `step_start` and `tool_use` are not surfaced as normalized events: the claude parser
        // likewise surfaces only assistant PROSE and the terminal result, leaving tool-level detail
        // to the transcript humanizer (`crate::humanize`), which reads the raw lines. Their session
        // id still propagates — on the measured captures `step_start` is the FIRST line of every
        // stream, so dropping it here would delay the resume id by a whole step.
        _ => Classified {
            session_id: r.session_id,
            ..Default::default()
        },
    }
}

/// Maps opencode's per-step token block onto the normalized [`Usage`].
///
/// `reasoning` is folded into `output_tokens` because reasoning tokens are billed output and
/// [`Usage`] has no separate field for them. That is not a guess: with this folding the derived
/// `total_tokens` (uncached input + output + cache-write + cache-read, the invariant `Usage`'s own
/// doc comment states) reproduces opencode's own reported `tokens.total` exactly on every
/// `step_finish` in the committed captures — which is what
/// `usage_total_matches_opencodes_own_arithmetic` asserts. Dropping `reasoning` instead would
/// silently under-report a reasoning-heavy turn.
fn usage_from_tokens(t: &RawTokens) -> Usage {
    let output = t.output + t.reasoning;
    Usage {
        input_tokens: t.input,
        output_tokens: output,
        cache_creation_tokens: t.cache.write,
        cache_read_tokens: t.cache.read,
        total_tokens: t.input + output + t.cache.write + t.cache.read,
    }
}

/// Adds one step's usage into a running turn total. Every field sums, including `total_tokens`:
/// each step is a separate billed API call (see the module doc).
pub fn add_usage(acc: &mut Usage, step: &Usage) {
    acc.input_tokens += step.input_tokens;
    acc.output_tokens += step.output_tokens;
    acc.cache_creation_tokens += step.cache_creation_tokens;
    acc.cache_read_tokens += step.cache_read_tokens;
    acc.total_tokens += step.total_tokens;
}

/// Truncates to `max` BYTES on a char boundary, keeping the head (mirrors the claude parser's
/// notification clamp).
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EVENT_NOTIFICATION, EVENT_TURN_FAILED};

    /// The committed spike captures (`harness/harness-spike/opencode/`) are this parser's
    /// acceptance map: they are real `opencode run --format json` output against a real provider,
    /// with the exact command that produced each recorded in that directory's README. Asserting
    /// against them rather than against hand-written lines is what keeps this parser honest about
    /// the CLI's actual stream — a hand-written line can only ever confirm what the author already
    /// believed. They are NOT `harness/fixtures/` goldens and are not normalized (that directory is
    /// exclusively Go-reference output, and `make fixtures` deletes anything else in it).
    fn capture(name: &str) -> Vec<u8> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../harness/harness-spike/opencode")
            .join(name);
        std::fs::read(&p).unwrap_or_else(|e| panic!("read capture {}: {e}", p.display()))
    }

    fn lines(name: &str) -> Vec<Vec<u8>> {
        capture(name)
            .split(|&b| b == b'\n')
            .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
            .map(<[u8]>::to_vec)
            .collect()
    }

    // The happy capture end to end: one session id throughout, exactly one terminal line, and it is
    // the LAST line. This is the shape the runner's loop depends on — opencode has no result event,
    // so "the stream ended" and "the turn finished" are only the same thing when the terminal
    // `step_finish{reason:"stop"}` really does arrive last.
    #[test]
    fn happy_capture_ends_with_exactly_one_terminal_stop() {
        let ls = lines("happy.jsonl");
        assert_eq!(ls.len(), 15, "the committed capture is 15 lines");

        let cs: Vec<Classified> = ls.iter().map(|l| classify(l)).collect();

        let ids: std::collections::BTreeSet<&str> = cs
            .iter()
            .map(|c| c.session_id.as_str())
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(
            ids.len(),
            1,
            "every line of one turn carries the same session id, got {ids:?}"
        );
        assert!(ids.contains("ses_f6cfb07faffeOeHel6dw2uoYgm"));
        // The id must be available from the FIRST line (a `step_start`, which is not surfaced as an
        // event): the runner seeds its resume id from it.
        assert_eq!(cs[0].session_id, "ses_f6cfb07faffeOeHel6dw2uoYgm");
        assert!(!cs[0].ok, "step_start is not surfaced as an event");

        let terminals: Vec<usize> = cs
            .iter()
            .enumerate()
            .filter(|(_, c)| c.terminal)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            terminals,
            vec![14],
            "exactly one terminal line, and it is the last"
        );
        assert!(cs.iter().all(|c| c.failure.is_none()));
    }

    // Four `step_finish` lines, only the last of which is terminal. The three intermediate ones
    // carry `reason: "tool-calls"`; a parser that keyed terminality on "a step finished" rather
    // than on the reason would end the turn after the first tool call.
    #[test]
    fn intermediate_steps_are_not_terminal() {
        let cs: Vec<Classified> = lines("happy.jsonl").iter().map(|l| classify(l)).collect();
        let steps: Vec<&Classified> = cs.iter().filter(|c| c.step_usage.is_some()).collect();
        assert_eq!(steps.len(), 4, "the capture has 4 step_finish lines");
        assert_eq!(
            steps.iter().filter(|c| c.terminal).count(),
            1,
            "only the `reason: \"stop\"` step is terminal"
        );
        assert!(!steps[0].terminal && !steps[1].terminal && !steps[2].terminal);
        assert!(steps[3].terminal);
    }

    // THE field-mapping proof. `usage_from_tokens` folds `reasoning` into `output_tokens` so that
    // the derived `total_tokens` — the invariant `Usage`'s doc comment states, uncached input +
    // output + cache-write + cache-read — equals opencode's OWN reported `tokens.total`. Checking
    // the mapping against the harness's own arithmetic, on every step of every committed capture,
    // is the difference between a measured mapping and a plausible one: dropping `reasoning`
    // instead still produces a total, just a wrong one, and nothing else in the pipeline would
    // notice.
    #[test]
    fn usage_total_matches_opencodes_own_arithmetic() {
        let mut checked = 0;
        for name in ["happy.jsonl", "long-turn.jsonl", "resume.jsonl"] {
            for raw in lines(name) {
                let v: serde_json::Value = match serde_json::from_slice(&raw) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let Some(tok) = v.pointer("/part/tokens") else {
                    continue;
                };
                let reported = tok["total"].as_i64().unwrap_or(-1);
                let derived = classify(&raw)
                    .step_usage
                    .map(|u| u.total_tokens)
                    .unwrap_or(-2);
                assert_eq!(
                    derived, reported,
                    "{name}: derived total {derived} != opencode's own {reported} for {tok}"
                );
                checked += 1;
            }
        }
        assert!(
            checked >= 4,
            "expected several step_finish lines, got {checked}"
        );
    }

    // Usage is per STEP, so a turn total is the SUM. Pinning the real figure from the committed
    // capture is what makes the "take the last step_finish" mistake fail loudly: that reading
    // returns 13886 for this turn, which is not far enough off to look wrong on a dashboard.
    #[test]
    fn turn_usage_is_the_sum_of_every_step_not_the_last() {
        let mut acc = Usage::default();
        let mut last = Usage::default();
        for l in lines("happy.jsonl") {
            if let Some(u) = classify(&l).step_usage {
                add_usage(&mut acc, &u);
                last = u;
            }
        }
        assert_eq!(acc.total_tokens, 52489, "summed turn total");
        assert_eq!(last.total_tokens, 13886, "the LAST step alone");
        assert_ne!(
            acc.total_tokens, last.total_tokens,
            "if these ever coincide this test has stopped proving anything"
        );
        // The parts sum too, and still satisfy Usage's documented invariant.
        assert_eq!(
            acc.total_tokens,
            acc.input_tokens
                + acc.output_tokens
                + acc.cache_creation_tokens
                + acc.cache_read_tokens
        );
    }

    // The last `text` line is the turn's result text — it is where a `HANDOFF:` declaration would
    // be, and it is NOT the first text line (which on this capture is a plan announcement).
    #[test]
    fn text_lines_surface_as_notifications_and_the_last_is_the_result() {
        let cs: Vec<Classified> = lines("happy.jsonl").iter().map(|l| classify(l)).collect();
        let texts: Vec<&Classified> = cs.iter().filter(|c| !c.text.is_empty()).collect();
        assert_eq!(texts.len(), 2);
        assert!(texts.iter().all(|c| c.ok));
        assert!(
            texts
                .iter()
                .all(|c| c.event.event_type == EVENT_NOTIFICATION)
        );
        assert_eq!(texts[0].text, "I'll do these steps in order.");
        assert!(
            texts[1]
                .text
                .starts_with("1. NOTES.md describes the build counter"),
            "the final text is the substantive answer, got {:?}",
            texts[1].text
        );
    }

    // The 401 capture: ONE line, the whole stream, carrying the status code and the retryability
    // boolean opencode uniquely reports. It is surfaced as a failure but NOT as terminal — see the
    // doc comment on `Classified::failure`.
    #[test]
    fn error_capture_carries_status_and_retryable_and_is_not_terminal() {
        let ls = lines("failure-401.jsonl");
        assert_eq!(ls.len(), 1, "the whole failure stream is one line");
        let c = classify(&ls[0]);
        assert!(c.ok);
        assert_eq!(c.event.event_type, EVENT_TURN_FAILED);
        assert!(!c.terminal, "an error line must not be read as terminal");
        let f = c.failure.expect("failure");
        assert_eq!(f.name, "APIError");
        assert_eq!(f.status_code, 401);
        assert!(!f.retryable);
        assert_eq!(f.message, "The API key you provided is invalid.");
        assert!(f.summary().contains("status 401"));
        assert!(!f.summary().contains("retryable"));
        // The exit status the same run produced is recorded beside it.
        assert_eq!(
            String::from_utf8_lossy(&capture("failure-401.exit")).trim(),
            "1"
        );
    }

    // The resume capture is the same session id as the turn it continues, which is what makes
    // `-s <id>` a correct resume rather than a new session that happens to answer.
    #[test]
    fn resume_capture_continues_the_same_session() {
        let cs: Vec<Classified> = lines("resume.jsonl").iter().map(|l| classify(l)).collect();
        assert!(
            cs.iter()
                .all(|c| c.session_id == "ses_f6cfb07faffeOeHel6dw2uoYgm"),
            "resume keeps the happy capture's session id"
        );
        assert_eq!(cs.iter().filter(|c| c.terminal).count(), 1);
        assert_eq!(
            cs.iter()
                .find(|c| !c.text.is_empty())
                .map(|c| c.text.as_str()),
            Some("8"),
            "the resumed turn answers from the prior turn's context"
        );
    }

    // The long capture batches 11 tool calls into 4 steps where an earlier capture of the SAME
    // prompt used 6 (spike README, "the same prompt does not produce the same step grouping"). The
    // parser must therefore not depend on a step count — only on the reason of the last one.
    #[test]
    fn long_capture_parses_without_assuming_a_step_count() {
        let cs: Vec<Classified> = lines("long-turn.jsonl")
            .iter()
            .map(|l| classify(l))
            .collect();
        let steps = cs.iter().filter(|c| c.step_usage.is_some()).count();
        assert!(steps > 1, "several steps, got {steps}");
        assert_eq!(
            cs.iter().filter(|c| c.terminal).count(),
            1,
            "exactly one terminal step whatever the grouping"
        );
        assert!(cs.last().map(|c| c.terminal).unwrap_or(false));
    }

    // Defensive: the parser never panics and never invents an event on junk. A partially-written
    // final line is a real case (the runner reads a live pipe).
    #[test]
    fn tolerates_blank_non_json_and_unknown_lines() {
        for raw in [
            &b""[..],
            b"   ",
            b"not json at all",
            br#"{"type":"step_start"}"#,
            br#"{"type":"something_new","sessionID":"ses_x"}"#,
            br#"{"type":"text","part":{"text":"   "}}"#,
        ] {
            let c = classify(raw);
            assert!(!c.ok, "junk/unsurfaced line must not be ok: {:?}", raw);
            assert!(!c.terminal);
            assert!(c.failure.is_none());
        }
        assert_eq!(
            classify(br#"{"type":"something_new","sessionID":"ses_x"}"#).session_id,
            "ses_x",
            "an unsurfaced line still yields its session id"
        );
    }
}
