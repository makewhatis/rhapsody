//! Deterministic landing checks; standalone runners with no scheduling side effects.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use rhapsody_config::landing::{CheckSpec, CommandCheck, HttpCheck};
use tokio::io::{AsyncRead, AsyncReadExt};

pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const OBSERVED_LIMIT: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    pub pass: bool,
    pub observed: String,
    pub duration: Duration,
}

/// Runs one deterministic check. The caller supplies env-file contents and a merged worktree.
pub async fn run_check(
    spec: &CheckSpec,
    worktree: &Path,
    env: &[(String, String)],
    timeout: Duration,
) -> CheckOutcome {
    let started = Instant::now();
    let result = execute(spec, worktree, env, timeout).await;
    let duration = started.elapsed();
    CheckOutcome {
        pass: result.pass,
        observed: evidence(
            &format!("{}; duration={}ms", result.observed, duration.as_millis()),
            env,
        ),
        duration,
    }
}

/// Returns the probe's trimmed stdout/HTTP body, never a status summary or a truncated revision.
pub async fn run_live_probe(
    spec: &CheckSpec,
    worktree: &Path,
    env: &[(String, String)],
    timeout: Duration,
) -> Result<String, String> {
    let result = execute(spec, worktree, env, timeout).await;
    if !result.pass {
        return Err(evidence(&result.observed, env));
    }
    let revision = result
        .revision
        .ok_or_else(|| "live probe output exceeds 2 KiB or is not UTF-8".to_string())?;
    let revision = revision.trim();
    if revision.is_empty() {
        return Err("live probe returned no revision".into());
    }
    if env
        .iter()
        .any(|(_, value)| !value.is_empty() && revision.contains(value))
    {
        return Err("live probe returned an env value instead of a revision".into());
    }
    Ok(revision.into())
}

struct ResultData {
    pass: bool,
    observed: String,
    revision: Option<String>,
}

impl ResultData {
    fn failed(message: impl Into<String>) -> Self {
        Self {
            pass: false,
            observed: message.into(),
            revision: None,
        }
    }
}

async fn execute(
    spec: &CheckSpec,
    worktree: &Path,
    env: &[(String, String)],
    timeout: Duration,
) -> ResultData {
    match spec {
        CheckSpec::Http(h) => {
            match tokio::time::timeout(timeout, http(h, capture_limit(env))).await {
                Ok(Ok(result)) => result,
                Ok(Err(e)) => ResultData::failed(format!("http error: {}", e.without_url())),
                Err(_) => ResultData::failed("http timeout"),
            }
        }
        CheckSpec::Command(c) => {
            command(
                c,
                worktree,
                env,
                if timeout.is_zero() {
                    DEFAULT_COMMAND_TIMEOUT
                } else {
                    timeout
                },
            )
            .await
        }
    }
}

// Keep enough lookahead to redact a value that straddles the eventual evidence boundary.
fn capture_limit(env: &[(String, String)]) -> usize {
    OBSERVED_LIMIT
        .saturating_add(env.iter().map(|(_, v)| v.len()).max().unwrap_or(0))
        .saturating_add(4)
}

