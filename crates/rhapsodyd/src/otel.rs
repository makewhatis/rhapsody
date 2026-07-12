//! otel — the daemon's OTel config resolution (parity port of the otel section of
//! `$REF/cmd/symphony/run.go`): maps the `otel:` config block + `OTEL_*` env into a
//! [`rhapsody_telemetry::Config`], deciding export enablement and the plaintext-vs-TLS transport for a
//! loopback collector. Kept a pure function over `(&Otel, getenv)` so the boot's best-effort
//! resolution and the tests drive it without touching real env.

use rhapsody_config::Otel;
use rhapsody_telemetry as telemetry;

/// Maps the `otel:` config + `OTEL_*` env into a [`telemetry::Config`] and decides enablement:
/// enabled when `otel.enabled` is set OR the `OTEL_EXPORTER_OTLP_ENDPOINT` env supplies an endpoint
/// (env wins over config for the endpoint value).
///
/// A *config* endpoint alone does NOT force-enable export — `otel.enabled` is authoritative for
/// config-driven export (the Settings "Export telemetry" opt-out writes `otel.enabled:false` while
/// keeping the endpoint, and that opt-out must actually stop export). The env endpoint is an explicit
/// out-of-band operator opt-in, so it still force-enables. `getenv` is injected for testing (Go's
/// `os.Getenv`). Mirrors Go `resolveOtelConfig`.
pub fn resolve_otel_config(otel: &Otel, getenv: impl Fn(&str) -> String) -> telemetry::Config {
    let env_endpoint = getenv("OTEL_EXPORTER_OTLP_ENDPOINT");
    let mut endpoint = otel.endpoint.clone();
    if !env_endpoint.is_empty() {
        endpoint = env_endpoint.clone();
    }

    let mut svc = otel.service_name.clone();
    let env_svc = getenv("OTEL_SERVICE_NAME");
    if !env_svc.is_empty() {
        svc = env_svc;
    }
    if svc.is_empty() {
        svc = "symphony".to_string();
    }

    let mut proto = otel.protocol.clone();
    let env_proto = getenv("OTEL_EXPORTER_OTLP_PROTOCOL");
    if !env_proto.is_empty() {
        proto = env_proto;
    }
    if proto.is_empty() {
        // Default to OTLP/HTTP — the transport the ops-oma-prod hub collector serves (HTTP-only; gRPC
        // paths 404). An explicit `protocol: grpc` below is still honored for a gRPC collector.
        proto = "http".to_string();
    }
    proto = if proto.starts_with("http") {
        "http".to_string()
    } else {
        "grpc".to_string()
    };

    // TLS by default; plaintext only when the operator opts in OR the endpoint is a loopback target
    // reached over a *non-https* scheme (a local/node collector over loopback). An explicit https://
    // loopback endpoint is honored as secure — auto-insecure must never silently downgrade a
    // deliberate TLS scheme. The explicit `otel.insecure` opt-in still forces plaintext regardless.
    let insecure =
        otel.insecure || (endpoint_is_loopback(&endpoint) && !endpoint_is_https(&endpoint));

    telemetry::Config {
        enabled: otel.enabled || !env_endpoint.is_empty(),
        endpoint,
        protocol: proto,
        service_name: svc,
        headers: otel.headers.clone(),
        insecure,
        operator: otel.operator.clone(),
    }
}

