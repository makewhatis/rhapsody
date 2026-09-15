//! Event humanizer for the UI — a parity port of Go `internal/agent/humanize.go`.
//!
//! [`humanize_stream_line`] parses one raw agent transcript line into zero or more [`LogEntry`]
//! values (the shape the `/log` API endpoint consumes). It is a pure, defensive transform: it never
//! panics on missing/odd fields and tolerates a partially-written final line (invalid JSON → empty).
//!
//! ## Two harnesses, one entry point (STUDIO-902)
//!
//! Claude's stream-json and opencode's JSONL are both handled here, dispatched by the line's own
//! `type`. That works because the two vocabularies are **disjoint**: claude's transcript lines are
//! `system` / `assistant` / `user` / `result` / `rate_limit_event`, and opencode's are `step_start`
//! / `tool_use` / `text` / `step_finish` / `error`. (`tool_use` and `text` do appear in claude
//! transcripts, but as content-block types NESTED inside an `assistant` line, never as a top-level
//! line type — so there is no collision at the level this function dispatches on.) Every opencode
//! arm therefore lands where claude's `_ => Vec::new()` default used to, and no claude line reaches
//! a different arm than before.
//!
//! ⚠️ **Sniffing the line type is a deliberate stand-in for slice 3, not a preferred design.**
//! Design §6.1 has `humanize` select its parser from the harness RECORDED ON THE RUN, which needs
//! the `runs.harness` column slice 3 adds (§6.2). STUDIO-902 landed ahead of that slice and does not
//! add store columns, so the harness is inferred from the content instead. When slice 3 lands, the
//! dispatch should become explicit and this note should go with it — the disjointness argued above
//! is a property of today's two vocabularies, and a third harness is not obliged to preserve it.

use serde::Deserialize;
use serde_json::value::RawValue;
use std::collections::BTreeMap;

/// `LogEntry` is one humanized line of a claude stream-json transcript: the shared shape produced
/// by [`humanize_stream_line`] and consumed by the `/log` API endpoint (the HTTP layer assigns the
/// 1-based `seq` + the wire json tags). It carries no `seq` and no json tags on purpose, mirroring
/// the reference split so the humanizer stays a pure transform.
///
/// `kind` is one of: `"thinking"`, `"text"`, `"tool_use"`, `"tool_result"`, `"event"`. `tool` is
/// set only on `tool_use` entries (the tool's name). `text` is a short, single-line summary
/// appropriate to the kind (assistant text, a compact tool-input summary, a short tool-result
/// excerpt, or an event label).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogEntry {
    pub kind: String,
    pub tool: String,
    pub text: String,
}

/// Bounds summarized tool I/O (inputs/results) — the usual verbosity offenders.
const MAX_LOG_TEXT_RUNES: usize = 240;

/// Bounds agent PROSE (text/thinking). Far more generous than [`MAX_LOG_TEXT_RUNES`] so reasoning
/// reads in full, while still capping pathological dumps (an echoed skill doc / a whole
/// WORKFLOW.md). The dashboard collapses anything long behind a "Show more" toggle.
const MAX_PROSE_RUNES: usize = 4000;

/// The defensively-decoded shape used only by the humanizer. Kept separate from the parser's line
/// type so the richer fields the transcript needs don't perturb event mapping or billing. Every
/// field is optional (container-level `#[serde(default)]`): any absent field contributes its zero
/// value rather than failing the decode.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct HumanizeLine {
    #[serde(rename = "type")]
    r#type: String,
    subtype: String,
    /// `result` carries `is_error`.
    is_error: bool,
    /// Content can live at the top level (user/tool_result lines) or nested under `message`
    /// (assistant lines). Both are decoded; `message.content` is preferred when present.
    content: Vec<HumanizeBlock>,
    message: Option<HumanizeMessage>,
    rate_limit_info: Option<RateLimitInfo>,
    /// opencode (STUDIO-902): every one of its line types carries its payload under `part`.
    part: Option<OpencodePart>,
    /// opencode: the in-band `error` line's payload.
    error: Option<OpencodeError>,
}

