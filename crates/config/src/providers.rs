//! `providers` — Rhapsody-only provider configuration (STUDIO-984; design records
//! `provider-auth-design.md` §2.2/§3 and `provider-broker-design.md` §8). **No Go counterpart** — the
//! frozen reference has no provider concept at all, so this whole module is an ADDITIVE divergence
//! (README "Divergences"). Nothing here is reachable unless a workflow writes a `providers:` block,
//! which is what keeps an existing installation byte-identical.
//!
//! # Secrets are structurally unrepresentable
//!
//! Every type here is YAML-facing or pure and can carry ONLY metadata: a provider id, a protocol
//! name, a display name, a `base_url`, the `allow_insecure_http` policy, a credential *source kind*,
//! and validated broker limits. There is deliberately no value/key/token/secret field anywhere, and
//! none may be added: `provider-auth-design.md` §2.2 says `WORKFLOW.md` "never stores an API key,
//! refresh token, OAuth secret, generated OpenCode `auth.json`, or provider response". The runtime
//! types that DO touch a credential live in later slices (`ResolvedProviderPlan` here, then PB5's
//! move-only `PreparedProvider`), and even they never carry a reusable key.
//!
//! # The cross-surface identifier and binding contracts
//!
//! A canonical provider id is 1-64 lowercase ASCII characters matching `[a-z][a-z0-9_-]{0,63}`.
//! Non-canonical spellings are REJECTED, never case-folded, so the id round-trips identically
//! through YAML, ticket labels, API paths, metrics, provenance, and the derived Keychain account
//! (`provider-auth-design.md` §2.2). [`canonical_provider_id`] is the one parser; every surface that
//! validates or normalizes an id must call it rather than re-implementing the rule.
//!
//! Provider normalization derives ONE credential binding,
//! `(provider_id, openai-chat-completions-bearer-v1, normalized_base_url)`, used identically by
//! desktop storage, credential reads, broker registration, status, and refusal tests
//! ([`ProviderDefinition::credential_binding`]). The adapter identity
//! ([`ADAPTER_OPENAI_CHAT_COMPLETIONS_BEARER_V1`]) is the reviewed Chat Completions + Bearer adapter
//! that v1's `openai-compatible` protocol means — NOT arbitrary auth headers or fields.
//!
//! # `base_url` is the protocol root immediately above `chat/completions`
//!
//! [`normalize_provider_base_url`] reduces a configured base URL to that root: it strips a trailing
//! `/`, and appends `/v1` ONLY when the path does not already end in `/v1`. A base that already ends
//! in `/v1` or `/inference/v1` is therefore left alone, so the chat-completions URL
//! ([`chat_completions_url`]) never grows a second `/v1`.
//!
//! # Broker limits
//!
//! [`BrokerLimits`] carries the always-on finite limits from `provider-broker-design.md` §8.1,
//! materialized to the V1 default column when absent, plus the optional durable UTC-day cap (absent
//! means "no Rhapsody daily cap"). [`BrokerLimits::validate`] checks every value together: positive,
//! at or below the compile-time hard ceiling, and the cross-field ordering rules. Hard ceilings are
//! compile-time constants and are NOT configurable through workflow or API input.

use std::collections::BTreeMap;

/// The hard ceiling on providers per effective workflow (`provider-auth-design.md` §2.2).
pub const MAX_PROVIDERS: usize = 256;

/// Maximum length of a canonical provider id.
pub const PROVIDER_ID_MAX_LEN: usize = 64;

/// Maximum length of a model id in UTF-8 bytes (`provider-auth-design.md` §2.2).
pub const MODEL_ID_MAX_BYTES: usize = 512;

/// The only protocol v1 understands: OpenAI Chat Completions with Bearer API-key auth.
pub const PROTOCOL_OPENAI_COMPATIBLE: &str = "openai-compatible";

/// The reviewed adapter identity `openai-compatible` denotes. A cross-surface contract: it is part
/// of every credential binding and must not drift from the broker crate's own spelling.
pub const ADAPTER_OPENAI_CHAT_COMPLETIONS_BEARER_V1: &str = "openai-chat-completions-bearer-v1";

/// The only credential source kind v1 accepts. A *kind*, never an account name and never a value.
pub const CREDENTIAL_SOURCE_KEYCHAIN: &str = "keychain";

// ---------------------------------------------------------------------------
// Broker limits: V1 defaults and daemon hard ceilings (`provider-broker-design.md` §8.1)
// ---------------------------------------------------------------------------

