//! The resolved telemetry configuration. Mirrors Go `telemetry.Config`.

use std::collections::HashMap;

/// The resolved telemetry configuration (env already applied by the caller). This is the T1→F1
/// contract: F1 builds it from the config crate's `otel:` block (`enabled`/`endpoint`/`protocol`/
/// `service_name`/`headers`/`insecure`/`operator`). Mirrors Go `telemetry.Config`.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// When false, [`crate::init`] is a complete no-op that still logs to stderr + the in-memory
    /// ring.
    pub enabled: bool,
    /// `host:port` (gRPC) or URL/`host:port` (HTTP); empty → the SDK reads `OTEL_*` env.
    pub endpoint: String,
    /// `"http"` selects the OTLP/HTTP exporters; anything else (default `"grpc"`) selects gRPC.
    pub protocol: String,
    /// The `service.name` resource attribute (defaults to `"symphony"` when empty).
    pub service_name: String,
    /// Extra OTLP request headers.
    pub headers: HashMap<String, String>,
    /// Selects plaintext (no-TLS) OTLP transport. When false the exporter uses the SDK default
    /// (TLS). The caller sets this true for loopback endpoints or an explicit `otel.insecure`.
    pub insecure: bool,
    /// A fleet-attribution operator (who runs this daemon). When empty [`crate::build_resource`]
    /// derives it from the OS user, falling back to the host name.
    pub operator: String,
}
