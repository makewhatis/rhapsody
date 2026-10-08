//! Passive ChatGPT account usage (STUDIO-1123). No inference, refresh, or Go counterpart.

use crate::ratelimit::{LimitObs, LimitStatus, WindowObs};
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

pub fn parse_usage(bytes: &[u8], now_s: i64) -> Option<LimitObs> {
    let doc: Value = serde_json::from_slice(bytes).ok()?;
    let limits = doc.get("rate_limit")?;
    let allowed = limits.get("allowed")?.as_bool()?;
    let reached = limits.get("limit_reached")?.as_bool()?;
    let mut windows = Vec::new();
    for name in ["primary", "secondary"] {
        let Some(window) = limits
            .get(format!("{name}_window"))
            .filter(|w| !w.is_null())
        else {
            continue;
        };
        let percent = window.get("used_percent")?.as_f64()?;
        if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
            return None;
        }
        windows.push(WindowObs {
            // Match the stream headers/errors' slot names, even when this account's primary
            // happens to be weekly. A new probe reset must retire the same window's old wall.
            window: name.into(),
            utilization: percent / 100.0,
            resets_at_s: window.get("reset_at")?.as_i64()?.max(0),
        });
    }
    if windows.is_empty() {
        return None;
    }
    Some(LimitObs {
        status: if !allowed || reached {
            LimitStatus::Rejected
        } else {
            LimitStatus::Allowed
        },
        windows,
        using_credits: false,
        source: "probe",
        observed_at_s: now_s,
    })
}

#[derive(Default)]
pub struct ProbeGate {
    last_attempt_s: Mutex<Option<i64>>,
}
impl ProbeGate {
    pub fn claim(&self, now_s: i64, active: bool) -> bool {
        if !active {
            return false;
        }
        let mut last = self
            .last_attempt_s
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|at| now_s.saturating_sub(at) < 600) {
            return false;
        }
        *last = Some(now_s);
        true
    }
}

/// Reads only the selected provider's kind from the already provisioned session login.
/// Malformed/missing auth stays unknown; parser errors never quote credential bytes.
pub(crate) fn auth_kind(path: &Path, provider: &str) -> Option<bool> {
    let bytes = std::fs::read(path).ok()?;
    let doc: Value = serde_json::from_slice(&bytes).ok()?;
    match doc.get(provider)?.get("type")?.as_str()? {
        "oauth" => Some(true),
        "api" => Some(false),
        _ => None,
    }
}

struct AccessOnly {
    access: String,
    account: Option<String>,
}

fn access_only(path: &Path, now_s: i64) -> Result<AccessOnly, &'static str> {
    let bytes = std::fs::read(path).map_err(|_| "login_unreadable")?;
    let mut doc: Value = serde_json::from_slice(&bytes).map_err(|_| "login_malformed")?;
    let login = doc
        .get_mut("openai")
        .and_then(Value::as_object_mut)
        .ok_or("login_missing")?;
    // The probe's in-memory copy is explicitly refresh-blank. Nothing is written back or refreshed.
    login.insert("refresh".into(), Value::String(String::new()));
    if login.get("type").and_then(Value::as_str) != Some("oauth") {
        return Err("login_not_oauth");
    }
    if login
        .get("expires")
        .and_then(Value::as_i64)
        .is_some_and(|ms| ms / 1000 <= now_s)
    {
        return Err("login_expired");
    }
    let access = login
        .get("access")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("access_missing")?
        .to_string();
    let account = login
        .get("accountId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok(AccessOnly { access, account })
}

async fn fetch(
    endpoint: &str,
    access: AccessOnly,
    timeout: std::time::Duration,
) -> Result<LimitObs, &'static str> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .timeout(timeout)
        .build()
        .map_err(|_| "client_unavailable")?;
    let mut req = client
        .get(endpoint)
        .bearer_auth(&access.access)
        .header("Accept", "application/json")
        .header("Accept-Encoding", "identity");
    if let Some(account) = access.account {
        req = req.header("ChatGPT-Account-Id", account);
    }
    let mut response = req.send().await.map_err(|e| {
        if e.is_timeout() {
            "usage_timeout"
        } else {
            "usage_unavailable"
        }
    })?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("usage_unauthorized");
    }
    if !response.status().is_success() {
        return Err("usage_refused");
    }
    const MAX_BODY: usize = 64 * 1024;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| {
        if e.is_timeout() {
            "usage_timeout"
        } else {
            "usage_unavailable"
        }
    })? {
        if bytes.len().saturating_add(chunk.len()) > MAX_BODY {
            return Err("usage_oversized");
        }
        bytes.extend_from_slice(&chunk);
    }
    parse_usage(&bytes, chrono::Utc::now().timestamp()).ok_or("usage_unknown")
}

pub struct UsageProbe {
    gate: ProbeGate,
    endpoint: String,
    monotonic_origin: std::time::Instant,
}

pub fn shared_probe() -> Arc<UsageProbe> {
    static PROBE: OnceLock<Arc<UsageProbe>> = OnceLock::new();
    Arc::clone(PROBE.get_or_init(|| {
        #[cfg(not(test))]
        let endpoint = "https://chatgpt.com/backend-api/wham/usage";
        // Unit runner tests never contact a provider. A runner test below injects a real
        // loopback transport and exercises this same admission/credential/fetch path.
        #[cfg(test)]
        let endpoint = "http://127.0.0.1:9/usage";
        Arc::new(UsageProbe {
            gate: ProbeGate::default(),
            endpoint: endpoint.into(),
            monotonic_origin: std::time::Instant::now(),
        })
    }))
}