/// Forwarded upstream requests per outer turn: default / ceiling.
pub const DEFAULT_FORWARDED_REQUESTS_PER_TURN: u32 = 64;
pub const MAX_FORWARDED_REQUESTS_PER_TURN: u32 = 256;
/// Authenticated locally denied requests before revocation: default / ceiling.
pub const DEFAULT_DENIED_REQUESTS_BEFORE_REVOCATION: u32 = 16;
pub const MAX_DENIED_REQUESTS_BEFORE_REVOCATION: u32 = 64;
/// Concurrent upstream requests per turn: default / ceiling.
pub const DEFAULT_CONCURRENT_UPSTREAM_REQUESTS_PER_TURN: u32 = 4;
pub const MAX_CONCURRENT_UPSTREAM_REQUESTS_PER_TURN: u32 = 8;
/// JSON request bytes: default / ceiling.
pub const DEFAULT_JSON_REQUEST_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_JSON_REQUEST_BYTES: u64 = 16 * 1024 * 1024;
/// Aggregate request bytes per outer turn: default / ceiling.
pub const DEFAULT_AGGREGATE_REQUEST_BYTES_PER_TURN: u64 = 32 * 1024 * 1024;
pub const MAX_AGGREGATE_REQUEST_BYTES_PER_TURN: u64 = 128 * 1024 * 1024;
/// Response bytes across one request: default / ceiling.
pub const DEFAULT_RESPONSE_BYTES_PER_REQUEST: u64 = 16 * 1024 * 1024;
pub const MAX_RESPONSE_BYTES_PER_REQUEST: u64 = 32 * 1024 * 1024;
/// Aggregate response bytes per outer turn: default / ceiling.
pub const DEFAULT_AGGREGATE_RESPONSE_BYTES_PER_TURN: u64 = 64 * 1024 * 1024;
pub const MAX_AGGREGATE_RESPONSE_BYTES_PER_TURN: u64 = 256 * 1024 * 1024;
/// Requested output tokens per request: default / ceiling.
pub const DEFAULT_REQUESTED_OUTPUT_TOKENS_PER_REQUEST: u64 = 32_000;
pub const MAX_REQUESTED_OUTPUT_TOKENS_PER_REQUEST: u64 = 131_072;
/// Reserved token units per outer turn: default / ceiling.
pub const DEFAULT_RESERVED_TOKEN_UNITS_PER_TURN: u64 = 1_000_000;
pub const MAX_RESERVED_TOKEN_UNITS_PER_TURN: u64 = 32_000_000;
/// Reserved token units per Rhapsody session/run: default / ceiling.
pub const DEFAULT_RESERVED_TOKEN_UNITS_PER_SESSION: u64 = 20_000_000;
pub const MAX_RESERVED_TOKEN_UNITS_PER_SESSION: u64 = 640_000_000;
/// Capability lifetime, in milliseconds: default / ceiling (one hour).
pub const DEFAULT_CAPABILITY_LIFETIME_MS: u64 = 3_600_000;
pub const MAX_CAPABILITY_LIFETIME_MS: u64 = 3_600_000;

/// The validated broker limits for one provider (`provider-broker-design.md` §8.1). Concrete values
/// with the V1 default column materialized when the workflow omits the block; there is no
/// "permissive implicit default" for [`Self::max_reserved_token_units_per_utc_day`], which is `None`
/// (no Rhapsody daily cap) unless the operator writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerLimits {
    pub forwarded_requests_per_turn: u32,
    pub denied_requests_before_revocation: u32,
    pub concurrent_upstream_requests_per_turn: u32,
    pub json_request_bytes: u64,
    pub aggregate_request_bytes_per_turn: u64,
    pub response_bytes_per_request: u64,
    pub aggregate_response_bytes_per_turn: u64,
    pub requested_output_tokens_per_request: u64,
    pub reserved_token_units_per_turn: u64,
    pub reserved_token_units_per_session: u64,
    pub capability_lifetime_ms: u64,
    /// Optional durable UTC-day cap. `None` — the default, and every install that never writes the
    /// key — means no Rhapsody daily cap. When present it is a checked positive `u64`, may be lower
    /// than one run cap, and needs durable budget storage available at runtime.
    pub max_reserved_token_units_per_utc_day: Option<u64>,
}

impl Default for BrokerLimits {
    /// The V1 default column exactly (`provider-broker-design.md` §8.1), with no daily cap.
    fn default() -> Self {
        Self {
            forwarded_requests_per_turn: DEFAULT_FORWARDED_REQUESTS_PER_TURN,
            denied_requests_before_revocation: DEFAULT_DENIED_REQUESTS_BEFORE_REVOCATION,
            concurrent_upstream_requests_per_turn: DEFAULT_CONCURRENT_UPSTREAM_REQUESTS_PER_TURN,
            json_request_bytes: DEFAULT_JSON_REQUEST_BYTES,
            aggregate_request_bytes_per_turn: DEFAULT_AGGREGATE_REQUEST_BYTES_PER_TURN,
            response_bytes_per_request: DEFAULT_RESPONSE_BYTES_PER_REQUEST,
            aggregate_response_bytes_per_turn: DEFAULT_AGGREGATE_RESPONSE_BYTES_PER_TURN,
            requested_output_tokens_per_request: DEFAULT_REQUESTED_OUTPUT_TOKENS_PER_REQUEST,
            reserved_token_units_per_turn: DEFAULT_RESERVED_TOKEN_UNITS_PER_TURN,
            reserved_token_units_per_session: DEFAULT_RESERVED_TOKEN_UNITS_PER_SESSION,
            capability_lifetime_ms: DEFAULT_CAPABILITY_LIFETIME_MS,
            max_reserved_token_units_per_utc_day: None,
        }
    }
}