/// Reports whether `ep` targets the loopback interface. Accepts both URL forms
/// (`http://host:port/path`) and bare `host:port`, falling back to treating the whole string as a
/// host when there is no port. An empty endpoint is not loopback (the SDK then reads `OTEL_*` env,
/// whose security we do not infer). Mirrors Go `endpointIsLoopback`.
fn endpoint_is_loopback(ep: &str) -> bool {
    if ep.is_empty() {
        return false;
    }
    let mut host = ep.to_string();
    // Prefer a URL parse (Go `url.Parse` + `u.Scheme != "" && u.Host != ""`); else `host:port`; else
    // treat the whole string as the host.
    if let Ok(u) = url::Url::parse(ep) {
        if let Some(h) = u.host_str() {
            // A parsed authority host (an `http://host…` URL).
            host = h.to_string();
        } else if let Some(h) = split_host_port(ep) {
            // A scheme-less `host:port` (url parses the leading token as an opaque scheme, no host).
            host = h;
        }
    } else if let Some(h) = split_host_port(ep) {
        host = h;
    }
    // Strip brackets from a bare IPv6 literal like "[::1]" (Go `u.Hostname()` already does; our
    // `host_str()` may retain them).
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match host.to_lowercase().as_str() {
        "localhost" | "127.0.0.1" | "::1" => return true,
        _ => {}
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    false
}

/// Reports whether `ep` is a URL with an explicit `https` scheme (case-insensitive). A bare
/// `host:port` or an `http://` endpoint is not https, so loopback auto-insecure still applies to
/// those; only a deliberate `https://` endpoint is treated as an explicit TLS opt-out of that
/// downgrade. Mirrors Go `endpointIsHTTPS`.
fn endpoint_is_https(ep: &str) -> bool {
    if ep.is_empty() {
        return false;
    }
    match url::Url::parse(ep) {
        Ok(u) => u.scheme().eq_ignore_ascii_case("https"),
        Err(_) => false,
    }
}

/// Extracts the host from a `host:port` (or `[ipv6]:port`) string, the pieces of Go's
/// `net.SplitHostPort` this resolution needs. Returns `None` when there is no port separator or the
/// host is a bare (bracket-less) IPv6 literal — mirroring the cases where Go's `SplitHostPort` errors
/// and the caller keeps the whole string as the host.
fn split_host_port(s: &str) -> Option<String> {
    if let Some(rest) = s.strip_prefix('[') {
        // "[ipv6]:port" → the bracketed host.
        return rest.split(']').next().map(str::to_string);
    }
    let (host, _port) = s.rsplit_once(':')?;
    // A host that still contains a colon is a bare (bracket-less) IPv6 literal — Go's SplitHostPort
    // rejects it ("too many colons"), so we do too (the caller keeps the whole string).
    if host.contains(':') {
        return None;
    }
    Some(host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noenv(_: &str) -> String {
        String::new()
    }

    // Mirrors Go `TestResolveOtelConfig`.
    #[test]
    fn resolve_otel_config_enablement() {
        // Disabled by default (no endpoint / enabled / env).
        let mut c = Otel {
            protocol: "grpc".to_string(),
            service_name: "symphony".to_string(),
            ..Default::default()
        };
        assert!(
            !resolve_otel_config(&c, noenv).enabled,
            "otel disabled when no endpoint/enabled/env"
        );
        // Enabled by config.enabled.
        c.enabled = true;
        assert!(
            resolve_otel_config(&c, noenv).enabled,
            "otel.enabled should enable"
        );
        // Enabled + endpoint from env (env overrides empty config endpoint).
        let c2 = Otel {
            protocol: "grpc".to_string(),
            ..Default::default()
        };
        let env = |k: &str| {
            if k == "OTEL_EXPORTER_OTLP_ENDPOINT" {
                "otel-collector:4317".to_string()
            } else {
                String::new()
            }
        };
        let oc = resolve_otel_config(&c2, env);
        assert!(oc.enabled && oc.endpoint == "otel-collector:4317");
        // Opt-out is authoritative (INF-299): otel.enabled:false with a config endpoint retained must
        // NOT export — a config endpoint alone never force-enables.
        let c3 = Otel {
            enabled: false,
            endpoint: "https://collector.example:4317".to_string(),
            ..Default::default()
        };
        assert!(
            !resolve_otel_config(&c3, noenv).enabled,
            "otel.enabled:false must disable export even with a config endpoint kept"
        );
        // But an env endpoint still force-enables even when otel.enabled is false.
        let c4 = Otel {
            enabled: false,
            ..Default::default()
        };
        assert!(
            resolve_otel_config(&c4, env).enabled,
            "OTEL_EXPORTER_OTLP_ENDPOINT env should force-enable even with otel.enabled:false"
        );
    }

    // Mirrors Go `TestResolveOtelConfigProtocolEnvOverride`.
    #[test]
    fn resolve_otel_config_protocol_env_override() {
        // Env OTEL_EXPORTER_OTLP_PROTOCOL overrides the (grpc) config protocol when it names an http
        // variant, so an HTTP endpoint isn't sent via the grpc exporter.
        let c = Otel {
            protocol: "grpc".to_string(),
            ..Default::default()
        };
        let env = |k: &str| {
            if k == "OTEL_EXPORTER_OTLP_PROTOCOL" {
                "http/protobuf".to_string()
            } else {
                String::new()
            }
        };
        assert_eq!(resolve_otel_config(&c, env).protocol, "http");
        // Config-side http variant (no env) normalizes the same way.
        let c2 = Otel {
            protocol: "http/protobuf".to_string(),
            ..Default::default()
        };
        assert_eq!(resolve_otel_config(&c2, noenv).protocol, "http");
        // Empty protocol (no config, no env) defaults to http.
        let c3 = Otel::default();
        assert_eq!(resolve_otel_config(&c3, noenv).protocol, "http");
        // An explicit protocol: grpc survives the http-default.
        let c4 = Otel {
            protocol: "grpc".to_string(),
            ..Default::default()
        };
        assert_eq!(resolve_otel_config(&c4, noenv).protocol, "grpc");
    }

    // Mirrors Go `TestResolveOtelConfigInsecureForLoopback`.
    #[test]
    fn resolve_otel_config_insecure_for_loopback() {
        let cases: &[(&str, &str, bool, bool)] = &[
            ("http loopback url", "http://localhost:4318", false, true),
            ("bare localhost hostport", "localhost:4318", false, true),
            ("grpc loopback hostport", "127.0.0.1:4317", false, true),
            ("ipv6 loopback url", "http://[::1]:4318", false, true),
            (
                "non-loopback hostport secure",
                "otel.example.com:4317",
                false,
                false,
            ),
            (
                "https non-loopback secure",
                "https://otel.example.com",
                false,
                false,
            ),
            (
                "non-loopback opt-in insecure",
                "otel.example.com:4317",
                true,
                true,
            ),
            ("empty endpoint secure", "", false, false),
            // Explicit https:// loopback stays secure — auto-insecure must not downgrade a deliberate
            // TLS scheme to plaintext.
            (
                "https loopback url secure",
                "https://localhost:4318",
                false,
                false,
            ),
            (
                "https loopback ip secure",
                "https://127.0.0.1:4318",
                false,
                false,
            ),
            // ...unless the operator explicitly opts into insecure, which still wins.
            (
                "https loopback opt-in insecure",
                "https://localhost:4318",
                true,
                true,
            ),
        ];
        for (name, endpoint, insecure, want) in cases {
            let c = Otel {
                protocol: "grpc".to_string(),
                endpoint: (*endpoint).to_string(),
                insecure: *insecure,
                ..Default::default()
            };
            let oc = resolve_otel_config(&c, noenv);
            assert_eq!(
                oc.insecure, *want,
                "{name}: endpoint {endpoint:?} insecure={insecure}: resolved {}",
                oc.insecure
            );
        }
    }
}
