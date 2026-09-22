//! PB0 fixture gate (STUDIO-995): pins the committed managed-OpenCode broker request fixtures.
//!
//! The fixtures under `harness/harness-spike/opencode/broker/` are real captures of the pinned
//! `opencode` 1.18.30 talking to the loopback fake provider in that directory (see its `README.md`
//! and `capture.sh`). This test is the deterministic half: it reads the committed output and
//! asserts every guarantee `provider-broker-design.md` §9.1 / §14.4 names, so a fixture that drifts
//! — or a capture that drops title-disable, changes the auth shape, or leaks a capability or an
//! absolute path — turns this red without needing a provider, a key, or the real binary.
//!
//! Each assertion is the guard for a named mutation in the ticket's mutation discipline:
//! dropping title-disable changes the happy request count; a drifted route/model/`max_tokens`/
//! header changes a pinned field; a changed agent or model changes the `argv`/`body` snapshot; an
//! omitted adapter identity breaks the compatibility check; and a leaked secret or machine path is
//! caught by the sanitization scan.

use std::path::PathBuf;

use rhapsody_agent::opencode::{SUPPORTED, parse_probe_output, resolve_row};
use serde_json::Value;

/// The closed top-level body schema of the managed main turn
/// (`provider-broker-design.md` §5.3 item 2, measured by PB0).
const CLOSED_KEYS: &[&str] = &[
    "max_tokens",
    "messages",
    "model",
    "stream",
    "stream_options",
    "tool_choice",
    "tools",
];

/// The compaction agent's request is the closed schema minus the tool fields — it is tool-less by
/// construction and must stay that way.
const COMPACTION_KEYS: &[&str] = &[
    "max_tokens",
    "messages",
    "model",
    "stream",
    "stream_options",
];

/// Substrings that must never appear in a committed fixture: a key-shaped seed, the fake
/// capability seed, and machine-local absolute path roots.
const FORBIDDEN_SUBSTRINGS: &[&str] = &[
    "sk-",
    "rhp-fake",
    "/Users/",
    "/home/",
    "/private/",
    "/var/folders",
    "\\Users\\",
];

fn broker_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../harness/harness-spike/opencode/broker")
}

fn read_json(path: &PathBuf) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path:?}: {e}"))
}

fn scenario(name: &str) -> Value {
    read_json(&broker_dir().join("requests").join(format!("{name}.json")))
}

fn str_at<'a>(value: &'a Value, pointer: &str) -> &'a str {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{pointer} is not a string in {value}"))
}

fn requests(doc: &Value) -> &Vec<Value> {
    doc.pointer("/requests")
        .and_then(Value::as_array)
        .expect("requests array")
}

fn body_keys(request: &Value) -> Vec<String> {
    request
        .pointer("/body/keys")
        .and_then(Value::as_array)
        .expect("body keys")
        .iter()
        .map(|v| v.as_str().expect("key string").to_string())
        .collect()
}

/// Asserts the fields every managed request shares, whatever its scenario.
fn assert_common_request_shape(request: &Value) {
    assert_eq!(str_at(request, "/method"), "POST");
    assert_eq!(str_at(request, "/path"), "/v1/chat/completions");
    assert_eq!(
        str_at(request, "/headers/authorization"),
        "Bearer <CAPABILITY>"
    );
    assert_eq!(str_at(request, "/headers/content-type"), "application/json");
    assert_eq!(str_at(request, "/body/model"), "probe-model");
    assert_eq!(
        request.pointer("/body/max_tokens").and_then(Value::as_i64),
        Some(32000)
    );
    assert_eq!(
        request.pointer("/body/stream").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        request
            .pointer("/body/stream_options/include_usage")
            .and_then(Value::as_bool),
        Some(true),
        "streaming requests must ask for usage"
    );
}

fn assert_closed_keys(request: &Value, expected: &[&str]) {
    let mut got = body_keys(request);
    got.sort();
    let mut want: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    want.sort();
    assert_eq!(got, want, "top-level request schema drifted");
}

#[test]
fn compatibility_file_matches_the_compiled_table() {
    let compat = read_json(&broker_dir().join("compatibility.json"));
    assert_eq!(str_at(&compat, "/opencode_version"), "1.18.30");
    assert_eq!(
        str_at(&compat, "/adapter_package"),
        "@ai-sdk/openai-compatible"
    );
    assert_eq!(str_at(&compat, "/adapter_version"), "2.0.41");

    assert_eq!(SUPPORTED.len(), 1);
    let row = &SUPPORTED[0];
    assert_eq!(row.opencode_version, str_at(&compat, "/opencode_version"));
    assert_eq!(row.adapter_package, str_at(&compat, "/adapter_package"));
    assert_eq!(row.adapter_version, str_at(&compat, "/adapter_version"));
}

#[test]
fn probe_fixture_identifies_the_pinned_version_and_is_accepted() {
    let text = std::fs::read_to_string(broker_dir().join("probe.txt")).expect("probe.txt");
    let version = parse_probe_output(&text).expect("probe.txt parses to one exact version");
    assert_eq!(version, "1.18.30");
    let row = resolve_row(&version).expect("the captured version is a supported row");
    assert_eq!(row.adapter_package, "@ai-sdk/openai-compatible");
    assert_eq!(row.adapter_version, "2.0.41");
}