impl BrokerLimits {
    /// Validates every limit together, returning an actionable reason on the first failure
    /// (`provider-broker-design.md` §8.1). Rules, in order:
    ///
    /// * every value is positive and at or below its compile-time hard ceiling;
    /// * per-request JSON bytes ≤ aggregate request bytes per turn;
    /// * per-request response bytes ≤ aggregate response bytes per turn;
    /// * concurrent requests ≤ forwarded requests per turn;
    /// * reserved token units per turn ≤ reserved token units per session/run;
    /// * capability lifetime ≤ `turn_timeout_ms` (the harness's own turn deadline);
    /// * the optional daily value, when present, is a positive `u64` (it may be lower than one run
    ///   cap by design).
    pub fn validate(&self, turn_timeout_ms: u64) -> Result<(), String> {
        macro_rules! bounded {
            ($field:literal, $value:expr, $max:expr) => {
                if $value == 0 {
                    return Err(format!("{} must be positive", $field));
                }
                if $value > $max {
                    return Err(format!(
                        "{} must be at most {} (the daemon hard ceiling)",
                        $field, $max
                    ));
                }
            };
        }
        bounded!(
            "forwarded_requests_per_turn",
            self.forwarded_requests_per_turn,
            MAX_FORWARDED_REQUESTS_PER_TURN
        );
        bounded!(
            "denied_requests_before_revocation",
            self.denied_requests_before_revocation,
            MAX_DENIED_REQUESTS_BEFORE_REVOCATION
        );
        bounded!(
            "concurrent_upstream_requests_per_turn",
            self.concurrent_upstream_requests_per_turn,
            MAX_CONCURRENT_UPSTREAM_REQUESTS_PER_TURN
        );
        bounded!(
            "json_request_bytes",
            self.json_request_bytes,
            MAX_JSON_REQUEST_BYTES
        );
        bounded!(
            "aggregate_request_bytes_per_turn",
            self.aggregate_request_bytes_per_turn,
            MAX_AGGREGATE_REQUEST_BYTES_PER_TURN
        );
        bounded!(
            "response_bytes_per_request",
            self.response_bytes_per_request,
            MAX_RESPONSE_BYTES_PER_REQUEST
        );
        bounded!(
            "aggregate_response_bytes_per_turn",
            self.aggregate_response_bytes_per_turn,
            MAX_AGGREGATE_RESPONSE_BYTES_PER_TURN
        );
        bounded!(
            "requested_output_tokens_per_request",
            self.requested_output_tokens_per_request,
            MAX_REQUESTED_OUTPUT_TOKENS_PER_REQUEST
        );
        bounded!(
            "reserved_token_units_per_turn",
            self.reserved_token_units_per_turn,
            MAX_RESERVED_TOKEN_UNITS_PER_TURN
        );
        bounded!(
            "reserved_token_units_per_session",
            self.reserved_token_units_per_session,
            MAX_RESERVED_TOKEN_UNITS_PER_SESSION
        );
        bounded!(
            "capability_lifetime_ms",
            self.capability_lifetime_ms,
            MAX_CAPABILITY_LIFETIME_MS
        );

        if self.json_request_bytes > self.aggregate_request_bytes_per_turn {
            return Err(
                "json_request_bytes must not exceed aggregate_request_bytes_per_turn".to_string(),
            );
        }
        if self.response_bytes_per_request > self.aggregate_response_bytes_per_turn {
            return Err(
                "response_bytes_per_request must not exceed aggregate_response_bytes_per_turn"
                    .to_string(),
            );
        }
        if self.concurrent_upstream_requests_per_turn > self.forwarded_requests_per_turn {
            return Err(
                "concurrent_upstream_requests_per_turn must not exceed forwarded_requests_per_turn"
                    .to_string(),
            );
        }
        if self.reserved_token_units_per_turn > self.reserved_token_units_per_session {
            return Err(
                "reserved_token_units_per_turn must not exceed reserved_token_units_per_session"
                    .to_string(),
            );
        }
        if self.capability_lifetime_ms > turn_timeout_ms {
            return Err(
                "capability_lifetime_ms must not exceed the harness turn timeout".to_string(),
            );
        }
        if let Some(daily) = self.max_reserved_token_units_per_utc_day
            && daily == 0
        {
            return Err("max_reserved_token_units_per_utc_day must be positive".to_string());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Credential reference (a storage KIND, never a value or an account)
// ---------------------------------------------------------------------------

/// A reference to where a provider's credential is stored (`provider-auth-design.md` §2.2/§2.4).
/// `source` names a storage KIND — v1 accepts only [`CREDENTIAL_SOURCE_KEYCHAIN`] — never an
/// arbitrary Keychain account and never a value. The struct has exactly this one field on purpose:
/// adding a `value`/`token`/`key` field would make a reusable secret representable in YAML, which
/// the acceptance contract forbids.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CredentialRef {
    pub source: String,
}

impl CredentialRef {
    /// Reports whether this names the one credential source v1 accepts.
    pub fn is_supported(&self) -> bool {
        self.source == CREDENTIAL_SOURCE_KEYCHAIN
    }
}

// ---------------------------------------------------------------------------
// Provider definition (persisted metadata; still no secret)
// ---------------------------------------------------------------------------

/// One operator-configured provider (`provider-auth-design.md` §2.2, `provider-broker-design.md`
/// §3.1's `ProviderDefinition`). Persisted metadata plus a derived credential *reference*; never a
/// secret. `id` is the canonical map key; `base_url` is stored verbatim here (normalization is a
/// pure derivation, [`Self::normalized_base_url`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderDefinition {
    /// Canonical provider id (`[a-z][a-z0-9_-]{0,63}`), equal to the YAML map key.
    pub id: String,
    /// Explicit protocol; v1 accepts only [`PROTOCOL_OPENAI_COMPATIBLE`]. Never inferred.
    pub protocol: String,
    /// Human-readable label; free-form and never used for routing or binding.
    pub display_name: String,
    /// The OpenAI client protocol base immediately above `chat/completions`. Required for the
    /// compatible protocol.
    pub base_url: String,
    /// Operator policy: `false` by default, required `true` for an `http` base URL, and rejected
    /// `true` on `https`. Never inferred from addressing or child input.
    pub allow_insecure_http: bool,
    /// Where the credential lives; a storage kind, never a value.
    pub credential: CredentialRef,
    /// Validated broker limits; the V1 default column when the workflow omits the block.
    pub broker_limits: BrokerLimits,
}

impl ProviderDefinition {
    /// The normalized base URL: [`normalize_provider_base_url`] applied to the configured value.
    pub fn normalized_base_url(&self) -> Result<String, String> {
        normalize_provider_base_url(&self.base_url)
    }