impl UsageProbe {
    /// Endpoint injection for hermetic daemon transport tests; production uses `shared_probe`.
    pub fn new(endpoint: String) -> Self {
        Self {
            gate: ProbeGate::default(),
            endpoint,
            monotonic_origin: std::time::Instant::now(),
        }
    }

    /// Called by the daemon at boot and while OpenAI work is active or queued. Admission is shared,
    /// and claimed before credential or network I/O, including failed attempts.
    pub async fn observe(&self, path: &Path) -> Option<Result<LimitObs, &'static str>> {
        let now_s = chrono::Utc::now().timestamp();
        // A wall-clock adjustment must not permit a second request inside ten real minutes.
        let elapsed_s =
            i64::try_from(self.monotonic_origin.elapsed().as_secs()).unwrap_or(i64::MAX);
        if !self.gate.claim(elapsed_s, true) {
            return None;
        }
        let result = match access_only(path, now_s) {
            Ok(access) => fetch(&self.endpoint, access, std::time::Duration::from_secs(20)).await,
            Err(reason) => Err(reason),
        };
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ratelimit::LimitStatus;

    #[test]
    fn probe_reads_access_only_and_never_changes_source_or_exposes_parse_errors() {
        let dir = super::super::testdir::TempDir::new();
        let path = dir.path().join("auth.json");
        let original = br#"{"openai":{"type":"oauth","access":"test-access","refresh":"never-refresh-canary","accountId":"test-account","expires":4102444800000},"other":{"key":"never-use"}}"#;
        std::fs::write(&path, original).unwrap();
        let access = access_only(&path, 1000).unwrap();
        assert_eq!(access.access, "test-access");
        assert_eq!(access.account.as_deref(), Some("test-account"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(access_only(&path, 4102444800).err(), Some("login_expired"));
        std::fs::write(&path, b"never-refresh-canary").unwrap();
        assert_eq!(access_only(&path, 1).err(), Some("login_malformed"));
        assert!(auth_kind(&path, "openai").is_none());
    }

    async fn served(response: String) -> (Result<LimitObs, &'static str>, String) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8(request).unwrap()
        });
        let result = fetch(
            &format!("http://{addr}/usage"),
            AccessOnly {
                access: "test-access".into(),
                account: Some("test-account".into()),
            },
            std::time::Duration::from_secs(20),
        )
        .await;
        (result, server.join().unwrap())
    }

    #[tokio::test]
    async fn passive_transport_is_get_only_bounded_and_never_follows_redirects() {
        let body = include_str!("../../testdata/limits/chatgpt-usage.json");
        let (result, request) = served(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ))
        .await;
        assert_eq!(result.unwrap().windows[0].utilization, 0.31);
        let request = request.to_ascii_lowercase();
        assert!(request.starts_with("get /usage http/1.1\r\n"));
        assert!(request.contains("authorization: bearer test-access\r\n"));
        assert!(request.contains("chatgpt-account-id: test-account\r\n"));
        assert!(!request.contains("refresh"));
        assert!(!request.contains("reserve"));
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (result, _) = served(format!("HTTP/1.1 302 Found\r\nLocation: http://{}/leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", target.local_addr().unwrap())).await;
        assert_eq!(result.err(), Some("usage_refused"));
        target.set_nonblocking(true).unwrap();
        assert_eq!(
            target.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let body = "x".repeat(65537);
        let (result, _) = served(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ))
        .await;
        assert_eq!(result.err(), Some("usage_oversized"));
        let (result, _) = served(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        )
        .await;
        assert_eq!(result.err(), Some("usage_unauthorized"));
    }

    #[tokio::test]
    async fn timeout_is_a_closed_failure_and_concurrent_attempts_are_coalesced() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/usage", listener.local_addr().unwrap());
        let result = fetch(
            &endpoint,
            AccessOnly {
                access: "test-access".into(),
                account: None,
            },
            std::time::Duration::from_millis(20),
        )
        .await;
        assert_eq!(result.err(), Some("usage_timeout"));
        let dir = super::super::testdir::TempDir::new();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, b"malformed-secret-canary").unwrap();
        let probe = UsageProbe::new(endpoint);
        let (a, b) = tokio::join!(probe.observe(&path), probe.observe(&path));
        assert_eq!(a.unwrap().err(), Some("login_malformed"));
        assert!(b.is_none());
    }

    #[test]
    fn measured_chatgpt_usage_preserves_primary_window_and_epoch_seconds() {
        let obs = parse_usage(
            include_bytes!("../../testdata/limits/chatgpt-usage.json"),
            1791384678,
        )
        .unwrap();
        assert_eq!(obs.status, LimitStatus::Allowed);
        assert_eq!(obs.source, "probe");
        assert_eq!(obs.windows.len(), 1);
        assert_eq!(obs.windows[0].window, "primary");
        assert_eq!(obs.windows[0].utilization, 0.31);
        assert_eq!(obs.windows[0].resets_at_s, 1791948615);
        assert!(!obs.using_credits);
        assert!(parse_usage(b"{}", 1).is_none());
    }

    #[test]
    fn probe_admission_is_ten_minutes_across_concurrent_runs_and_failures() {
        let gate = ProbeGate::default();
        assert!(!gate.claim(1000, false));
        assert!(gate.claim(1000, true));
        assert!(!gate.claim(1000, true));
        assert!(!gate.claim(1599, true));
        assert!(gate.claim(1600, true));
        assert!(!gate.claim(2200, false));
        assert!(gate.claim(2200, true));
        assert!(!gate.claim(2199, true));
    }
}