#[test]
fn every_fixture_declares_the_same_compatibility_row() {
    let compat = read_json(&broker_dir().join("compatibility.json"));
    for name in ["happy", "retry", "auth", "compaction", "subagent"] {
        let doc = scenario(name);
        assert_eq!(doc.pointer("/compatibility"), Some(&compat), "{name}");
    }
}

#[test]
fn title_disabled_happy_path_makes_exactly_one_request() {
    let doc = scenario("happy");
    assert_eq!(doc.pointer("/exit_code").and_then(Value::as_i64), Some(0));
    let reqs = requests(&doc);
    assert_eq!(
        reqs.len(),
        1,
        "title-disable must reduce the first turn to one request"
    );
    assert_common_request_shape(&reqs[0]);
    assert_closed_keys(&reqs[0], CLOSED_KEYS);
    // The explicit built-in `build` agent, on the generated provider/model.
    let argv: Vec<&str> = doc
        .pointer("/argv")
        .and_then(Value::as_array)
        .expect("argv")
        .iter()
        .map(|v| v.as_str().expect("argv string"))
        .collect();
    assert!(
        argv.windows(2).any(|w| w == ["--agent", "build"]),
        "{argv:?}"
    );
    assert!(argv.contains(&"<PROVIDER>/probe-model"), "{argv:?}");
}

#[test]
fn a_retry_is_a_second_provider_request() {
    let doc = scenario("retry");
    assert_eq!(doc.pointer("/exit_code").and_then(Value::as_i64), Some(0));
    let reqs = requests(&doc);
    assert_eq!(reqs.len(), 2, "a forwarded retry consumes another request");
    assert_eq!(
        reqs[0].pointer("/response_status").and_then(Value::as_i64),
        Some(500)
    );
    assert_eq!(
        reqs[1].pointer("/response_status").and_then(Value::as_i64),
        Some(200)
    );
    for request in reqs {
        assert_common_request_shape(request);
        assert_closed_keys(request, CLOSED_KEYS);
    }
}

#[test]
fn auth_failure_is_non_retryable_and_single_request() {
    let doc = scenario("auth");
    assert_eq!(doc.pointer("/exit_code").and_then(Value::as_i64), Some(1));
    let reqs = requests(&doc);
    assert_eq!(reqs.len(), 1, "a non-retryable 401 must not be retried");
    assert_eq!(
        reqs[0].pointer("/response_status").and_then(Value::as_i64),
        Some(401)
    );
    assert_common_request_shape(&reqs[0]);
    assert_closed_keys(&reqs[0], CLOSED_KEYS);
}

#[test]
fn compaction_stays_on_the_generated_model_and_tool_less_schema() {
    let doc = scenario("compaction");
    assert_eq!(doc.pointer("/exit_code").and_then(Value::as_i64), Some(0));
    let reqs = requests(&doc);
    assert_eq!(reqs.len(), 1);
    assert_common_request_shape(&reqs[0]);
    assert_closed_keys(&reqs[0], COMPACTION_KEYS);
    assert_eq!(
        reqs[0]
            .pointer("/body/tool_names")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(0),
        "compaction is tool-less"
    );
    assert!(
        reqs[0]
            .pointer("/body/tool_choice")
            .is_none_or(Value::is_null)
    );
}

#[test]
fn subagent_requests_stay_on_the_generated_model_and_closed_schema() {
    let doc = scenario("subagent");
    assert_eq!(doc.pointer("/exit_code").and_then(Value::as_i64), Some(0));
    let reqs = requests(&doc);
    assert_eq!(
        reqs.len(),
        3,
        "main tool call, the subagent turn, then the resumed main turn"
    );
    for request in reqs {
        assert_common_request_shape(request);
        assert_closed_keys(request, CLOSED_KEYS);
    }
    let first_tools: Vec<&str> = reqs[0]
        .pointer("/body/tool_names")
        .and_then(Value::as_array)
        .expect("tool names")
        .iter()
        .map(|v| v.as_str().expect("tool name"))
        .collect();
    assert!(
        first_tools.contains(&"task"),
        "the fixture must exercise the native subagent path"
    );
    // The subagent turn carries the tool result plus the delegated prompt.
    let roles: Vec<&str> = reqs[2]
        .pointer("/body/message_roles")
        .and_then(Value::as_array)
        .expect("roles")
        .iter()
        .map(|v| v.as_str().expect("role"))
        .collect();
    assert!(roles.contains(&"tool"), "{roles:?}");
}

#[test]
fn no_secret_or_machine_path_is_committed() {
    let dir = broker_dir();
    let mut checked = 0;
    let entries = std::fs::read_dir(dir.join("requests")).expect("requests dir");
    for entry in entries {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("fixture text");
        for needle in FORBIDDEN_SUBSTRINGS {
            assert!(
                !text.contains(needle),
                "{path:?} contains forbidden {needle:?}; a fake key or machine path was committed"
            );
        }
        assert!(
            !text.contains("<UNEXPECTED>"),
            "{path:?} proves the wrong auth path"
        );
        checked += 1;
    }
    assert_eq!(checked, 5, "expected every scenario fixture to be scanned");
}