    /// The one canonical credential binding this provider derives
    /// (`provider-auth-design.md` §2.2 / `provider-broker-design.md` §3.1): the canonical id, the
    /// reviewed adapter identity, and the normalized endpoint. Used identically by desktop storage,
    /// credential reads, broker registration, status, and refusal tests.
    pub fn credential_binding(&self) -> Result<CredentialBinding, String> {
        Ok(CredentialBinding {
            provider_id: self.id.clone(),
            adapter: ADAPTER_OPENAI_CHAT_COMPLETIONS_BEARER_V1.to_string(),
            base_url: self.normalized_base_url()?,
        })
    }
}

// ---------------------------------------------------------------------------
// Canonical provider id
// ---------------------------------------------------------------------------

/// Validates a provider id against the exact canonical syntax in `provider-auth-design.md` §2.2:
/// 1-64 lowercase ASCII characters matching `[a-z][a-z0-9_-]{0,63}`. Non-canonical spellings are
/// REJECTED, never case-folded, so every cross-surface spelling agrees. Returns an actionable reason
/// on failure.
pub fn canonical_provider_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("provider id must not be empty".to_string());
    }
    if id.len() > PROVIDER_ID_MAX_LEN {
        return Err(format!(
            "provider id must be at most {PROVIDER_ID_MAX_LEN} characters"
        ));
    }
    let bytes = id.as_bytes();
    let first = bytes[0];
    if !(first.is_ascii_lowercase()) {
        return Err(format!(
            "provider id {id:?} must start with a lowercase ASCII letter"
        ));
    }
    for &b in &bytes[1..] {
        if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-') {
            return Err(format!(
                "provider id {id:?} may contain only lowercase ASCII letters, digits, '_' and '-'"
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Model id
// ---------------------------------------------------------------------------

/// Validates a model id against §2.2's TRANSPORT bounds only: non-empty, at most
/// [`MODEL_ID_MAX_BYTES`] UTF-8 bytes, no control/NUL bytes, and no surrounding whitespace. Model
/// ids otherwise remain OPAQUE (including `/`) even when model discovery is unavailable — this
/// deliberately does NOT require catalog membership.
pub fn validate_model_id(model: &str) -> Result<(), String> {
    if model.is_empty() {
        return Err("model id must not be empty".to_string());
    }
    if model.len() > MODEL_ID_MAX_BYTES {
        return Err(format!(
            "model id must be at most {MODEL_ID_MAX_BYTES} UTF-8 bytes"
        ));
    }
    if model.trim() != model {
        return Err("model id must not have surrounding whitespace".to_string());
    }
    if model.chars().any(|c| c.is_control() || c == '\0') {
        return Err("model id must not contain control or NUL bytes".to_string());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// base_url normalization and TLS policy
// ---------------------------------------------------------------------------

/// The scheme of a base URL, after the allow_insecure_http policy check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseUrlScheme {
    Https,
    Http,
}

impl BaseUrlScheme {
    /// Whether this scheme is plaintext HTTP.
    pub fn is_insecure(self) -> bool {
        matches!(self, BaseUrlScheme::Http)
    }
}

/// Parses and checks the `base_url` TLS policy, returning the scheme
/// (`provider-auth-design.md` §2.2):
///
/// * the URL must be absolute with an `http`/`https` scheme and a non-empty host;
/// * `allow_insecure_http` must be `true` for an `http` base URL;
/// * `allow_insecure_http: true` is REJECTED on an `https` base URL (an explicit `false` or an
///   omitted value is fine there).
pub fn base_url_scheme(base_url: &str, allow_insecure_http: bool) -> Result<BaseUrlScheme, String> {
    let (scheme, rest) = split_scheme(base_url)
        .ok_or_else(|| format!("base_url {base_url:?} must be an absolute http(s) URL"))?;
    let scheme = match scheme {
        "https" => BaseUrlScheme::Https,
        "http" => BaseUrlScheme::Http,
        other => {
            return Err(format!(
                "base_url {base_url:?} has unsupported scheme {other:?} (want http or https)"
            ));
        }
    };
    if rest.is_empty() || rest.starts_with('/') {
        return Err(format!("base_url {base_url:?} has no host"));
    }
    match scheme {
        BaseUrlScheme::Http if !allow_insecure_http => Err(format!(
            "base_url {base_url:?} is http but allow_insecure_http is not true"
        )),
        BaseUrlScheme::Https if allow_insecure_http => Err(format!(
            "base_url {base_url:?} is https but allow_insecure_http is true"
        )),
        _ => Ok(scheme),
    }
}

/// Splits `scheme://…` into its lowercase scheme and remainder, or `None` when malformed.
fn split_scheme(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() || !scheme.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    Some((scheme, rest))
}

/// Normalizes a `base_url` to the OpenAI client protocol base immediately above
/// `chat/completions` (`provider-auth-design.md` §2.2 / the ticket's "Ways to get this wrong").
///
/// The rule is exactly: strip a trailing `/`; then append `/v1` ONLY when the path does not already
/// end in `/v1`. A base ending in `/v1` or `/inference/v1` is therefore unchanged, so a subsequent
/// [`chat_completions_url`] never grows a second `/v1`.
pub fn normalize_provider_base_url(base_url: &str) -> Result<String, String> {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(format!("base_url {base_url:?} must not be empty"));
    }
    // Validate shape (scheme + host), but with no TLS assumption here — the caller applies
    // `base_url_scheme` with the configured policy.
    let (_, rest) = split_scheme(trimmed)
        .ok_or_else(|| format!("base_url {base_url:?} must be an absolute http(s) URL"))?;
    if rest.is_empty() || rest.starts_with('/') {
        return Err(format!("base_url {base_url:?} has no host"));
    }
    if trimmed.ends_with("/v1") {
        return Ok(trimmed.to_string());
    }
    Ok(format!("{trimmed}/v1"))
}

/// The chat-completions URL for a base URL, where `base` is the normalized protocol root
/// ([`normalize_provider_base_url`]). The `/v1` is already part of `base`, so this appends only the
/// final `/chat/completions`.
pub fn chat_completions_url(base: &str) -> String {
    format!("{base}/chat/completions")
}

// ---------------------------------------------------------------------------
// Canonical credential binding
// ---------------------------------------------------------------------------

/// The one canonical credential binding `(provider_id, adapter, normalized_base_url)` from
/// `provider-auth-design.md` §2.2. Non-secret; the desktop store, credential reads, broker
/// registration, status and refusal tests all derive it the same way
/// ([`ProviderDefinition::credential_binding`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialBinding {
    pub provider_id: String,
    pub adapter: String,
    pub base_url: String,
}

impl CredentialBinding {
    /// A stable, non-secret string identity for this binding. Never a secret and never serialized
    /// to an operator-facing surface; it exists so two derivations can be compared for equality.
    pub fn identity(&self) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}",
            self.provider_id, self.adapter, self.base_url
        )
    }

    /// The canonical Keychain account this binding derives. Derived ONLY from the canonical provider
    /// id (`provider-auth-design.md` §2.4: "an account derived only from the canonical provider ID,
    /// for example `v1:anthropic`"). An operator can neither spell this in YAML nor make a provider
    /// definition name an arbitrary Keychain item.
    pub fn keychain_account(&self) -> String {
        format!("v1:{}", self.provider_id)
    }
}