/// opencode's `part` object. One struct covers all of its line types because the CLI reuses the
/// same envelope for each; every field is optional, so a line contributes only what it has.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OpencodePart {
    /// `text` lines.
    text: String,
    /// `tool_use` lines: the tool name, already in opencode's `<server>_<tool>` spelling.
    tool: String,
    /// `tool_use` lines: input, output and status in ONE object — unlike claude, which splits the
    /// call and its result across two transcript lines.
    state: Option<OpencodeToolState>,
    /// `step_finish` lines: `"stop"` ends the turn, `"tool-calls"` does not.
    reason: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OpencodeToolState {
    status: String,
    input: Option<Box<RawValue>>,
    output: Option<Box<RawValue>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OpencodeError {
    name: String,
    data: OpencodeErrorData,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OpencodeErrorData {
    message: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct HumanizeMessage {
    content: Vec<HumanizeBlock>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RateLimitInfo {
    #[serde(rename = "rateLimitType")]
    rate_limit_type: String,
    #[serde(rename = "resetsAt")]
    resets_at: String,
}

/// One content block. Fields are read defensively: any block whose shape we don't recognize
/// contributes nothing rather than panicking. `input` / `content` stay as raw JSON (Go's
/// `json.RawMessage`) and are re-parsed contextually by the summarizers.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct HumanizeBlock {
    #[serde(rename = "type")]
    r#type: String,
    /// text block
    text: String,
    /// thinking block
    thinking: String,
    /// tool_use block: the tool's name
    name: String,
    /// tool_use block: the tool input object
    input: Option<Box<RawValue>>,
    /// tool_result content can be a string OR an array of blocks (e.g. `[{type:text,text:…}]`).
    content: Option<Box<RawValue>>,
}

/// Parses one raw stream-json line into zero or more [`LogEntry`] values, oldest content-block
/// first. The returned vec is empty for lines that carry nothing meaningful (unknown types, empty
/// assistant messages). It never panics on missing/odd fields and tolerates a partially-written
/// final line (invalid JSON → empty).
pub fn humanize_stream_line(raw: &[u8]) -> Vec<LogEntry> {
    if String::from_utf8_lossy(raw).trim().is_empty() {
        return Vec::new();
    }
    let line: HumanizeLine = match serde_json::from_slice(raw) {
        Ok(l) => l,
        Err(_) => return Vec::new(),
    };

    match line.r#type.as_str() {
        "system" => {
            if line.subtype == "init" {
                vec![event_entry("session started")]
            } else {
                Vec::new()
            }
        }
        "result" => {
            if line.is_error {
                let label = if line.subtype.is_empty() {
                    "turn failed".to_string()
                } else {
                    format!("turn failed: {}", line.subtype)
                };
                vec![event_entry(&label)]
            } else {
                vec![event_entry("turn completed")]
            }
        }
        "rate_limit_event" => match &line.rate_limit_info {
            Some(info) => vec![event_entry(&format!(
                "rate limit: {} resets {}",
                info.rate_limit_type, info.resets_at
            ))],
            None => vec![event_entry("rate limit")],
        },
        "assistant" | "user" => {
            let blocks = match &line.message {
                Some(m) if !m.content.is_empty() => &m.content,
                _ => &line.content,
            };
            humanize_blocks(blocks)
        }
        // --- opencode (STUDIO-902) ---------------------------------------------------------
        // `step_start` is deliberately absent: it is a step boundary, not content, and opencode
        // emits one per step (four on the committed happy capture), so surfacing it would put
        // four meaningless rows in a timeline where claude puts none.
        "text" => {
            let t = line.part.as_ref().map(|p| p.text.trim()).unwrap_or("");
            if t.is_empty() {
                Vec::new()
            } else {
                vec![LogEntry {
                    kind: "text".to_string(),
                    tool: String::new(),
                    text: truncate(t, MAX_PROSE_RUNES),
                }]
            }
        }
        // ONE opencode line becomes TWO entries. opencode packs a tool call and its result into a
        // single `tool_use` line with a `state` object, where claude splits them across an
        // `assistant` line and a following `user` line — so producing both here is what makes the
        // two harnesses render the same call/result pairing in the Trace console rather than
        // opencode showing calls with no results.
        "tool_use" => {
            let Some(p) = line.part.as_ref() else {
                return Vec::new();
            };
            let state = p.state.as_ref();
            let mut out = vec![LogEntry {
                kind: "tool_use".to_string(),
                tool: p.tool.clone(),
                text: truncate(
                    &summarize_input(state.and_then(|s| s.input.as_deref())),
                    MAX_LOG_TEXT_RUNES,
                ),
            }];
            // Only a finished call has a result to show; a `pending`/`running` state has not
            // produced one yet, and rendering "(ok)" for it would claim an outcome that has not
            // happened.
            if let Some(s) = state
                && s.status == "completed"
            {
                let summary = summarize_result(s.output.as_deref());
                out.push(LogEntry {
                    kind: "tool_result".to_string(),
                    tool: String::new(),
                    text: if summary.is_empty() {
                        "(ok)".to_string()
                    } else {
                        truncate(&summary, MAX_LOG_TEXT_RUNES)
                    },
                });
            }
            out
        }
        // opencode's turn end. Intermediate steps carry `reason: "tool-calls"` and are not a turn
        // boundary — labelling one "turn completed" would report a turn finishing once per step.
        "step_finish" => {
            let stop = line.part.as_ref().is_some_and(|p| p.reason == "stop");
            if stop {
                vec![event_entry("turn completed")]
            } else {
                Vec::new()
            }
        }
        "error" => {
            let label = match &line.error {
                Some(e) if !e.data.message.is_empty() => {
                    format!("turn failed: {}: {}", e.name, e.data.message)
                }
                Some(e) if !e.name.is_empty() => format!("turn failed: {}", e.name),
                _ => "turn failed".to_string(),
            };
            vec![event_entry(&truncate(&label, MAX_LOG_TEXT_RUNES))]
        }
        _ => Vec::new(),
    }
}

/// Builds an `"event"`-kind entry with the given label.
fn event_entry(text: &str) -> LogEntry {
    LogEntry {
        kind: "event".to_string(),
        tool: String::new(),
        text: text.to_string(),
    }
}

/// Folds a content-block slice into entries (one per meaningful block).
fn humanize_blocks(blocks: &[HumanizeBlock]) -> Vec<LogEntry> {
    let mut out = Vec::new();
    for b in blocks {
        match b.r#type.as_str() {
            "text" => {
                // Agent PROSE keeps its newlines and most of its length (generous MAX_PROSE_RUNES
                // cap; the dashboard collapses long entries behind "Show more") — no first-line clip.
                let t = b.text.trim();
                if !t.is_empty() {
                    out.push(LogEntry {
                        kind: "text".to_string(),
                        tool: String::new(),
                        text: truncate(t, MAX_PROSE_RUNES),
                    });
                }
            }
            "thinking" => {
                let t = b.thinking.trim();
                if !t.is_empty() {
                    out.push(LogEntry {
                        kind: "thinking".to_string(),
                        tool: String::new(),
                        text: truncate(t, MAX_PROSE_RUNES),
                    });
                }
            }
            "tool_use" | "server_tool_use" => {
                out.push(LogEntry {
                    kind: "tool_use".to_string(),
                    tool: b.name.clone(),
                    text: truncate(&summarize_input(b.input.as_deref()), MAX_LOG_TEXT_RUNES),
                });
            }
            "tool_result" => {
                let s = summarize_result(b.content.as_deref());
                let text = if s.is_empty() {
                    "(ok)".to_string()
                } else {
                    truncate(&s, MAX_LOG_TEXT_RUNES)
                };
                out.push(LogEntry {
                    kind: "tool_result".to_string(),
                    tool: String::new(),
                    text,
                });
            }
            _ => {}
        }
    }
    out
}

/// Renders a tool_use input object as a compact one-line summary. Object inputs become `key=value`
/// pairs (sorted for determinism via [`BTreeMap`], values clipped); other JSON shapes are collapsed
/// to a single line. Empty/absent input yields `""`.
fn summarize_input(raw: Option<&RawValue>) -> String {
    let Some(raw) = raw else {
        return String::new();
    };
    let text = raw.get();
    if let Ok(obj) = serde_json::from_str::<BTreeMap<String, Box<RawValue>>>(text) {
        // BTreeMap iterates in sorted key order — the mirror of Go's `sort.Strings(keys)`.
        let parts: Vec<String> = obj
            .iter()
            .map(|(k, v)| format!("{k}={}", clip_value(v)))
            .collect();
        return parts.join(" ");
    }
    collapse_ws(text)
}

/// Renders one JSON value for an input summary: strings unquoted+clipped, other scalars/compound
/// shapes collapsed to a single short line.
fn clip_value(raw: &RawValue) -> String {
    if let Ok(s) = serde_json::from_str::<String>(raw.get()) {
        return truncate(&collapse_ws(&s), 60);
    }
    truncate(&collapse_ws(raw.get()), 60)
}

/// Renders tool_result content (which may be a bare string or an array of content blocks) as a
/// short single line.
fn summarize_result(raw: Option<&RawValue>) -> String {
    let Some(raw) = raw else {
        return String::new();
    };
    let text = raw.get();
    if let Ok(s) = serde_json::from_str::<String>(text) {
        return first_line(&s);
    }
    if let Ok(blocks) = serde_json::from_str::<Vec<HumanizeBlock>>(text) {
        for b in &blocks {
            let t = first_line(&b.text);
            if !t.is_empty() {
                return t;
            }
        }
        return String::new();
    }
    collapse_ws(text)
}

/// Flattens all runs of whitespace (incl. newlines) to single spaces and trims, so a multi-line
/// JSON value renders as one tidy line.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Collapses `s` to its first non-empty line and trims surrounding whitespace, so a multi-line
/// assistant message renders as a single timeline row.
fn first_line(s: &str) -> String {
    for ln in s.split('\n') {
        let t = ln.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    s.trim().to_string()
}

/// Clamps `s` to at most `max` runes (chars), appending an ellipsis when it cuts.
fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return chars[..max].iter().collect();
    }
    let mut out: String = chars[..max - 1].iter().collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors Go `agent.TestHumanizeStreamLine_SystemInit` (humanize_test.go).
    #[test]
    fn system_init() {
        let got = humanize_stream_line(br#"{"type":"system","subtype":"init"}"#);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "event");
        assert_eq!(got[0].text, "session started");
        // Non-init system carries nothing.
        assert!(humanize_stream_line(br#"{"type":"system","subtype":"other"}"#).is_empty());
    }

    // Mirrors Go `agent.TestHumanizeStreamLine_Result`.
    #[test]
    fn result() {
        let ok = humanize_stream_line(br#"{"type":"result"}"#);
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].kind, "event");
        assert_eq!(ok[0].text, "turn completed");

        let err =
            humanize_stream_line(br#"{"type":"result","is_error":true,"subtype":"max_turns"}"#);
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].kind, "event");
        assert_eq!(err[0].text, "turn failed: max_turns");

        let err_no_subtype = humanize_stream_line(br#"{"type":"result","is_error":true}"#);
        assert_eq!(err_no_subtype.len(), 1);
        assert_eq!(err_no_subtype[0].text, "turn failed");
    }

    // Mirrors Go `agent.TestHumanizeStreamLine_RateLimit`.
    #[test]
    fn rate_limit() {
        let got = humanize_stream_line(
            br#"{"type":"rate_limit_event","rate_limit_info":{"rateLimitType":"primary","resetsAt":"2026-06-01T00:00:00Z"}}"#,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "event");
        assert_eq!(
            got[0].text,
            "rate limit: primary resets 2026-06-01T00:00:00Z"
        );

        let no_info = humanize_stream_line(br#"{"type":"rate_limit_event"}"#);
        assert_eq!(no_info.len(), 1);
        assert_eq!(no_info[0].text, "rate limit");
    }

    // Mirrors Go `agent.TestHumanizeStreamLine_AssistantText`: prose kept in full (multi-line), no
    // first-line clip.
    #[test]
    fn assistant_text() {
        let got = humanize_stream_line(
            br#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello\nworld"}]}}"#,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "text");
        assert_eq!(got[0].text, "hello\nworld");
    }

    // Mirrors Go `agent.TestHumanizeStreamLine_Thinking`: full thinking kept (multi-line).
    #[test]
    fn thinking() {
        let got = humanize_stream_line(
            br#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"let me think\nmore"}]}}"#,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "thinking");
        assert_eq!(got[0].text, "let me think\nmore");
    }

    // Mirrors Go `agent.TestHumanizeStreamLine_ToolUse`: sorted k=v summary, command before timeout.
    #[test]
    fn tool_use() {
        let got = humanize_stream_line(
            br#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls -la","timeout":5}}]}}"#,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "tool_use");
        assert_eq!(got[0].tool, "Bash");
        assert_eq!(got[0].text, "command=ls -la timeout=5");
    }

    // Mirrors Go `agent.TestHumanizeStreamLine_ToolResult`.
    #[test]
    fn tool_result() {
        // String content.
        let s = humanize_stream_line(
            br#"{"type":"user","message":{"content":[{"type":"tool_result","content":"output line 1\nline 2"}]}}"#,
        );
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].kind, "tool_result");
        assert_eq!(s[0].text, "output line 1");

        // Array-of-blocks content.
        let blocks = humanize_stream_line(
            br#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"blocked output"}]}]}}"#,
        );
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "blocked output");

        // Empty content -> "(ok)".
        let empty = humanize_stream_line(
            br#"{"type":"user","message":{"content":[{"type":"tool_result"}]}}"#,
        );
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].text, "(ok)");
    }

    // Mirrors Go `agent.TestHumanizeStreamLine_TopLevelContent`: content at top level (not nested
    // under message).
    #[test]
    fn top_level_content() {
        let got = humanize_stream_line(
            br#"{"type":"user","content":[{"type":"text","text":"top-level"}]}"#,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "top-level");
    }

    // Mirrors Go `agent.TestHumanizeStreamLine_Tolerant`.
    #[test]
    fn tolerant() {
        assert!(humanize_stream_line(b"").is_empty());
        assert!(humanize_stream_line(b"   \n").is_empty());
        assert!(humanize_stream_line(br#"{"type":"assistant","message":{"#).is_empty());
        assert!(humanize_stream_line(br#"{"type":"unknown"}"#).is_empty());
    }

    // Mirrors Go `agent.TestHumanizeStreamLine_Truncation`.
    // --- opencode (STUDIO-902) -------------------------------------------------------------
    //
    // These run against the COMMITTED spike captures rather than hand-written lines. The
    // acceptance criterion is that opencode's stream drives the Trace console the way claude's
    // does, and only real captured output can show that.

    fn opencode_capture(name: &str) -> Vec<Vec<u8>> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../harness/harness-spike/opencode")
            .join(name);
        let raw = std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
        raw.split(|&b| b == b'\n')
            .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
            .map(<[u8]>::to_vec)
            .collect()
    }

    // The whole happy capture, humanized. The shape a reader should get is the same one claude
    // produces: prose, tool calls each followed by their result, and a terminal event — with no
    // row per step boundary.
    #[test]
    fn opencode_happy_capture_drives_the_timeline() {
        let entries: Vec<LogEntry> = opencode_capture("happy.jsonl")
            .iter()
            .flat_map(|l| humanize_stream_line(l))
            .collect();

        let kinds: Vec<&str> = entries.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "tool_use",
                "tool_result", // read
                "tool_use",
                "tool_result", // read
                "text",        // "I'll do these steps in order."
                "tool_use",
                "tool_result", // edit
                "tool_use",
                "tool_result", // symphony_symphony_state
                "tool_use",
                "tool_result", // bash
                "text",        // the final answer
                "event",       // turn completed
            ],
            "got {entries:#?}"
        );

        // Every tool_use is immediately followed by its own tool_result — the pairing the console
        // renders, and the thing opencode's single-line shape would lose if only one were emitted.
        for (i, e) in entries.iter().enumerate() {
            if e.kind == "tool_use" {
                assert_eq!(
                    entries.get(i + 1).map(|n| n.kind.as_str()),
                    Some("tool_result"),
                    "tool_use at {i} has no result"
                );
                assert!(!e.tool.is_empty(), "tool_use at {i} names no tool");
            }
        }

        let tools: Vec<&str> = entries
            .iter()
            .filter(|e| e.kind == "tool_use")
            .map(|e| e.tool.as_str())
            .collect();
        assert_eq!(
            tools,
            vec!["read", "read", "edit", "symphony_symphony_state", "bash"],
            "the daemon's own MCP tool appears in opencode's `<server>_<tool>` spelling"
        );

        assert_eq!(
            entries.last().map(|e| e.text.as_str()),
            Some("turn completed")
        );

        // Input summaries are real, and they are rendered exactly as claude's are — `key=value`
        // pairs in sorted key order.
        let summaries: Vec<&str> = entries
            .iter()
            .filter(|e| e.kind == "tool_use")
            .map(|e| e.text.as_str())
            .collect();
        assert!(summaries[0].starts_with("filePath="), "{summaries:?}");
        assert!(summaries[0].ends_with("NOTES.md"), "{summaries:?}");
        assert!(
            summaries[4].contains("command=./check.sh") && summaries[4].contains("workdir="),
            "bash args render as sorted key=value pairs: {summaries:?}"
        );
        // ⚠️ `symphony_symphony_state` really does take `{}`, so its summary is EMPTY — and that is
        // the same thing claude's humanizer produces for a no-argument tool (the console's own
        // fixtures carry `mcp__symphony__symphony_handoff` with `text: ""`). Asserting it here
        // rather than requiring every summary to be non-empty keeps this test describing the
        // captured data instead of a tidier version of it.
        assert_eq!(summaries[3], "", "a no-argument tool summarizes to nothing");
    }

    // ⚠️ Exactly ONE "turn completed" for a four-step turn. `step_finish` arrives once per step, so
    // an arm that did not read `reason` would report the turn finishing four times.
    #[test]
    fn opencode_reports_one_turn_completed_per_turn_not_one_per_step() {
        for name in ["happy.jsonl", "long-turn.jsonl", "resume.jsonl"] {
            let events: Vec<LogEntry> = opencode_capture(name)
                .iter()
                .flat_map(|l| humanize_stream_line(l))
                .filter(|e| e.kind == "event")
                .collect();
            assert_eq!(
                events.len(),
                1,
                "{name}: expected one terminal event, got {events:#?}"
            );
            assert_eq!(events[0].text, "turn completed");
        }
    }

    // The 401 capture renders as a failure naming the provider's own message, so the console shows
    // why rather than an anonymous red row.
    #[test]
    fn opencode_error_capture_renders_the_provider_message() {
        let entries: Vec<LogEntry> = opencode_capture("failure-401.jsonl")
            .iter()
            .flat_map(|l| humanize_stream_line(l))
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "event");
        assert_eq!(
            entries[0].text,
            "turn failed: APIError: The API key you provided is invalid."
        );
    }

    // `step_start` is content-free and arrives once per step; it must add no rows at all.
    #[test]
    fn opencode_step_start_adds_nothing() {
        let starts: Vec<Vec<u8>> = opencode_capture("happy.jsonl")
            .into_iter()
            .filter(|l| {
                serde_json::from_slice::<serde_json::Value>(l)
                    .map(|v| v["type"] == "step_start")
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(
            starts.len(),
            4,
            "the capture really does have 4 step_start lines"
        );
        for l in &starts {
            assert!(humanize_stream_line(l).is_empty());
        }
    }

    // An unfinished tool call has no result to show: claiming "(ok)" would report an outcome that
    // has not happened. The captures only contain completed calls, so this one line is synthetic
    // and says so.
    #[test]
    fn opencode_an_unfinished_tool_call_shows_no_result() {
        let line = br#"{"type":"tool_use","sessionID":"ses_x","part":{"type":"tool","tool":"bash","state":{"status":"running","input":{"command":"sleep 60"}}}}"#;
        let got = humanize_stream_line(line);
        assert_eq!(got.len(), 1, "no result row yet: {got:#?}");
        assert_eq!(got[0].kind, "tool_use");
        assert_eq!(got[0].tool, "bash");
        assert!(got[0].text.contains("sleep 60"));
    }

    // ⚠️ Adding the opencode arms must not have moved any claude line to a different arm. Both
    // vocabularies are exercised through the one entry point here, on real captured lines from
    // each, so a future edit that makes them overlap fails loudly.
    #[test]
    fn claude_lines_are_unaffected_by_the_opencode_arms() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../harness/harness-spike/claude/happy.jsonl");
        let raw = std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
        let claude_lines: Vec<Vec<u8>> = raw
            .split(|&b| b == b'\n')
            .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
            .map(<[u8]>::to_vec)
            .collect();

        // No claude transcript line may carry a top-level type the opencode arms claim.
        let opencode_types = ["step_start", "step_finish", "tool_use", "text", "error"];
        for l in &claude_lines {
            let v: serde_json::Value = match serde_json::from_slice(l) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let t = v["type"].as_str().unwrap_or("");
            assert!(
                !opencode_types.contains(&t),
                "a claude line has top-level type {t:?}, which an opencode arm now claims — the \
                 disjointness this dispatch relies on no longer holds"
            );
        }

        // And the claude capture still humanizes to the shapes it always did.
        let entries: Vec<LogEntry> = claude_lines
            .iter()
            .flat_map(|l| humanize_stream_line(l))
            .collect();
        assert!(
            entries.iter().any(|e| e.text == "session started"),
            "claude's system/init still renders"
        );
        assert!(
            entries.iter().any(|e| e.text == "turn completed"),
            "claude's result line still renders"
        );
        assert!(
            entries
                .iter()
                .any(|e| e.kind == "tool_use" && e.tool.starts_with("mcp__symphony__")),
            "claude's own tool spelling is untouched"
        );
    }

    #[test]
    fn truncation() {
        let big = "x".repeat(400);
        // Agent PROSE (text) is shown in full — no rune cap.
        let full = humanize_stream_line(
            format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{big}"}}]}}}}"#
            )
            .as_bytes(),
        );
        assert_eq!(full.len(), 1);
        assert_eq!(full[0].kind, "text");
        assert_eq!(full[0].text.chars().count(), 400, "prose not truncated");

        // Tool OUTPUTS stay summarized — capped to MAX_LOG_TEXT_RUNES.
        let got = humanize_stream_line(
            format!(r#"{{"type":"user","content":[{{"type":"tool_result","content":"{big}"}}]}}"#)
                .as_bytes(),
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text.chars().count(), MAX_LOG_TEXT_RUNES);
    }
}
