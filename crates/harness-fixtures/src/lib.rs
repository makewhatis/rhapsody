//! harness-fixtures — loads the committed golden fixtures captured from Go Symphony v0.4.0
//! (see `harness/capture/README.md`). Dev-dependency for every porting crate's golden tests:
//! a crate serializes its output, runs it through [`normalize`], and asserts equality with a
//! committed fixture.
//!
//! [`normalize`] MUST stay in lockstep with `harness/capture/normalize.sh` — the two implement
//! the SAME placeholder rules, in the same order. The `normalize_matches_shell_rules` canary
//! runs the shell script and asserts byte-identical output, so any drift turns CI red.
//!
//! `unwrap`/`expect`/`panic!` are intentional here: this is a test-only dev-dependency, so a
//! loud failure (missing fixture, malformed JSON) IS the feature — a silently-skipped golden
//! would defeat the parity gate. The `Regex::new(...)` calls are all on static patterns.

use std::path::{Path, PathBuf};

/// Absolute path to the committed golden fixture tree (`harness/fixtures/`).
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/fixtures")
}

/// Read a committed fixture by path relative to `harness/fixtures/` (e.g. `config/minimal.json`).
/// Panics with an actionable message if it is missing.
pub fn load(rel: &str) -> String {
    let p = fixtures_dir().join(rel);
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("missing fixture {}: {e} — run `make fixtures`", p.display()))
}

/// Read a committed fixture and parse it as JSON. Panics if missing or not valid JSON.
pub fn load_json(rel: &str) -> serde_json::Value {
    serde_json::from_str(&load(rel)).unwrap_or_else(|e| panic!("fixture {rel} is not JSON: {e}"))
}

/// Rust mirror of `harness/capture/normalize.sh` with no `$CAPTURE_HOME` substitution.
/// Use [`normalize_with_home`] when a home path must be rewritten to `<HOME>`.
pub fn normalize(s: &str) -> String {
    normalize_with_home(s, "")
}

