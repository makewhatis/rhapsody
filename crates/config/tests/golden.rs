//! Golden parity test — the P1 GATE.
//!
//! The three committed capture workflows (`harness/capture/workflows/{minimal,full,graphite}.md`)
//! must, when rendered through [`effective_json::render`] and passed through the shared
//! `harness_fixtures::normalize`, be BYTE-IDENTICAL to the R4 fixtures
//! `harness/fixtures/config/{minimal,full,graphite}.json` — the `GET /api/v1/config` responses the
//! reference Go daemon produced. This proves the Rust config parser + effective-config view match
//! Symphony v0.4.0 exactly.
//!
//! The capture workflows carry three placeholders the capture pipeline sed-substitutes before the
//! daemon loads them (see `harness/capture/workflows/minimal.md`):
//!
//! - `__STUB_PORT__` → the linear-stub port; normalize.sh reduces `127.0.0.1:<port>` → `<PORT>`.
//! - `__CLAUDE_CMD__` → `$CAPTURE_HOME/bin/fake-claude`; normalize reduces `$CAPTURE_HOME` → `<HOME>`.
//! - `__STORE_PATH__` → `$CAPTURE_HOME/symphony.db`; same `<HOME>` rule.
//!
//! This test substitutes them with values under a synthetic `CAPTURE_HOME` and normalizes the render
//! with that same home, so the rendered paths/port reduce to the exact `<HOME>`/`<PORT>` the
//! committed (already-normalized) fixtures contain.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rhapsody_config::{decode, effective_json, resolve, validate, workflow};
use serde_json::Value;

/// Synthetic daemon `$HOME` the placeholders substitute in and `normalize_with_home` reduces to
/// `<HOME>`. Any absolute path works; this one never collides with a real fixture value.
const CAPTURE_HOME: &str = "/capture-home";
/// Any digit run: `127.0.0.1:<this>` normalizes to `127.0.0.1:<PORT>`.
const STUB_PORT: &str = "51234";

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn capture_workflow_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../harness/capture/workflows/{name}.md"))
}

/// Load a capture workflow with the three placeholders substituted, via the real `workflow::load`
/// (front-matter split + YAML parse), by materializing it under a unique temp file.
fn load_substituted(name: &str) -> workflow::Definition {
    let raw = std::fs::read_to_string(capture_workflow_path(name))
        .unwrap_or_else(|e| panic!("read capture workflow {name}: {e}"));
    let substituted = raw
        .replace("__STUB_PORT__", STUB_PORT)
        .replace("__CLAUDE_CMD__", &format!("{CAPTURE_HOME}/bin/fake-claude"))
        .replace("__STORE_PATH__", &format!("{CAPTURE_HOME}/symphony.db"));

    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rhapsody-golden-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("WORKFLOW.md");
    std::fs::write(&path, substituted).expect("write temp workflow");
    let def = workflow::load(&path).unwrap_or_else(|e| panic!("load workflow {name}: {e}"));
    let _ = std::fs::remove_dir_all(&dir);
    def
}

/// Recursively sort object keys, mirroring the capture pipeline's `jq -S .` (which stabilizes key
/// order before the fixture is committed).
fn sort_keys(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let sorted: BTreeMap<String, Value> =
                m.into_iter().map(|(k, v)| (k, sort_keys(v))).collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

/// The P1 gate: effective config byte-identical to the Go fixtures for minimal / full / graphite.
#[test]
fn effective_config_matches_go_fixtures() {
    for wf in ["minimal", "full", "graphite"] {
        let def = load_substituted(wf);

        // Sanity: the config must pass the daemon's full load pipeline (Decode -> Resolve ->
        // Validate). The workflows are captured from a running daemon, so this always holds; a
        // failure here would flag a parser regression before the golden diff even runs.
        let decoded = decode(&def).unwrap_or_else(|e| panic!("decode {wf}: {e}"));
        let mut resolved = resolve(decoded, ".").unwrap_or_else(|e| panic!("resolve {wf}: {e}"));
        validate(&mut resolved).unwrap_or_else(|e| panic!("validate {wf}: {e}"));

        // Render the GET /api/v1/config view (render decodes `def` internally, matching the Go GET
        // handler which uses Decode, never Resolve).
        let rendered = sort_keys(effective_json::render(&def));
        // `jq -S .` emits a trailing newline; match it so the byte comparison is exact.
        let pretty = format!("{}\n", serde_json::to_string_pretty(&rendered).unwrap());
        let got = harness_fixtures::normalize_with_home(&pretty, CAPTURE_HOME);

        let want = harness_fixtures::normalize_with_home(
            &harness_fixtures::load(&format!("config/{wf}.json")),
            CAPTURE_HOME,
        );

        assert_eq!(got, want, "effective config drift for {wf}");
    }
}