// ---------------------------------------------------------------------------
// Provider reload change signal
// ---------------------------------------------------------------------------

/// The pure, non-secret change signal a workflow reload emits for the provider set (STUDIO-984;
/// the P9 acceptance bullet: "provider reload emits the generation/change information P9 needs to
/// invalidate and asynchronously refresh non-secret binding status"). It performs NO owner I/O and
/// is safe to compute in a control task or a GET handler's snapshot — the actual credential read is
/// P9's off-loop job.
///
/// [`Self::revision`] is a stable digest over the canonical bindings and their limits: two provider
/// sets with the same bindings and limits share a revision, and any change to a binding, a limit, or
/// the set itself moves it. [`Self::bindings`] names exactly which non-secret status P9 must refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderReload {
    revision: u64,
    bindings: Vec<CredentialBinding>,
}

impl ProviderReload {
    /// Derives the reload signal from an effective provider map. A provider whose binding cannot be
    /// derived (malformed base URL) contributes its raw id and an empty endpoint, so a config that
    /// would fail validation still produces a deterministic revision rather than panicking.
    pub fn from_providers(providers: &BTreeMap<String, ProviderDefinition>) -> Self {
        let mut bindings: Vec<CredentialBinding> = Vec::with_capacity(providers.len());
        let mut hash = FNV_OFFSET;
        for (id, def) in providers {
            let binding = match def.credential_binding() {
                Ok(b) => b,
                Err(_) => CredentialBinding {
                    provider_id: id.clone(),
                    adapter: ADAPTER_OPENAI_CHAT_COMPLETIONS_BEARER_V1.to_string(),
                    base_url: String::new(),
                },
            };
            hash = fnv1a(hash, binding.identity().as_bytes());
            // The limits are part of what P9's status shows, so a limits change is a change too.
            hash = fnv1a(
                hash,
                &def.broker_limits
                    .reserved_token_units_per_session
                    .to_le_bytes(),
            );
            hash = fnv1a(
                hash,
                &def.broker_limits
                    .max_reserved_token_units_per_utc_day
                    .unwrap_or(0)
                    .to_le_bytes(),
            );
            bindings.push(binding);
        }
        Self {
            revision: hash,
            bindings,
        }
    }

