//! OTLP endpoint helpers: scheme detection and per-signal HTTP path defaulting. Mirrors the
//! endpoint functions in Go `telemetry.go`.

/// Per-signal default OTLP/HTTP request paths — the OTel SDK defaults, filled in by
/// [`endpoint_url_for_signal`] when a configured endpoint URL carries no path of its own.
pub const DEFAULT_LOGS_PATH: &str = "/v1/logs";
pub const DEFAULT_TRACES_PATH: &str = "/v1/traces";
pub const DEFAULT_METRICS_PATH: &str = "/v1/metrics";

/// Reports whether `ep` carries an explicit http/https scheme, in which case the SDK's
/// endpoint-URL setter (not the bare `host:port` setter) must be used. Mirrors Go `endpointIsURL`.
pub fn endpoint_is_url(ep: &str) -> bool {
    ep.starts_with("http://") || ep.starts_with("https://")
}

/// Returns `raw_url` with the per-signal default OTLP path filled in when it carries no path (empty
/// or `/`). A URL that already carries a path is returned unchanged so an operator-supplied path is
/// honored; an unparseable `raw_url` is returned unchanged so the SDK surfaces the parse error.
/// Only the HTTP exporters call this — gRPC has no URL path. Mirrors Go `endpointURLForSignal` (the
/// path-less-URL 404 fix).
pub fn endpoint_url_for_signal(raw_url: &str, default_path: &str) -> String {
    match url::Url::parse(raw_url) {
        // Explicit, non-root path: honor it.
        Ok(u) if !u.path().is_empty() && u.path() != "/" => raw_url.to_string(),
        Ok(mut u) => {
            u.set_path(default_path);
            u.to_string()
        }
        // Unparseable: return unchanged so the SDK surfaces the parse error.
        Err(_) => raw_url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors Go `TestEndpointIsURL`.
    #[test]
    fn endpoint_is_url_cases() {
        let cases = [
            ("http://localhost:4318", true),
            ("https://otel.example.com", true),
            ("otel.example.com:4317", false),
            ("127.0.0.1:4317", false),
            ("", false),
        ];
        for (ep, want) in cases {
            assert_eq!(endpoint_is_url(ep), want, "endpoint_is_url({ep:?})");
        }
    }

    // Mirrors Go `TestEndpointURLForSignal`.
    #[test]
    fn endpoint_url_for_signal_cases() {
        let cases = [
            (
                "path-less https gets default",
                "https://collector.example:4317",
                DEFAULT_LOGS_PATH,
                "https://collector.example:4317/v1/logs",
            ),
            (
                "root-only path gets default",
                "https://host/",
                DEFAULT_TRACES_PATH,
                "https://host/v1/traces",
            ),
            (
                "explicit path preserved",
                "https://host/custom/otlp",
                DEFAULT_LOGS_PATH,
                "https://host/custom/otlp",
            ),
            (
                "http scheme and port preserved",
                "http://localhost:4318",
                DEFAULT_METRICS_PATH,
                "http://localhost:4318/v1/metrics",
            ),
            (
                "unparseable returned unchanged",
                "http://\x7f",
                DEFAULT_LOGS_PATH,
                "http://\x7f",
            ),
        ];
        for (name, input, def, want) in cases {
            assert_eq!(endpoint_url_for_signal(input, def), want, "{name}");
        }
    }
}