fn evidence(text: &str, env: &[(String, String)]) -> String {
    // Longest first prevents an overlapping shorter secret from leaving a longer one's suffix.
    let mut values: Vec<_> = env
        .iter()
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
        .collect();
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    let mut text = text.to_string();
    for value in values {
        text = text.replace(value, "[redacted]");
    }
    let mut end = text.len().min(OBSERVED_LIMIT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    // Even the marker/static evidence can coincide with an env value. In that rare case
    // suppress evidence rather than publishing the value again through the replacement.
    if env
        .iter()
        .any(|(_, value)| !value.is_empty() && text.contains(value))
    {
        text.clear();
    }
    text
}

async fn http(spec: &HttpCheck, limit: usize) -> Result<ResultData, reqwest::Error> {
    let client = reqwest::Client::builder().build()?;
    let mut response = client.get(&spec.url).send().await?;
    let status = response.status().as_u16();
    let mut pass = status == spec.status;
    let mut observed = format!("status={status}");
    for (name, expected) in &spec.headers {
        let actual = response.headers().get(name);
        pass &=
            actual.is_some_and(|v| expected == "present" || v.as_bytes() == expected.as_bytes());
        let value = actual.map(|v| String::from_utf8_lossy(v.as_bytes()));
        observed.push_str(&format!(
            "; {name}={}",
            value.as_deref().unwrap_or("missing")
        ));
    }
    let needle = spec.body_contains.as_deref().unwrap_or("").as_bytes();
    let mut found = needle.is_empty();
    let mut tail = Vec::new();
    let mut body = Vec::new();
    let mut oversized = false;
    while let Some(chunk) = response.chunk().await? {
        if !found {
            tail.extend_from_slice(&chunk);
            found = tail.windows(needle.len()).any(|w| w == needle);
            let keep = needle.len().saturating_sub(1).min(tail.len());
            tail.drain(..tail.len() - keep);
        }
        let available = limit.saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(available)]);
        oversized |= chunk.len() > available;
    }
    pass &= found;
    if spec.body_contains.is_some() {
        observed.push_str(&format!("; body_contains={found}"));
    }
    let revision = if oversized || body.len() > OBSERVED_LIMIT {
        None
    } else {
        String::from_utf8(body).ok()
    };
    Ok(ResultData {
        pass,
        observed,
        revision,
    })
}

struct KillGroup(i32);
impl Drop for KillGroup {
    fn drop(&mut self) {
        // Child is put in a new process group before exec; retain its PGID even after leader exit.
        let result = unsafe { libc::kill(-self.0, libc::SIGKILL) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                tracing::warn!(pgid = self.0, %error, "landing command group cleanup failed");
            }
        }
    }
}

async fn command(
    spec: &CommandCheck,
    worktree: &Path,
    env: &[(String, String)],
    timeout: Duration,
) -> ResultData {
    let Some(executable) = spec.argv.first().filter(|s| !s.is_empty()) else {
        return ResultData::failed("command has no executable");
    };
    let mut cmd = tokio::process::Command::new(executable);
    cmd.args(&spec.argv[1..])
        .current_dir(worktree)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => return ResultData::failed(format!("command spawn error: {error}")),
    };
    let Some(pid) = child.id().and_then(|p| i32::try_from(p).ok()) else {
        return ResultData::failed("command has no process id");
    };
    let group = KillGroup(pid);
    let Some(stdout) = child.stdout.take() else {
        return ResultData::failed("command has no stdout pipe");
    };
    let result = tokio::time::timeout(timeout, async {
        tokio::try_join!(
            child.wait(),
            drain(stdout, spec.expect_stdout.as_deref(), capture_limit(env))
        )
    })
    .await;
    // Covers successful commands that left background descendants as well as failure/cancellation.
    drop(group);
    match result {
        Ok(Ok((status, output))) => ResultData {
            pass: status.success() && output.matches(),
            observed: format!(
                "exit={}; stdout={}",
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into()),
                output.prefix.trim_end()
            ),
            revision: (!output.truncated && output.prefix.trim_end().len() <= OBSERVED_LIMIT)
                .then(|| output.prefix.trim_end().to_string()),
        },
        other => {
            // kill_on_drop is a backstop; explicitly reap the leader after group cleanup.
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            match other {
                Err(_) => ResultData::failed("command timeout"),
                Ok(Err(error)) => ResultData::failed(format!("command read/wait error: {error}")),
                Ok(Ok(_)) => ResultData::failed("command failed"),
            }
        }
    }
}

// Compare the entire trimmed stdout while retaining only a bounded evidence prefix. Whitespace
// after the last non-whitespace char is provisional: it matters only if another non-space follows.
struct Output<'a> {
    expected: Option<&'a str>,
    index: Option<usize>,
    committed: Option<usize>,
    started: bool,
    prefix: String,
    limit: usize,
    truncated: bool,
}
impl<'a> Output<'a> {
    fn new(expected: Option<&'a str>, limit: usize) -> Self {
        Self {
            expected,
            index: Some(0),
            committed: Some(0),
            started: false,
            prefix: String::new(),
            limit,
            truncated: false,
        }
    }
    fn push(&mut self, text: &str) {
        for ch in text.chars() {
            if !self.started && ch.is_whitespace() {
                continue;
            }
            self.started = true;
            if let Some(expected) = self.expected {
                self.index = self.index.and_then(|i| {
                    expected
                        .get(i..)
                        .filter(|s| s.starts_with(ch))
                        .map(|_| i + ch.len_utf8())
                });
                if !ch.is_whitespace() {
                    self.committed = self.index;
                }
            }
            if self.prefix.len() + ch.len_utf8() <= self.limit && !self.truncated {
                self.prefix.push(ch);
            } else {
                self.truncated = true;
            }
        }
    }
    fn matches(&self) -> bool {
        self.expected
            .is_none_or(|s| self.committed == Some(s.len()))
    }
}