    /// The stable, non-secret revision digest.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The canonical bindings whose non-secret status must be refreshed.
    pub fn bindings(&self) -> &[CredentialBinding] {
        &self.bindings
    }

    /// Whether this signal differs from a previously observed revision. `None` (no prior signal)
    /// is always a change, so the first reload after boot refreshes every binding.
    pub fn changed_from(&self, previous: Option<u64>) -> bool {
        previous != Some(self.revision)
    }
}

/// FNV-1a 64-bit offset basis. A tiny, dependency-free, deterministic digest — no cryptographic
/// property is claimed or needed; this only detects "did the non-secret binding set change".
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider definition builder for table tests; every field is explicit so a row isolates the
    /// ONE field it flips.
    fn provider(id: &str, base_url: &str, allow_insecure_http: bool) -> ProviderDefinition {
        ProviderDefinition {
            id: id.to_string(),
            protocol: PROTOCOL_OPENAI_COMPATIBLE.to_string(),
            display_name: String::new(),
            base_url: base_url.to_string(),
            allow_insecure_http,
            credential: CredentialRef {
                source: CREDENTIAL_SOURCE_KEYCHAIN.to_string(),
            },
            broker_limits: BrokerLimits::default(),
        }
    }

    // The canonical-id rule, table-driven. Non-canonical spellings are REJECTED, never case-folded —
    // a mutation that lowercases/titlecases its input before checking reds the upper/symbol rows.
    #[test]
    fn canonical_provider_id_table() {
        for ok in [
            "a",
            "fireworks",
            "fireworks-ai",
            "a_b_c",
            "z9",
            "a-very-long-id",
        ] {
            assert!(
                canonical_provider_id(ok).is_ok(),
                "{ok:?} should be canonical"
            );
        }
        for bad in [
            "",
            "Fireworks", // uppercase
            "9lives",    // must start with a letter
            "-lead",     // must start with a letter
            "_lead",     // must start with a letter
            "has space",
            "has.dot",
            "café",      // non-ASCII
            "trailing-", // trailing '-' is actually allowed; kept out of `bad` below
        ] {
            if bad == "trailing-" {
                continue;
            }
            assert!(
                canonical_provider_id(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
        // 64 chars is the maximum; 65 is over.
        let max = "a".repeat(PROVIDER_ID_MAX_LEN);
        assert!(canonical_provider_id(&max).is_ok());
        let over = "a".repeat(PROVIDER_ID_MAX_LEN + 1);
        assert!(canonical_provider_id(&over).is_err());
    }

    // TLS policy: false by default, required for http, rejected as true on https, and explicit
    // false/omitted round-trip identically on https.
    #[test]
    fn base_url_tls_policy_table() {
        assert_eq!(
            base_url_scheme("https://api.example/v1", false),
            Ok(BaseUrlScheme::Https)
        );
        assert_eq!(
            base_url_scheme("http://localhost:8080/v1", true),
            Ok(BaseUrlScheme::Http)
        );
        assert!(
            base_url_scheme("http://localhost:8080/v1", false).is_err(),
            "http without opt-in must refuse"
        );
        assert!(
            base_url_scheme("https://api.example/v1", true).is_err(),
            "allow_insecure_http: true on https must refuse"
        );
        assert!(base_url_scheme("ftp://api.example/v1", false).is_err());
        assert!(base_url_scheme("api.example/v1", false).is_err());
        assert!(base_url_scheme("https:///v1", false).is_err());
    }

    // MUTATION GUARD (`/v1` must not be appended unconditionally): a base that already ends in
    // `/v1` or `/inference/v1` normalizes to itself, so the derived chat URL never doubles the
    // version. An implementation that appends `/v1` unconditionally produces
    // `https://api.fireworks.ai/inference/v1/v1/chat/completions` and reds the first row.
    #[test]
    fn base_url_normalization_never_adds_a_second_v1() {
        assert_eq!(
            normalize_provider_base_url("https://api.fireworks.ai/inference/v1").unwrap(),
            "https://api.fireworks.ai/inference/v1"
        );
        assert_eq!(
            normalize_provider_base_url("https://api.openai.com/v1").unwrap(),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            normalize_provider_base_url("https://api.openai.com/v1/").unwrap(),
            "https://api.openai.com/v1"
        );
        // A bare host gains exactly one /v1.
        assert_eq!(
            normalize_provider_base_url("https://api.openai.com").unwrap(),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            normalize_provider_base_url("https://api.example/").unwrap(),
            "https://api.example/v1"
        );
        // The chat URL is the normalized root plus one path segment.
        let base = normalize_provider_base_url("https://api.fireworks.ai/inference/v1").unwrap();
        assert_eq!(
            chat_completions_url(&base),
            "https://api.fireworks.ai/inference/v1/chat/completions"
        );
    }

    // Model ids are opaque within §2.2's transport bounds: `/` is allowed, catalog membership is
    // never required, and only the four transport rules refuse.
    #[test]
    fn model_id_transport_bounds_table() {
        for ok in [
            "accounts/fireworks/models/deepseek-v4p1-flash",
            "gpt-4o",
            "a",
        ] {
            assert!(validate_model_id(ok).is_ok(), "{ok:?} is opaque and valid");
        }
        assert!(validate_model_id("").is_err(), "empty refused");
        assert!(
            validate_model_id(" padded ").is_err(),
            "surrounding whitespace"
        );
        assert!(validate_model_id("has\nnewline").is_err(), "control byte");
        assert!(validate_model_id("has\0nul").is_err(), "NUL byte");
        let long = "a".repeat(MODEL_ID_MAX_BYTES + 1);
        assert!(validate_model_id(&long).is_err(), "over 512 bytes");
        let max = "a".repeat(MODEL_ID_MAX_BYTES);
        assert!(validate_model_id(&max).is_ok(), "exactly 512 bytes");
    }

    // The canonical binding is derived identically from the definition and used by the Keychain
    // account derivation. A mutation that derives the account from a display name or an arbitrary
    // string reds `keychain_account_ignores_display_name`.
    #[test]
    fn credential_binding_is_canonical() {
        let mut p = provider("fireworks", "https://api.fireworks.ai/inference/v1/", false);
        p.display_name = "Fireworks".to_string();
        let binding = p.credential_binding().unwrap();
        assert_eq!(binding.provider_id, "fireworks");
        assert_eq!(binding.adapter, ADAPTER_OPENAI_CHAT_COMPLETIONS_BEARER_V1);
        assert_eq!(binding.base_url, "https://api.fireworks.ai/inference/v1");
        assert_eq!(binding.keychain_account(), "v1:fireworks");
        // The identity is stable and contains no secret-looking value.
        assert_eq!(binding.identity(), binding.identity());
        assert!(!binding.identity().contains("Fireworks"));
    }

    // Broker limits: the default column materializes exactly, and the daily cap has no implicit
    // default (None means "no Rhapsody daily cap").
    #[test]
    fn broker_limits_defaults_match_the_v1_column() {
        let l = BrokerLimits::default();
        assert_eq!(l.forwarded_requests_per_turn, 64);
        assert_eq!(l.denied_requests_before_revocation, 16);
        assert_eq!(l.concurrent_upstream_requests_per_turn, 4);
        assert_eq!(l.json_request_bytes, 8 * 1024 * 1024);
        assert_eq!(l.aggregate_request_bytes_per_turn, 32 * 1024 * 1024);
        assert_eq!(l.response_bytes_per_request, 16 * 1024 * 1024);
        assert_eq!(l.aggregate_response_bytes_per_turn, 64 * 1024 * 1024);
        assert_eq!(l.requested_output_tokens_per_request, 32_000);
        assert_eq!(l.reserved_token_units_per_turn, 1_000_000);
        assert_eq!(l.reserved_token_units_per_session, 20_000_000);
        assert_eq!(l.capability_lifetime_ms, 3_600_000);
        assert_eq!(
            l.max_reserved_token_units_per_utc_day, None,
            "absent means no Rhapsody daily cap, never a permissive implicit one"
        );
        assert!(l.validate(3_600_000).is_ok());
    }

    // MUTATION GUARD: a limit over its hard ceiling, or a broken cross-field ordering, must refuse.
    // Weakening `validate` to check only positivity reds these rows.
    #[test]
    fn broker_limits_validation_table() {
        let base = BrokerLimits::default();
        // Over the daemon ceiling for forwarded requests.
        let over = BrokerLimits {
            forwarded_requests_per_turn: MAX_FORWARDED_REQUESTS_PER_TURN + 1,
            ..base.clone()
        };
        assert!(over.validate(3_600_000).is_err());

        // Concurrency greater than forwarded count.
        let bad = BrokerLimits {
            concurrent_upstream_requests_per_turn: 64,
            ..base.clone()
        };
        assert!(bad.validate(3_600_000).is_err());

        // Per-request JSON bytes greater than the aggregate.
        let bad = BrokerLimits {
            json_request_bytes: 64 * 1024 * 1024,
            ..base.clone()
        };
        assert!(bad.validate(3_600_000).is_err());

        // Per-request response bytes greater than the aggregate.
        let bad = BrokerLimits {
            response_bytes_per_request: 128 * 1024 * 1024,
            ..base.clone()
        };
        assert!(bad.validate(3_600_000).is_err());

        // Per-turn reserved units greater than the session cap.
        let bad = BrokerLimits {
            reserved_token_units_per_turn: 30_000_000,
            ..base.clone()
        };
        assert!(bad.validate(3_600_000).is_err());

        // Capability lifetime greater than the harness turn timeout.
        let bad = BrokerLimits {
            capability_lifetime_ms: 3_600_000,
            ..base.clone()
        };
        assert!(
            bad.validate(60_000).is_err(),
            "a lifetime longer than the turn deadline must refuse"
        );

        // Zero is not positive.
        let bad = BrokerLimits {
            requested_output_tokens_per_request: 0,
            ..base.clone()
        };
        assert!(bad.validate(3_600_000).is_err());

        // A zero daily cap is refused; a positive one below one run cap is allowed.
        let bad = BrokerLimits {
            max_reserved_token_units_per_utc_day: Some(0),
            ..base.clone()
        };
        assert!(bad.validate(3_600_000).is_err());
        let ok = BrokerLimits {
            max_reserved_token_units_per_utc_day: Some(500_000),
            ..base
        };
        assert!(ok.validate(3_600_000).is_ok());
    }

    // The reload signal is pure: the same set yields the same revision, and any binding/limit change
    // moves it. `changed_from(None)` is always a change (the post-boot first refresh).
    #[test]
    fn provider_reload_is_a_pure_change_signal() {
        let mut a = BTreeMap::new();
        a.insert(
            "fireworks".to_string(),
            provider("fireworks", "https://api.fireworks.ai/inference/v1", false),
        );
        let r1 = ProviderReload::from_providers(&a);
        let r2 = ProviderReload::from_providers(&a);
        assert_eq!(r1.revision(), r2.revision());
        assert!(!r1.changed_from(Some(r1.revision())));
        assert!(r1.changed_from(None), "first reload is always a change");
        assert_eq!(r1.bindings().len(), 1);

        // A changed endpoint moves the revision.
        let mut b = a.clone();
        b.get_mut("fireworks").unwrap().base_url = "https://api.fireworks.ai/inference/v1/".into();
        assert!(
            !ProviderReload::from_providers(&b).changed_from(Some(r1.revision())),
            "a trailing slash normalizes away, so this is NOT a change"
        );

        let mut c = a.clone();
        c.get_mut("fireworks").unwrap().allow_insecure_http = false;
        c.insert(
            "other".to_string(),
            provider("other", "https://other.example/v1", false),
        );
        let r3 = ProviderReload::from_providers(&c);
        assert!(
            r3.changed_from(Some(r1.revision())),
            "adding a provider is a change"
        );
        assert_eq!(r3.bindings().len(), 2);

        // A limits change alone is a change.
        let mut d = a.clone();
        d.get_mut("fireworks")
            .unwrap()
            .broker_limits
            .reserved_token_units_per_session = 1;
        assert!(ProviderReload::from_providers(&d).changed_from(Some(r1.revision())));
    }

    // A malformed base URL must not panic the pure signal; it contributes an empty endpoint.
    #[test]
    fn provider_reload_tolerates_a_malformed_base_url() {
        let mut a = BTreeMap::new();
        a.insert("bad".to_string(), provider("bad", "not a url", false));
        let reload = ProviderReload::from_providers(&a);
        assert_eq!(reload.bindings().len(), 1);
        assert_eq!(reload.bindings()[0].base_url, "");
    }
}
