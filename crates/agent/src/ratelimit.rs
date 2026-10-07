//! Account-limit observations (STUDIO-1123). Rhapsody-only; no Go counterpart.

use chrono::Utc;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitStatus {
    Allowed,
    Warning,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct WindowObs {
    pub window: String,
    pub utilization: f64,
    /// Epoch seconds; 0 means the provider supplied no reset hint.
    pub resets_at_s: i64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct LimitObs {
    pub status: LimitStatus,
    pub windows: Vec<WindowObs>,
    pub using_credits: bool,
    pub source: &'static str,
    pub observed_at_s: i64,
}

pub fn parse_claude_rate_limit(line: &[u8]) -> Option<LimitObs> {
    let event: Value = serde_json::from_slice(line).ok()?;
    if event.get("type")?.as_str()? != "rate_limit_event" {
        return None;
    }
    let info = event.get("rate_limit_info")?;
    let status = match info.get("status")?.as_str()? {
        "allowed" => LimitStatus::Allowed,
        "allowed_warning" => LimitStatus::Warning,
        "rejected" => LimitStatus::Rejected,
        _ => return None,
    };
    let mut windows = Vec::new();
    if let Some(unified) = info.get("unifiedWindows").and_then(Value::as_object) {
        for (name, window) in unified {
            if let Some(utilization) = window.get("utilization").and_then(valid_utilization) {
                windows.push(WindowObs {
                    window: name.clone(),
                    utilization,
                    resets_at_s: window
                        .get("resetsAt")
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
                        .max(0),
                });
            }
        }
    }
    if windows.is_empty() {
        let name = info.get("rateLimitType")?.as_str()?;
        let utilization = info
            .get("utilization")
            .and_then(valid_utilization)
            .or_else(|| (status == LimitStatus::Rejected).then_some(1.0))?;
        windows.push(WindowObs {
            window: name.to_string(),
            utilization,
            resets_at_s: info
                .get("resetsAt")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .max(0),
        });
    }
    Some(LimitObs {
        status,
        windows,
        using_credits: info
            .get("isUsingOverage")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        source: "stream",
        observed_at_s: Utc::now().timestamp(),
    })
}

/// Parse CLI rejection diagnostics only; callers must not pass ordinary assistant prose.
/// A local-time reset hint without a date/time-zone contract is kept unknown.
pub fn parse_claude_limit_error(text: &str) -> Option<LimitObs> {
    let text = text.trim().to_ascii_lowercase();
    if !text.starts_with("you've hit your limit")
        && !text.starts_with("you've hit your weekly limit")
    {
        return None;
    }
    Some(LimitObs {
        status: LimitStatus::Rejected,
        windows: vec![WindowObs {
            window: if text.starts_with("you've hit your weekly limit") {
                "seven_day"
            } else {
                "five_hour"
            }
            .into(),
            utilization: 1.0,
            resets_at_s: 0,
        }],
        using_credits: false,
        source: "stream",
        observed_at_s: Utc::now().timestamp(),
    })
}

/// v1.18.30 forwards provider headers/body in APIError.data, including stream errors
/// without an HTTP status. This parser never treats model prose or a 401 as a limit.
pub fn parse_opencode_limit(event: &Value) -> Option<LimitObs> {
    if event.get("type")?.as_str()? != "error" {
        return None;
    }
    let data = event.get("error")?.get("data")?;
    if data.get("statusCode").and_then(Value::as_i64) == Some(401) {
        return None;
    }
    let body = data
        .get("responseBody")
        .and_then(Value::as_str)
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let error = body.as_ref().and_then(|b| b.get("error"));
    let code = error.and_then(|e| e.get("code")).and_then(Value::as_str);
    if data.get("statusCode").and_then(Value::as_i64) != Some(429)
        && code != Some("usage_limit_reached")
    {
        return None;
    }
    let observed_at_s = event
        .get("timestamp")
        .and_then(Value::as_i64)
        .filter(|t| *t > 0)
        .map_or_else(|| Utc::now().timestamp(), |ms| ms / 1000);
    let mut windows = Vec::new();
    if let Some(headers) = data.get("responseHeaders").and_then(Value::as_object) {
        for window in ["primary", "secondary"] {
            let get = |suffix: &str| -> Option<f64> {
                let key = format!("x-codex-{window}-{suffix}");
                headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(&key))
                    .and_then(|(_, value)| value.as_str()?.parse::<f64>().ok())
                    .filter(|n| n.is_finite())
            };
            if let Some(percent) = get("used-percent").filter(|n| (0.0..=100.0).contains(n)) {
                let reset = get("reset-at")
                    .filter(|n| *n > 0.0 && *n < i64::MAX as f64)
                    .map(|n| n as i64)
                    .or_else(|| {
                        get("reset-after-seconds")
                            .filter(|n| *n >= 0.0 && *n <= i32::MAX as f64)
                            .map(|seconds| observed_at_s.saturating_add(seconds.ceil() as i64))
                    })
                    .unwrap_or(0);
                windows.push(WindowObs {
                    window: window.into(),
                    utilization: percent / 100.0,
                    resets_at_s: reset,
                });
            }
        }
    }
    if windows.is_empty() {
        let reset = error
            .and_then(|e| e.get("resets_at"))
            .and_then(Value::as_i64)
            .filter(|n| *n > 0)
            .unwrap_or(0);
        windows.push(WindowObs {
            window: "primary".into(),
            utilization: 1.0,
            resets_at_s: reset,
        });
    }
    Some(LimitObs {
        status: LimitStatus::Rejected,
        windows,
        using_credits: false,
        source: "stream",
        observed_at_s,
    })
}

fn valid_utilization(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|n| n.is_finite() && (0.0..=1.0).contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ALLOWED: &[u8] = include_bytes!("../testdata/limits/allowed.jsonl");
    const WARNING: &[u8] = include_bytes!("../testdata/limits/warning.jsonl");
    const REJECTED: &[u8] = include_bytes!("../testdata/limits/rejected.jsonl");

    #[test]
    fn parse_real_rate_limit_fixture() {
        for (line, status, credits, five, seven) in [
            (ALLOWED, LimitStatus::Allowed, false, 0.05, 0.02),
            (WARNING, LimitStatus::Warning, false, 0.26, 0.88),
            (REJECTED, LimitStatus::Rejected, true, 1.0, 0.19),
        ] {
            let obs = parse_claude_rate_limit(line).expect("real provider observation");
            assert_eq!(obs.status, status);
            assert_eq!(obs.using_credits, credits);
            assert_eq!(obs.source, "stream");
            assert_eq!(obs.windows.len(), 2);
            assert_eq!(
                obs.windows
                    .iter()
                    .find(|w| w.window == "five_hour")
                    .unwrap()
                    .utilization,
                five
            );
            assert_eq!(
                obs.windows
                    .iter()
                    .find(|w| w.window == "seven_day")
                    .unwrap()
                    .utilization,
                seven
            );
        }
    }

    #[test]
    fn resets_at_is_epoch_seconds() {
        let obs = parse_claude_rate_limit(ALLOWED).expect("observation");
        let five = obs
            .windows
            .iter()
            .find(|w| w.window == "five_hour")
            .unwrap();
        assert_eq!(five.resets_at_s, 1_791_312_600);
        let time = chrono::DateTime::from_timestamp(five.resets_at_s, 0).unwrap();
        assert_eq!(
            time.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "2026-10-06T18:50:00Z"
        );
        assert_eq!(
            time.with_timezone(&chrono::Local).timestamp(),
            five.resets_at_s
        );
    }

    #[test]
    fn claude_limit_error_is_rejected() {
        for text in [
            "You've hit your limit · resets 3pm (Europe/London)",
            "You've hit your weekly limit",
        ] {
            let obs = parse_claude_limit_error(text).expect("CLI rejection");
            assert_eq!(obs.status, LimitStatus::Rejected);
            assert!(!obs.using_credits);
            assert_eq!(
                obs.windows[0].resets_at_s, 0,
                "ambiguous local time is not a fabricated epoch"
            );
        }
        assert!(parse_claude_limit_error("API Error: 401 invalid credentials").is_none());
        assert!(parse_claude_limit_error("rate limit documentation").is_none());
    }

    // Source-shaped boundary cases, NOT a recorded 429 (see testdata/limits/README.md).
    #[test]
    fn opencode_limit_shapes() {
        let http = json!({"type":"error", "timestamp":1791312000123i64,
            "error":{"name":"APIError", "data":{"statusCode":429,
                "responseBody":"{\"error\":{\"code\":\"usage_limit_reached\",\"resets_at\":1791312600}}"}}});
        let obs = parse_opencode_limit(&http).expect("HTTP limit");
        assert_eq!(obs.status, LimitStatus::Rejected);
        assert_eq!(obs.observed_at_s, 1_791_312_000);
        assert_eq!(obs.windows[0].resets_at_s, 1_791_312_600);
        let stream = json!({"type":"error", "error":{"name":"APIError", "data":{
            "responseBody":"{\"type\":\"error\",\"error\":{\"code\":\"usage_limit_reached\"}}"}}});
        assert_eq!(
            parse_opencode_limit(&stream).unwrap().windows[0].resets_at_s,
            0
        );
        assert!(
            parse_opencode_limit(&json!({"type":"text","part":{"text":"usage_limit_reached"}}))
                .is_none()
        );
        assert!(
            parse_opencode_limit(&json!({"type":"error","error":{"data":{"statusCode":401}}}))
                .is_none()
        );
    }

    #[test]
    fn opencode_error_headers_preserve_both_windows_and_relative_resets() {
        let event = json!({"type":"error", "timestamp":1791312000123i64, "error":{"data":{
            "statusCode":429, "responseHeaders":{
                "X-Codex-Primary-Used-Percent":"100", "x-codex-primary-reset-after-seconds":"600",
                "x-codex-secondary-used-percent":"55", "x-codex-secondary-reset-after-seconds":"86400"}}}});
        let obs = parse_opencode_limit(&event).unwrap();
        assert_eq!(obs.windows.len(), 2);
        assert_eq!(
            (obs.windows[0].utilization, obs.windows[0].resets_at_s),
            (1.0, 1791312600)
        );
        assert_eq!(
            (obs.windows[1].utilization, obs.windows[1].resets_at_s),
            (0.55, 1791398400)
        );
    }

    #[test]
    fn opencode_absolute_reset_at_headers_are_epoch_seconds() {
        let event = json!({"type":"error", "timestamp":1791312000123i64, "error":{"data":{
            "statusCode":429, "responseHeaders":{
                "X-Codex-Primary-Used-Percent":"100", "x-codex-primary-reset-at":"1791312600",
                "x-codex-secondary-used-percent":"55", "x-codex-secondary-reset-at":"1791398400"}}}});
        let obs = parse_opencode_limit(&event).unwrap();
        assert_eq!(obs.windows[0].resets_at_s, 1791312600);
        assert_eq!(obs.windows[1].resets_at_s, 1791398400);
    }

    #[test]
    fn malformed_or_unknown_claude_events_are_not_healthy_observations() {
        for line in [br#"{"type":"rate_limit_event","rate_limit_info":{"status":"unknown"}}"#.as_slice(),
            br#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour","utilization":2}}"#,
            b"not json"] {
            assert!(parse_claude_rate_limit(line).is_none());
        }
    }
}
