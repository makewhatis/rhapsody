//! STUDIO-994 (provider-auth P13) — the cross-lane migration/doctor release gate.
//!
//! This is the release gate's *own* end-to-end surface, deliberately narrow: it drives the real
//! [`rhapsodyd::doctor`] over real workflow files and asserts the two compatibility promises the
//! binding contract makes, rather than re-implementing the hostile broker fixtures PB8
//! (STUDIO-1003) already owns. Concretely:
//!
//! 1. **No-provider legacy workflows stay on the existing parity path.** A Claude workflow with no
//!    `providers:` block yields a valid diagnosis, zero migration warnings, and — critically — the
//!    workflow bytes on disk are unchanged: the design (§7) requires diagnostics to *suggest* the
//!    new form, never rewrite the operator's workflow.
//! 2. **A provider workflow reports its canonical, non-secret binding**, so the registry a doctor
//!    inspects is the same one dispatch derives its credential binding from (config → doctor lane).
//! 3. **A refused config is reported, not hidden**: the daemon's preflight reason reaches the report
//!    and the exit code is nonzero.
//!
//! Live credential *status* (missing/denied/mismatch, broker availability, canary cleanliness) is
//! consumed from the existing P9/PB8 surfaces (`GET /api/v1/providers`,
//! `crates/rhapsodyd/tests/brokered_daemon_e2e.rs`, `crates/provider-broker/tests/*`); this gate
//! proves the migration/diagnostics lane joins them without duplicating them.

use std::io;
use std::path::{Path, PathBuf};

/// A minimal in-memory writer so the test can assert the exact stream contract.
#[derive(Default)]
struct Buf(Vec<u8>);

impl Buf {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }
}

impl io::Write for Buf {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("rhapsody-doctor-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self { dir }
    }

    fn write(&self, name: &str, body: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, body).expect("write workflow");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("RHAPSODY_KEEP_TEST_DIRS").is_none() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn run(path: &Path) -> (i32, String, String) {
    let mut out = Buf::default();
    let mut err = Buf::default();
    let code =
        rhapsodyd::doctor::run_doctor(&[path.to_string_lossy().into_owned()], &mut out, &mut err);
    (code, out.text(), err.text())
}

/// A legacy, no-provider Claude workflow: the pre-provider shape every existing install has.
const LEGACY: &str = r#"---
tracker:
  kind: file
  source: /tmp/issues.json
agent:
  backend: claude
---
body
"#;

/// A provider-first OpenCode workflow selecting one configured provider.
const PROVIDER: &str = r#"---
tracker:
  kind: file
  source: /tmp/issues.json
agent:
  backend: opencode
  provider: fireworks
  model: accounts/fireworks/models/deepseek-v4p1-flash
providers:
  fireworks:
    protocol: openai-compatible
    base_url: https://fireworks.example/inference/v1
    credential:
      source: keychain
---
body
"#;

/// (1) + the read-only promise: a no-provider legacy workflow diagnoses clean and is not rewritten.
#[test]
fn a_legacy_no_provider_workflow_is_clean_and_untouched() {
    let scratch = Scratch::new("legacy");
    let path = scratch.write("WORKFLOW.md", LEGACY);
    let before = std::fs::read(&path).expect("read before");

    let (code, out, err) = run(&path);

    assert_eq!(
        code, 0,
        "a valid legacy workflow must diagnose clean: {err}"
    );
    assert!(
        err.is_empty(),
        "a clean diagnosis writes nothing to stderr: {err}"
    );
    assert!(out.contains("config: valid"), "{out}");
    assert!(
        out.contains("warnings: none"),
        "legacy defaults are not noisy: {out}"
    );
    assert!(out.contains("providers: none configured"), "{out}");
    assert_eq!(
        std::fs::read(&path).expect("read after"),
        before,
        "the doctor must never rewrite the operator's workflow (design §7)"
    );
}

/// (2) A provider workflow reports its canonical binding, and the endpoint the doctor prints is
/// exactly the canonical one the config derives (config → doctor lane, no second derivation).
#[test]
fn a_provider_workflow_reports_its_canonical_binding() {
    let scratch = Scratch::new("provider");
    let path = scratch.write("WORKFLOW.md", PROVIDER);

    let (code, out, err) = run(&path);

    assert_eq!(code, 0, "{err}");
    assert!(out.contains("providers: 1 configured"), "{out}");
    assert!(
        out.contains("fireworks") && out.contains("https://fireworks.example/inference/v1"),
        "the canonical endpoint must appear: {out}"
    );
    assert!(out.contains("binding=ok"), "{out}");
    assert!(
        out.contains(
            "selection: provider=fireworks model=accounts/fireworks/models/deepseek-v4p1-flash"
        ),
        "{out}"
    );
    assert!(
        out.contains("keychain"),
        "the credential storage kind is non-secret: {out}"
    );
    // The reusable key/binding never appears — only the storage KIND and the public endpoint.
    assert!(
        !out.contains("auth.json"),
        "no credential path may be printed: {out}"
    );
}

/// (3) A refused config is reported with its typed reason and a nonzero exit, never hidden.
#[test]
fn a_refused_config_reports_the_reason_and_exits_nonzero() {
    let scratch = Scratch::new("refused");
    let path = scratch.write(
        "WORKFLOW.md",
        "---\ntracker:\n  kind: file\n  source: /tmp/issues.json\nagent:\n  backend: nonsense\n---\nbody\n",
    );

    let (code, out, err) = run(&path);

    assert_eq!(code, 1, "a refused config must exit nonzero");
    assert!(
        out.contains("config: INVALID (unsupported_agent_backend"),
        "the report must carry the typed reason: {out}"
    );
    assert!(
        err.is_empty(),
        "the diagnostic report goes to stdout, not stderr: {err}"
    );
}
