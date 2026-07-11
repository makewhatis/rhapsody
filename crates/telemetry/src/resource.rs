//! resource — the OTLP resource shared by all signals. Parity port of Go `telemetry.buildResource`.

use opentelemetry::KeyValue;
use opentelemetry_sdk::Resource;

use crate::config::Config;
use crate::operator::{hostname, resolve_operator};

/// The `service.name` resource attribute key (stable OTLP semantic convention; Go uses the
/// `semconv.ServiceName` helper, which emits this same key).
const SERVICE_NAME: &str = "service.name";
/// The `host.name` resource attribute key (Go's `resource.WithHost()` sets this).
const HOST_NAME: &str = "host.name";
/// The fleet-attribution operator key (Go's `attribute.String("operator", …)`).
const OPERATOR: &str = "operator";

/// Constructs the OTLP resource shared by all signals. Beyond the SDK + env attributes that
/// [`Resource::builder`] seeds (Go's `WithFromEnv` + `WithTelemetrySDK`), it adds `service.name`
/// (from `cfg`, defaulting to `"symphony"`) and two fleet-attribution attributes: `host.name` (the
/// SDK does NOT auto-detect it — Go's `resource.WithHost()`) and `operator` (`cfg.operator`, else
/// the OS user, else the host — so every signal carries a non-empty operator to group by). Pure (no
/// exporters), so tests can inspect its attributes without standing up OTLP transport. Mirrors Go
/// `buildResource` (which returns a possibly-partial resource + non-fatal merge error; the Rust SDK
/// resolves merges by precedence, so there is no error to surface).
pub fn build_resource(cfg: &Config) -> Resource {
    let service_name = if cfg.service_name.is_empty() {
        "symphony".to_string()
    } else {
        cfg.service_name.clone()
    };
    Resource::builder()
        .with_attributes([
            KeyValue::new(SERVICE_NAME, service_name),
            KeyValue::new(HOST_NAME, hostname()),
            KeyValue::new(OPERATOR, resolve_operator(&cfg.operator)),
        ])
        .build()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn attrs(res: &Resource) -> HashMap<String, String> {
        res.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // Mirrors Go `TestBuildResourceOperatorAndHost`: an explicit operator wins, host.name is
    // populated, service.name is the configured value.
    #[test]
    fn build_resource_operator_and_host() {
        let res = build_resource(&Config {
            enabled: true,
            service_name: "symphony".to_string(),
            operator: "david".to_string(),
            ..Config::default()
        });
        let a = attrs(&res);
        assert_eq!(
            a.get(OPERATOR).map(String::as_str),
            Some("david"),
            "explicit operator wins"
        );
        assert!(
            a.get(HOST_NAME).is_some_and(|h| !h.is_empty()),
            "host.name populated"
        );
        assert_eq!(a.get(SERVICE_NAME).map(String::as_str), Some("symphony"));
    }

    // Mirrors Go `TestBuildResourceOperatorDefaultsToOSUser`: an empty operator derives a non-empty
    // value (OS user → host); operator is the fleet-attribution key and must never be empty.
    #[test]
    fn build_resource_operator_defaults_non_empty() {
        let res = build_resource(&Config {
            enabled: true,
            ..Config::default()
        });
        let a = attrs(&res);
        assert!(
            a.get(OPERATOR).is_some_and(|o| !o.is_empty()),
            "operator must default to a non-empty value (OS user / host)"
        );
    }
}