async fn drain<'a>(
    mut reader: impl AsyncRead + Unpin,
    expected: Option<&'a str>,
    limit: usize,
) -> std::io::Result<Output<'a>> {
    let mut output = Output::new(expected, limit);
    let mut pending = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let n = reader.read(&mut buffer).await?;
        if n == 0 {
            output.push(&String::from_utf8_lossy(&pending));
            break;
        }
        pending.extend_from_slice(&buffer[..n]);
        loop {
            match std::str::from_utf8(&pending) {
                Ok(text) => {
                    output.push(text);
                    pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    output.push(&String::from_utf8_lossy(&pending[..valid]));
                    pending.drain(..valid);
                    if let Some(invalid) = error.error_len() {
                        output.push("\u{fffd}");
                        pending.drain(..invalid);
                    } else {
                        break;
                    }
                }
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_config::landing::{CommandCheck, HttpCheck};
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Behavior checks need scheduling headroom; timeout tests supply their own short deadlines.
    const BEHAVIOR_TIMEOUT: Duration = Duration::from_secs(30);

    struct Server {
        url: String,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Server {
        fn new(status: u16, headers: &str, body: &str, delay: Duration) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            let response = format!(
                "HTTP/1.1 {status} Test\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let thread = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(BEHAVIOR_TIMEOUT)).unwrap();
                let mut request = [0u8; 4096];
                let _ = socket.read(&mut request);
                std::thread::sleep(delay);
                let _ = socket.write_all(response.as_bytes());
            });
            Self {
                url,
                thread: Some(thread),
            }
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }
    fn http(url: &str) -> CheckSpec {
        CheckSpec::Http(HttpCheck {
            url: url.into(),
            status: 200,
            headers: BTreeMap::from([("x-ready".into(), "present".into())]),
            body_contains: Some("healthy".into()),
        })
    }
    fn command(script: &str, expected: Option<&str>) -> CheckSpec {
        CheckSpec::Command(CommandCheck {
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            expect_stdout: expected.map(str::to_string),
        })
    }
    async fn check_behavior(spec: &CheckSpec) -> CheckOutcome {
        run_check(spec, Path::new("."), &[], BEHAVIOR_TIMEOUT).await
    }

    #[tokio::test]
    async fn http_check_passes_on_status_header_present_and_body_contains() {
        let server = Server::new(200, "X-Ready: yes\r\n", "healthy app", Duration::ZERO);
        let result = check_behavior(&http(&server.url)).await;
        assert!(result.pass, "{}", result.observed);
        assert!(result.observed.contains("200") && result.observed.contains("x-ready"));
        assert!(result.observed.contains("yes"));
    }

    #[tokio::test]
    async fn http_header_present_does_not_require_an_ascii_value() {
        let server = Server::new(200, "X-Ready: é\r\n", "healthy", Duration::ZERO);
        assert!(check_behavior(&http(&server.url)).await.pass);
    }
    #[tokio::test]
    async fn http_check_fails_on_wrong_status() {
        let server = Server::new(503, "X-Ready: yes\r\n", "healthy", Duration::ZERO);
        let result = check_behavior(&http(&server.url)).await;
        assert!(!result.pass);
        assert!(result.observed.contains("503"));
    }
    #[tokio::test]
    async fn http_check_header_exact_value_mismatch_fails() {
        for value in ["wrong", "ready"] {
            let server = Server::new(
                200,
                &format!("X-Ready: {value}\r\n"),
                "healthy",
                Duration::ZERO,
            );
            let mut spec = http(&server.url);
            if let CheckSpec::Http(h) = &mut spec {
                h.headers.insert("x-ready".into(), "ready".into());
            }
            assert_eq!(check_behavior(&spec).await.pass, value == "ready");
        }
    }
    #[tokio::test]
    async fn http_check_fails_on_missing_header_or_body_substring() {
        for (headers, body) in [("", "healthy"), ("X-Ready: yes\r\n", "broken")] {
            let server = Server::new(200, headers, body, Duration::ZERO);
            assert!(!check_behavior(&http(&server.url)).await.pass);
        }
    }
    #[tokio::test]
    async fn http_check_times_out() {
        let server = Server::new(
            200,
            "X-Ready: yes\r\n",
            "healthy",
            Duration::from_millis(150),
        );
        let result = run_check(
            &http(&server.url),
            Path::new("."),
            &[],
            Duration::from_millis(30),
        )
        .await;
        assert!(!result.pass);
        assert!(result.observed.contains("timeout"), "{}", result.observed);
    }
    #[tokio::test]
    async fn command_check_exit_zero_passes() {
        let result = check_behavior(&command("printf 'ok'", None)).await;
        assert!(result.pass);
        assert!(result.observed.contains("ok"));
    }
    #[tokio::test]
    async fn command_nonzero_fails_with_exit_code_in_observed() {
        let result = check_behavior(&command("printf 'broken'; exit 7", None)).await;
        assert!(!result.pass);
        assert!(result.observed.contains("7") && result.observed.contains("broken"));
    }
    #[tokio::test]
    async fn command_expect_stdout_trims_and_compares_exactly() {
        assert!(
            check_behavior(&command("printf '  8|f\n\t'", Some("8|f")))
                .await
                .pass
        );
        assert!(
            !check_behavior(&command("printf '8|false'", Some("8|f")))
                .await
                .pass
        );
        assert!(
            !check_behavior(&command("printf '8|f'", Some(" 8|f")))
                .await
                .pass
        );
        assert!(
            !check_behavior(&command("printf '8|f'; exit 1", Some("8|f")))
                .await
                .pass
        );
        // A matching evidence prefix is not a match for the complete stdout.
        let expected = "x".repeat(4096);
        assert!(
            check_behavior(&command("perl -e 'print \"x\" x 4096'", Some(&expected)))
                .await
                .pass
        );
        assert!(
            !check_behavior(&command(
                "perl -e 'print \"x\" x 4096, \"y\"'",
                Some(&expected)
            ))
            .await
            .pass
        );
    }
    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "rhapsody-landingcheck-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[tokio::test]
    async fn command_timeout_kills_process_group() {
        let dir = Scratch::new();
        // The leader exits but the grandchild retains stdout; timeout must still kill the group.
        let spec = command("(sleep 0.4; printf survived > escaped) & exit 0", None);
        let result = run_check(&spec, &dir.0, &[], Duration::from_millis(80)).await;
        assert!(!result.pass);
        assert!(result.observed.contains("timeout"));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !dir.0.join("escaped").exists(),
            "descendant survived the group kill"
        );
        let result = run_check(
            &command("sleep 5", None),
            &dir.0,
            &[],
            Duration::from_millis(30),
        )
        .await;
        assert!(!result.pass);
        assert!(result.observed.contains("timeout"));
    }
    #[tokio::test]
    async fn command_output_capped_at_2kib() {
        let result = check_behavior(&command("perl -e 'print \"é\" x 8192'", None)).await;
        assert!(result.pass, "{}", result.observed);
        assert!(result.observed.len() <= 2048);
        assert!(result.observed.contains('é'));
        // stderr cannot fill its pipe and deadlock the check.
        assert!(
            check_behavior(&command(
                "perl -e 'print STDERR \"err\" x 20000; print \"ok\"'",
                Some("ok")
            ))
            .await
            .pass
        );
    }
    #[tokio::test]
    async fn command_receives_env_file_vars_but_observed_never_contains_them() {
        let secret = "landing-private-dsn";
        let env = vec![("LANDING_DSN".into(), secret.into())];
        let spec = command("printf '  %s\n' \"$LANDING_DSN\"", Some(secret));
        let result = run_check(&spec, Path::new("."), &env, BEHAVIOR_TIMEOUT).await;
        assert!(result.pass);
        assert!(!result.observed.contains(secret), "{}", result.observed);
        assert!(result.observed.contains("[redacted]"));
        let spec = command(
            "perl -e 'print \"x\" x 2010'; printf '%s' \"$LANDING_DSN\"",
            None,
        );
        let result = run_check(&spec, Path::new("."), &env, BEHAVIOR_TIMEOUT).await;
        assert!(result.pass);
        assert!(!result.observed.contains(secret));
        assert!(!result.observed.ends_with("landing-private"));
    }
    #[tokio::test]
    async fn live_probe_returns_revision_and_rejects_failed_or_empty_probes() {
        let revision = "a".repeat(40);
        assert_eq!(
            run_live_probe(
                &command(&format!("printf '  {revision}\n'"), None),
                Path::new("."),
                &[],
                BEHAVIOR_TIMEOUT
            )
            .await
            .unwrap(),
            revision
        );
        let server = Server::new(200, "", &format!("{revision}\n"), Duration::ZERO);
        let spec = CheckSpec::Http(HttpCheck {
            url: server.url.clone(),
            status: 200,
            headers: BTreeMap::new(),
            body_contains: None,
        });
        assert_eq!(
            run_live_probe(&spec, Path::new("."), &[], BEHAVIOR_TIMEOUT)
                .await
                .unwrap(),
            revision
        );
        for script in ["exit 7", "printf '  '"] {
            assert!(
                run_live_probe(
                    &command(script, None),
                    Path::new("."),
                    &[],
                    BEHAVIOR_TIMEOUT
                )
                .await
                .is_err()
            );
        }
        assert!(
            run_live_probe(
                &command("sleep 5", None),
                Path::new("."),
                &[],
                Duration::from_millis(50)
            )
            .await
            .is_err()
        );
    }
    #[tokio::test]
    async fn errors_and_http_evidence_do_not_expose_env_values() {
        let secret = "landing-secret-error";
        let env = vec![("TOKEN".into(), secret.into())];
        let server = Server::new(
            200,
            &format!("X-Ready: {secret}\r\n"),
            "healthy",
            Duration::ZERO,
        );
        let result = run_check(&http(&server.url), Path::new("."), &env, BEHAVIOR_TIMEOUT).await;
        assert!(result.pass);
        assert!(!result.observed.contains(secret));
        let spec = CheckSpec::Command(CommandCheck {
            argv: vec![format!("/nonexistent/{secret}")],
            expect_stdout: None,
        });
        assert!(
            !run_check(&spec, Path::new("."), &env, BEHAVIOR_TIMEOUT)
                .await
                .observed
                .contains(secret)
        );
    }

    #[test]
    fn redaction_never_reintroduces_an_env_value_in_its_marker() {
        for value in ["redacted", "[redacted]", "e", "200"] {
            let observed = evidence(
                &format!("status=200; stdout={value}"),
                &[("TOKEN".into(), value.into())],
            );
            assert!(!observed.contains(value), "{value}: {observed}");
        }
    }

    #[tokio::test]
    async fn streaming_stdout_preserves_unicode_trim_and_internal_whitespace() {
        for (output, expected, pass) in [
            ("\u{2003}héllo\n\t", "héllo", true),
            ("\n8 | f\t", "8 | f", true),
            ("\n8 | f\t", "8|f", false),
            ("\n\t", "", true),
        ] {
            let (mut writer, reader) = tokio::io::duplex(8);
            let producer = async {
                use tokio::io::AsyncWriteExt;
                for byte in output.as_bytes() {
                    writer.write_all(&[*byte]).await.unwrap();
                }
                drop(writer);
            };
            let (_, output) = tokio::join!(producer, drain(reader, Some(expected), 2048));
            assert_eq!(output.unwrap().matches(), pass);
        }
    }

    #[tokio::test]
    async fn command_runs_relative_argv_in_the_supplied_worktree() {
        let dir = Scratch::new();
        std::fs::write(dir.0.join("revision"), "worktree-revision\n").unwrap();
        let spec = CheckSpec::Command(CommandCheck {
            argv: vec!["/bin/cat".into(), "revision".into()],
            expect_stdout: Some("worktree-revision".into()),
        });
        assert!(run_check(&spec, &dir.0, &[], BEHAVIOR_TIMEOUT).await.pass);
    }
}