/// Rust mirror of `harness/capture/normalize.sh`. Rewrites the machine-specific / wall-clock
/// values a capture produces to the fixed placeholders `<TIMESTAMP>`, `<UUID>`, `<HOME>`,
/// `<PORT>`, `<NUM>`. Each step below mirrors one `-e` of that script's `sed` pipeline, applied
/// in the same order (see the script header for the rule-by-rule rationale) — change them in
/// lockstep or the `normalize_matches_shell_rules` canary fails.
pub fn normalize_with_home(s: &str, home: &str) -> String {
    // 1. RFC3339 timestamps, quoted -> "<TIMESTAMP>"
    let ts = regex::Regex::new(r#""[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+(Z|[+-][0-9:]+)""#).unwrap();
    // 2. bare YYYY-MM-DD dates, quoted -> "<TIMESTAMP>"
    let date = regex::Regex::new(r#""[0-9]{4}-[0-9]{2}-[0-9]{2}""#).unwrap();
    // 3. compact run-transcript timestamps (unquoted, inside transcript_path) -> <TIMESTAMP>
    let compact = regex::Regex::new(r"[0-9]{8}T[0-9]{6}\.[0-9]+Z").unwrap();
    // 4. UUIDs -> <UUID>
    let uuid = regex::Regex::new(
        r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}",
    )
    .unwrap();
    // 6. loopback host:port -> 127.0.0.1:<PORT>
    let port = regex::Regex::new(r"127\.0\.0\.1:[0-9]+").unwrap();
    // 7. wall-clock measurement numerics -> "<NUM>" (quoted, so fixtures stay valid JSON)
    let num =
        regex::Regex::new(r#""([a-z_]*(_at_ms|duration|_running))": *[0-9]+(\.[0-9]+)?"#).unwrap();

    let s = ts.replace_all(s, r#""<TIMESTAMP>""#);
    let s = date.replace_all(&s, r#""<TIMESTAMP>""#);
    let s = compact.replace_all(&s, "<TIMESTAMP>");
    let s = uuid.replace_all(&s, "<UUID>");
    // 5. the capture HOME dir -> <HOME> (literal, like the shell's `s|$CAPTURE_HOME|<HOME>|`).
    //    Skipped when empty so `normalize()` (no home) cannot match every byte boundary.
    let s: String = if home.is_empty() {
        s.into_owned()
    } else {
        s.replace(home, "<HOME>")
    };
    let s = port.replace_all(&s, "127.0.0.1:<PORT>");
    num.replace_all(&s, r#""${1}": "<NUM>""#).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // A single input exercising every placeholder class of normalize.sh, used both to assert
    // the expected per-rule output and to prove byte-identity with the shell script. Ends with
    // a trailing newline so `sed` (per-line) and the Rust whole-string pass agree exactly.
    const SAMPLE: &str = r#"{
  "started_at": "2026-07-10T12:00:00Z",
  "ended_at": "2026-07-10T12:00:00.123456789+02:00",
  "date": "2026-07-10",
  "transcript_path": "/cap/home/.symphony/obslog/20260710T120000.123456789Z-x.jsonl",
  "session_uuid": "6f1e0b9a-1c2d-4e5f-8a9b-0c1d2e3f4a5b",
  "db": "/cap/home/symphony.db",
  "endpoint": "http://127.0.0.1:53211/graphql",
  "seconds_running": 12.5,
  "due_at_ms": 1720612800000,
  "some_duration": 42,
  "interval_ms": 500
}
"#;

    // Run the real normalize.sh with $CAPTURE_HOME=home, feeding `input` on stdin.
    fn shell_normalize(input: &str, home: &str) -> String {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/capture/normalize.sh");
        let mut child = Command::new("bash")
            .arg(&script)
            .env("CAPTURE_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", script.display()));
        {
            let mut stdin = child.stdin.take().expect("child stdin");
            stdin.write_all(input.as_bytes()).expect("write stdin");
        }
        let out = child.wait_with_output().expect("normalize.sh output");
        assert!(
            out.status.success(),
            "normalize.sh failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("normalize.sh output is utf8")
    }

    // Recursively collect fixture paths relative to `harness/fixtures/`.
    fn all_fixtures() -> Vec<String> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("read fixtures dir").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, base, out);
                } else {
                    out.push(
                        path.strip_prefix(base)
                            .expect("under base")
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
        }
        let mut out = Vec::new();
        walk(&fixtures_dir(), &fixtures_dir(), &mut out);
        out.sort();
        out
    }

    // CANARY: exact table set of the committed schema. If schema.sql is edited, regenerated
    // incompatibly, or corrupted, CI goes red — this is the drift gate (spec §4).
    #[test]
    fn canary_schema_has_all_tables() {
        let schema = load("schema.sql");
        // The trailing space is load-bearing: it counts real `CREATE TABLE <name>` statements,
        // so the documented red-on-drift edit (`CREATE TABLE` -> `CREATE TABLE_CORRUPTED`) turns
        // this red. A bare "CREATE TABLE" substring would still match the corrupted text.
        assert_eq!(
            schema.matches("CREATE TABLE ").count(),
            6,
            "schema.sql must hold exactly the 6 v0.4.0 tables"
        );
        for table in [
            "runs",
            "events",
            "retry_queue",
            "claims",
            "totals",
            "run_messages",
        ] {
            assert!(
                schema.contains(&format!("CREATE TABLE {table} (")),
                "schema.sql missing `CREATE TABLE {table}`"
            );
        }
    }

    // CANARY: exact values of the minimal effective config (post-normalization). Catches any
    // drift in config/minimal.json — the fixture P1 (config) will assert its parser against.
    #[test]
    fn canary_minimal_config_exact_values() {
        let cfg = load_json("config/minimal.json");
        let c = &cfg["config"];
        assert_eq!(c["tracker"]["kind"], "linear");
        assert_eq!(c["tracker"]["endpoint"], "http://127.0.0.1:<PORT>/graphql");
        assert_eq!(c["tracker"]["project_slug"], "558008ab185c");
        assert_eq!(
            c["tracker"]["active_states"],
            serde_json::json!(["Todo", "In Progress"])
        );
        assert_eq!(
            c["tracker"]["terminal_states"],
            serde_json::json!(["Done", "Canceled"])
        );
        assert_eq!(c["agent"]["backend"], "claude");
        assert_eq!(c["agent"]["max_concurrent_agents"], 1);
        assert_eq!(c["polling"]["interval_ms"], 500);
        assert_eq!(c["server"]["port"], 0);
        assert_eq!(c["mcp"]["enabled"], false);
        assert_eq!(c["otel"]["enabled"], false);
        assert_eq!(c["claude"]["command"], "<HOME>/bin/fake-claude");
        assert_eq!(c["storage"]["path"], "<HOME>/symphony.db");
        // Nondeterministic capture fields must be fully normalized in the committed golden.
        assert_eq!(cfg["generated_at"], "<TIMESTAMP>");
    }

    // CANARY: the Rust normalizer implements the same rules as normalize.sh. Asserts the
    // expected placeholder for each rule class AND requires byte-identity with the shell
    // script — so editing one normalizer but not the other turns CI red (lockstep gate).
    #[test]
    fn normalize_matches_shell_rules() {
        let home = "/cap/home";
        let n = normalize_with_home(SAMPLE, home);
        // 1 RFC3339 timestamps (Z and ±hh:mm) -> "<TIMESTAMP>"
        assert!(
            n.contains(r#""started_at": "<TIMESTAMP>""#),
            "rfc3339 Z: {n}"
        );
        assert!(
            n.contains(r#""ended_at": "<TIMESTAMP>""#),
            "rfc3339 offset: {n}"
        );
        // 2 bare YYYY-MM-DD dates -> "<TIMESTAMP>"
        assert!(n.contains(r#""date": "<TIMESTAMP>""#), "bare date: {n}");
        // 3 compact transcript timestamp (unquoted, inside the path) -> <TIMESTAMP>
        assert!(n.contains("/obslog/<TIMESTAMP>-x.jsonl"), "compact ts: {n}");
        // 4 UUID -> <UUID>
        assert!(n.contains(r#""session_uuid": "<UUID>""#), "uuid: {n}");
        // 5 capture HOME -> <HOME>
        assert!(n.contains(r#""db": "<HOME>/symphony.db""#), "home: {n}");
        assert!(!n.contains("/cap/home"), "home fully stripped: {n}");
        // 6 loopback host:port -> 127.0.0.1:<PORT>
        assert!(
            n.contains("127.0.0.1:<PORT>") && !n.contains(":53211"),
            "port: {n}"
        );
        // 7 wall-clock numerics -> QUOTED "<NUM>" (_running float, _at_ms, duration)
        assert!(n.contains(r#""seconds_running": "<NUM>""#), "running: {n}");
        assert!(n.contains(r#""due_at_ms": "<NUM>""#), "at_ms: {n}");
        assert!(n.contains(r#""some_duration": "<NUM>""#), "duration: {n}");
        // 7 deviation: plain `_ms` config constants are deterministic and NOT normalized.
        assert!(
            n.contains(r#""interval_ms": 500"#),
            "interval_ms preserved: {n}"
        );

        // Strongest guard: byte-for-byte identical to the shell single-source-of-truth.
        assert_eq!(
            n,
            shell_normalize(SAMPLE, home),
            "normalize() drifted from normalize.sh"
        );
    }

    // CANARY: every committed fixture is already fully normalized, so re-normalizing is a no-op.
    // Catches a raw timestamp / UUID / absolute path / live port hand-edited into a golden.
    #[test]
    fn canary_fixtures_are_normalized() {
        let fixtures = all_fixtures();
        assert!(
            fixtures.len() >= 18,
            "expected the committed fixture tree, got {fixtures:?}"
        );
        for rel in fixtures {
            let raw = load(&rel);
            assert_eq!(
                normalize(&raw),
                raw,
                "fixture {rel} is not idempotent under normalize()"
            );
        }
    }

    // load() must fail loudly (not silently) when a fixture is missing.
    #[test]
    #[should_panic(expected = "missing fixture")]
    fn load_missing_fixture_panics() {
        let _ = load("does/not/exist.json");
    }
}
