//! Rhapsody Teams memory, `memory.backend: hindsight` — the shared cloud bank
//! behind the same [`MemoryBackend`] trait (STUDIO-660, slice T8; design record
//! `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §5 and §5.4).
//!
//! §5.4 calls this "the *cloud upgrade* — valuable for a bank shared across
//! machines — not a prerequisite for memory existing at all". Everything that
//! makes memory *good* stays where it was: STUDIO-569's retention rules, the
//! read-time content cap, `top_k`, and §5.2's re-grounding are **policy above
//! the trait** and do not vary by backend. Only storage and lookup move, so
//! only storage and lookup are here.
//!
//! # The wire contract is the deployed service's, not the design's sketch
//!
//! §5's backend table describes hindsight as "MCP direct" and §2.2's schema
//! spells `memory.endpoint` with a `/mcp/` suffix. The service actually deployed
//! at `https://hindsight.yak-saturation.ts.net` (STUDIO-629) is **Hindsight HTTP
//! API 0.9.1**, a FastAPI app whose own `/openapi.json` is the contract this
//! module implements, and where the two disagree the deployment wins (the T8
//! ticket says so in as many words). What that pinned, for the record:
//!
//! | §5 says | `/openapi.json` says |
//! | --- | --- |
//! | bank `<bank_prefix><name>`, `default` tenant | `/v1/default/banks/{bank_id}/…` — matches |
//! | recall `types:["experience"]`, source facts | `POST …/memories/recall`, `RecallRequest.types` + `include.source_facts` — matches |
//! | invalidate `PATCH {"state":"invalidated","reason":…}` | `PATCH …/memories/{id}`, `UpdateMemoryRequest{state,reason}` — matches, **and `state:"valid"` reverts** |
//! | `enable_observations: false` | a *bank* setting, not a per-retain one: `CreateBankRequest.enable_observations` |
//! | — | **recall has no `top_k`/`limit`**, only `budget`/`max_tokens`, so `Query::top_k` is applied client-side |
//! | — | **every `/v1/**` path requires an `Authorization` header**; without one the service answers `401 {"detail":"Authentication failed: Invalid API key"}` |
//!
//! The last row is why [`Memory::api_key`](crate::teams::Memory::api_key)
//! exists: §2.2's sketch has no credential field, and an endpoint alone cannot
//! reach a bank.
//!
//! # This type may never be reached from the dispatch path
//!
//! [`crate::memory`]'s module docs give the rule: `dispatch_issue` runs inline on
//! the single control task and is `fn`, not `async fn`, so it holds the concrete
//! [`LocalBank`](crate::memory::LocalBank) and *cannot* hold a
//! `dyn MemoryBackend`. This backend is HTTP over a tailnet — the exact stall the
//! rule exists for — so it is reachable only from off-loop callers: the daemon's
//! `/api/v1/teams/*` handlers, the `teams_*` MCP tools, and the orchestrator's
//! prefetch task, which fills the turn-1 fact slot *ahead of* dispatch rather
//! than during it.
//!
//! # Every failure is a degradation
//!
//! A retain is "best-effort and never fatal" (§5.1) and a recall failure costs a
//! prompt its memory section, never a run. So the timeouts here are short and
//! explicit ([`REQUEST_TIMEOUT`], [`CONNECT_TIMEOUT`]) rather than reqwest's
//! default of none: with the tailnet down, the correct outcome is a quick error
//! the caller logs, not a task parked on a connect that will never complete.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::SecondsFormat;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::memory::{
    Fact, MAX_FACT_CONTENT_BYTES, MAX_RETAIN_CONTENT_BYTES, MemoryBackend, MemoryError, Query,
    Recalled, Record, STATE_INVALIDATED, STATE_VALID, bank_id_for, truncate_bytes,
};

/// The tenant every Rhapsody bank lives in (§5's backend table, and the only
/// tenant the deployed API exposes — every path is literally `/v1/default/…`).
pub const TENANT: &str = "default";

/// The whole-request timeout. Short and explicit: a recall that has not answered
/// in this long has already missed the prefetch cycle it was fired for, and the
/// prompt is better off without the section than the task is parked.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);

/// The connect timeout, kept well under [`REQUEST_TIMEOUT`] so "the tailnet is
/// down" fails fast and distinctly from "the bank is slow".
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// `RecallRequest.types` — §5.2: "`types: ["experience"]`", because `world` "is
/// where laundered conclusions live".
pub const FACT_TYPE_EXPERIENCE: &str = "experience";

/// `RecallRequest.max_tokens`. The API's own default, restated so the request
/// shape is pinned by this crate's tests rather than by a server default that
/// could move underneath a released daemon. The answer is bounded again by
/// [`Query::top_k`] and by [`MAX_FACT_CONTENT_BYTES`] on the way out.
pub const RECALL_MAX_TOKENS: i64 = 4096;

/// The `Authorization` scheme prefixed to a bare credential. An
/// `memory.api_key` that already names a scheme is sent verbatim
/// ([`authorization_value`]), so an operator whose deployment wants something
/// else is not locked out by this default.
const BEARER_PREFIX: &str = "Bearer ";

/// Schemes recognised as "the operator already wrote a full header value".
const KNOWN_SCHEMES: [&str; 3] = ["bearer ", "token ", "apikey "];

/// How much of an error body is quoted back in a [`MemoryError`]. Enough to see
/// hindsight's `{"detail": …}`, short enough that a stray HTML error page does
/// not become the log line.
const MAX_ERROR_BODY: usize = 300;

/// `memory.backend: hindsight` — the remote bank, behind the same trait (§5.4).
///
/// Holds one [`reqwest::Client`] (and so one connection pool) for the daemon's
/// lifetime, which is what keeps a recall to a warm bank one round trip rather
/// than a fresh TLS handshake.
#[derive(Debug)]
pub struct HindsightBackend {
    /// The service base, normalized: scheme + authority + any path prefix, with
    /// no trailing slash and no `/mcp` tail. Every request is this plus
    /// `/v1/default/banks/…`.
    base: String,
    bank_prefix: String,
    /// Per-identity bank-id overrides from the roster's `bank:` field, resolved
    /// by exactly [`bank_id_for`] — the same function
    /// [`LocalBank`](crate::memory::LocalBank) uses, so switching backends
    /// cannot silently move an identity's bank.
    banks: HashMap<String, String>,
    /// The `Authorization` header value, already scheme-prefixed. Empty ⇒ the
    /// header is not sent at all.
    authorization: String,
    http: reqwest::Client,
    /// Bank ids whose `enable_observations: false` has already been applied in
    /// this process (§5's backend table). Guards a once-per-bank config write,
    /// never held across an `await`.
    configured: Mutex<HashSet<String>>,
}

impl HindsightBackend {
    /// Builds a backend against `endpoint`, with `api_key` resolved through the
    /// `$NAME` environment indirection [`crate::resolve::resolve_var`] applies to
    /// `tracker.api_key`.
    ///
    /// **Creates nothing and dials nothing** — the T1/T2 "never create anything
    /// on read" rule, carried into the remote backend: constructing this makes no
    /// request, so a daemon that boots with `backend: hindsight` and never runs a
    /// teammate never touches the service. The first request is a retain, a
    /// recall or an invalidate.
    ///
    /// Fails only on an endpoint this module could not turn into a URL; every
    /// other failure is a per-call degradation, not a construction error.
    pub fn new(
        endpoint: &str,
        bank_prefix: impl Into<String>,
        api_key: &str,
    ) -> Result<Self, MemoryError> {
        let base = normalize_base(endpoint)?;
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| MemoryError::Invalid(format!("build hindsight http client: {e}")))?;
        Ok(Self {
            base,
            bank_prefix: bank_prefix.into(),
            banks: HashMap::new(),
            authorization: authorization_value(&crate::resolve::resolve_var(api_key)),
            http,
            configured: Mutex::new(HashSet::new()),
        })
    }

    /// Honours the roster's per-identity `bank:` overrides, filtered by exactly
    /// [`LocalBank::with_bank_overrides`](crate::memory::LocalBank::with_bank_overrides)'s
    /// rule: a bank id that is not label-safe is dropped rather than joined,
    /// because here it becomes a URL path segment.
    pub fn with_bank_overrides<I, K, V>(mut self, overrides: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (identity, bank) in overrides {
            let (identity, bank) = (identity.into(), bank.into());
            if !bank.is_empty() && crate::teams::is_label_safe(&bank) {
                self.banks.insert(identity, bank);
            }
        }
        self
    }

    /// The normalized service base every request is built from.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The bank id `identity`'s facts live in — the roster override, else
    /// `<bank_prefix><name>`. Identical to
    /// [`LocalBank::bank_id`](crate::memory::LocalBank::bank_id) by construction.
    pub fn bank_id(&self, identity: &str) -> String {
        bank_id_for(&self.bank_prefix, &self.banks, identity)
    }

    /// The bank id, refusing an identity that is not label-safe.
    ///
    /// `identity` reaches this from an MCP tool argument as well as from a
    /// validated roster, and here it becomes a **URL path segment** — so the
    /// charset is checked rather than trusted, exactly as
    /// [`LocalBank::bank_dir`](crate::memory::LocalBank::bank_dir) checks it
    /// before it becomes a directory name.
    fn checked_bank_id(&self, identity: &str) -> Result<String, MemoryError> {
        if !crate::teams::is_label_safe(identity) {
            return Err(MemoryError::Invalid(format!(
                "identity {identity:?} is not label-safe (must match ^[a-z][a-z0-9-]*$)"
            )));
        }
        Ok(self.bank_id(identity))
    }

    /// `<base>/v1/default/banks/<bank>`.
    fn bank_url(&self, bank: &str) -> String {
        format!("{}/v1/{TENANT}/banks/{bank}", self.base)
    }

    /// Applies the `Authorization` header when there is one.
    fn authorized(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.authorization.is_empty() {
            rb
        } else {
            rb.header(reqwest::header::AUTHORIZATION, &self.authorization)
        }
    }

    /// Sends `rb` and returns the decoded JSON body, or a [`MemoryError`] naming
    /// the status and a bounded slice of the body.
    ///
    /// `not_found_is_none` distinguishes the two ways a 404 is read: for an
    /// invalidate it means "no such fact" and must surface as
    /// [`MemoryError::NotFound`]; for the bank-config probe it means "the bank
    /// does not exist yet", which is a normal first-write state.
    async fn send(
        &self,
        rb: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<HindsightResponse, MemoryError> {
        let resp = self
            .authorized(rb)
            .send()
            .await
            .map_err(|e| MemoryError::Io(format!("{what}: {e}")))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Ok(HindsightResponse {
            status: status.as_u16(),
            body,
        })
    }

    /// Applies §5's `enable_observations: false` to `bank`, once per process.
    ///
    /// §5 chose it deliberately: consolidation is the one capability `local`
    /// gives up (§5.3 says why that is acceptable), and turning it on for
    /// hindsight would mean the two backends no longer store the same thing —
    /// "the consolidation/observation features of hindsight" are named as
    /// explicitly out of scope for this slice.
    ///
    /// `PATCH` first because the OpenAPI is explicit that it updates "only
    /// provided fields", where `PUT` "auto-fills missing fields with defaults" —
    /// so a bank a human has given a mission keeps it. A `PUT` follows only when
    /// the `PATCH` reports the bank does not exist yet.
    ///
    /// **Best-effort**: a failure is returned to the caller as a reason to log,
    /// never a reason to skip the retain. A bank with consolidation left on
    /// still stores the fact; the fact is what the run produced.
    async fn ensure_bank_config(&self, bank: &str) -> Result<(), MemoryError> {
        // Two short critical sections around the await rather than one across it:
        // a `std::sync::MutexGuard` held over `.await` is not `Send` and would
        // deadlock the task that re-enters. Two tasks racing the same new bank
        // simply both send the idempotent PATCH.
        {
            let seen = self
                .configured
                .lock()
                .map_err(|_| MemoryError::Io("hindsight bank-config lock poisoned".to_string()))?;
            if seen.contains(bank) {
                return Ok(());
            }
        }
        let url = self.bank_url(bank);
        let body = json!({ "enable_observations": false });
        let patched = self
            .send(
                self.http.patch(&url).json(&body),
                "hindsight configure bank",
            )
            .await?;
        if patched.status == 404 {
            self.send(self.http.put(&url).json(&body), "hindsight create bank")
                .await?
                .ok(&format!("create bank {bank}"))?;
        } else {
            patched.ok(&format!("configure bank {bank}"))?;
        }
        if let Ok(mut seen) = self.configured.lock() {
            seen.insert(bank.to_string());
        }
        Ok(())
    }

    /// The reversal §5.3 requires, and the deployed API confirms:
    /// `UpdateMemoryRequest.state` documents `'valid'` as the revert of
    /// `'invalidated'`, with nothing deleted in between.
    ///
    /// Exposed as its own method for exactly the reason
    /// [`LocalBank::revalidate`](crate::memory::LocalBank::revalidate) is, so
    /// "reversible" is a property this code has rather than one the wire format
    /// merely permits.
    pub async fn revalidate(&self, identity: &str, fact_id: &str) -> Result<bool, MemoryError> {
        let bank = self.checked_bank_id(identity)?;
        let id = checked_fact_id(fact_id)?;
        let url = format!("{}/memories/{id}", self.bank_url(&bank));
        self.send(
            self.http.patch(&url).json(&json!({ "state": STATE_VALID })),
            "hindsight revalidate",
        )
        .await?
        .ok_or_not_found(
            "revalidate",
            &format!("no fact {fact_id:?} in bank {bank:?}"),
        )?;
        Ok(true)
    }

    /// `GET …/memories/list` — the browse path (STUDIO-652's "show me what this
    /// teammate remembers").
    ///
    /// [`Query::browse`] with no terms has no query to score against, and
    /// hindsight's recall *requires* a query string, so a browse cannot go
    /// through search: it goes through the list endpoint, filtered to valid
    /// experience facts and bounded by `top_k`.
    ///
    /// `ListMemoryUnitsResponse.items` is the one shape the OpenAPI leaves
    /// untyped (`object` with `additionalProperties: true`), so the mapping reads
    /// the same field names `RecallResult` uses and treats every one of them as
    /// optional — an item that names no text is skipped rather than rendered
    /// blank.
    async fn browse(
        &self,
        bank: &str,
        identity: &str,
        top_k: usize,
    ) -> Result<Recalled, MemoryError> {
        let url = format!("{}/memories/list", self.bank_url(bank));
        let resp = self
            .send(
                self.http.get(&url).query(&[
                    ("type", FACT_TYPE_EXPERIENCE.to_string()),
                    ("state", STATE_VALID.to_string()),
                    ("limit", top_k.to_string()),
                ]),
                "hindsight browse",
            )
            .await?
            .ok("browse")?;
        let listed: ListResponse = decode_json(&resp, "browse")?;
        let mut out = Recalled::default();
        for item in listed.items.into_iter().take(top_k) {
            match fact_from_value(&item, identity) {
                Some(f) => out.facts.push(f),
                None => out
                    .skipped
                    .push((String::new(), "list item names no id or text".to_string())),
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl MemoryBackend for HindsightBackend {
    /// `POST …/memories` with one `MemoryItem`.
    ///
    /// §5.1's provenance travels as the item's `metadata`, which the deployed
    /// schema types as `{string: string}` — so all five host-stamped fields go
    /// over as strings, present even when empty, mirroring what `local` writes
    /// into front matter. `document_id` carries §5.1's `run-<run_id>`.
    ///
    /// `async: false`, so the call returns only once the fact is stored: a
    /// fire-and-forget retain would report success for a run whose memory never
    /// landed, and this is already the off-loop path where waiting is free.
    ///
    /// **The returned id is the `document_id` we supplied, not a fact id.**
    /// Hindsight *extracts* facts from the content, so one retain can produce
    /// several facts and the response (`RetainResponse`) names none of them; the
    /// document is the only stable handle the caller gave and can name again.
    async fn retain(&self, rec: &Record) -> Result<String, MemoryError> {
        let bank = self.checked_bank_id(&rec.identity)?;
        // Best-effort and never fatal to the retain (§5.1): a bank whose
        // consolidation could not be switched off still stores the fact.
        if let Err(e) = self.ensure_bank_config(&bank).await {
            tracing::warn!(
                bank = %bank,
                error = %e,
                "hindsight: could not apply enable_observations=false to the bank; retaining \
                 anyway (a bank with consolidation left on still stores the fact)"
            );
        }
        let url = format!("{}/memories", self.bank_url(&bank));
        let body = json!({
            "items": [{
                "content": truncate_bytes(&rec.content, MAX_RETAIN_CONTENT_BYTES),
                "timestamp": rec.at.to_rfc3339_opts(SecondsFormat::Secs, true),
                "document_id": rec.document_id,
                "metadata": {
                    "identity": rec.identity,
                    "ticket": rec.ticket,
                    "commit_sha": rec.commit_sha,
                    "pr": rec.pr,
                    "run_id": rec.run_id,
                },
            }],
            "async": false,
        });
        self.send(self.http.post(&url).json(&body), "hindsight retain")
            .await?
            .ok("retain")?;
        Ok(rec.document_id.clone())
    }

    /// `POST …/memories/recall` with §5.2's two overrides — `types:
    /// ["experience"]` and `include: {source_facts: {}}` — both of which §5.2
    /// says are wrong by default for this use.
    ///
    /// **`top_k` is applied here, not by the service.** The deployed
    /// `RecallRequest` has no `top_k` or `limit`, only `budget` and `max_tokens`,
    /// so the "every recalled byte is turn-1 prompt cost, forever" bound is
    /// enforced on the response — which is where `local` enforces it too.
    ///
    /// `source_facts` is requested because §5.2 requires it, and is normally
    /// empty under `types: ["experience"]`: it carries the sources of
    /// *observation* results, and this recall asks for none. It is sent anyway
    /// rather than conditionally, so the request shape does not quietly change
    /// if a future slice ever asks for observations.
    async fn recall(&self, identity: &str, q: &Query) -> Result<Recalled, MemoryError> {
        let bank = self.checked_bank_id(identity)?;
        let top_k = effective_top_k(q);
        let query = query_text(q);
        if query.is_empty() {
            // A browse with no terms, or a query whose every term was empty. The
            // recall endpoint requires a query string, so the honest answer to
            // "what does this teammate remember" comes from the list endpoint.
            return if q.browse {
                self.browse(&bank, identity, top_k).await
            } else {
                Ok(Recalled::default())
            };
        }
        let url = format!("{}/memories/recall", self.bank_url(&bank));
        let body = json!({
            "query": query,
            "types": [FACT_TYPE_EXPERIENCE],
            "include": { "source_facts": {} },
            "max_tokens": RECALL_MAX_TOKENS,
        });
        let resp = self
            .send(self.http.post(&url).json(&body), "hindsight recall")
            .await?
            .ok("recall")?;
        let recalled: RecallResponse = decode_json(&resp, "recall")?;
        let mut out = Recalled::default();
        for r in recalled.results.into_iter().take(top_k) {
            out.facts.push(r.into_fact(identity));
        }
        Ok(out)
    }

    /// `PATCH …/memories/{id}` with `{"state":"invalidated","reason":…}` — §5.3's
    /// path verbatim, and the one the design says is confirmed: the 400 the
    /// ticket warned about came from the Go client's reason-less body, which this
    /// never sends.
    ///
    /// The record is not deleted, so this is reversible ([`revalidate`]).
    /// `Ok(false)` ⇒ already invalidated, which is read from the fact's own
    /// `state` before the PATCH so the answer matches `local`'s. When that read
    /// cannot answer, the PATCH goes ahead: doing the work is the safe side of
    /// that particular doubt.
    ///
    /// [`revalidate`]: HindsightBackend::revalidate
    async fn invalidate(
        &self,
        identity: &str,
        fact_id: &str,
        reason: &str,
    ) -> Result<bool, MemoryError> {
        let bank = self.checked_bank_id(identity)?;
        let id = checked_fact_id(fact_id)?;
        let url = format!("{}/memories/{id}", self.bank_url(&bank));
        let current = self
            .send(self.http.get(&url), "hindsight read fact")
            .await?;
        if current.status == 404 {
            return Err(MemoryError::NotFound(format!(
                "no fact {fact_id:?} in bank {bank:?}"
            )));
        }
        if current.status < 400
            && let Ok(v) = serde_json::from_str::<Value>(&current.body)
            && v.get("state").and_then(Value::as_str) == Some(STATE_INVALIDATED)
        {
            return Ok(false);
        }
        self.send(
            self.http.patch(&url).json(&json!({
                "state": STATE_INVALIDATED,
                "reason": reason,
            })),
            "hindsight invalidate",
        )
        .await?
        .ok_or_not_found(
            "invalidate",
            &format!("no fact {fact_id:?} in bank {bank:?}"),
        )?;
        Ok(true)
    }
}

/// One raw HTTP answer, kept as status + text so a non-2xx can quote the body
/// back and a 404 can be branched on before anything is parsed.
struct HindsightResponse {
    status: u16,
    body: String,
}

impl HindsightResponse {
    /// The body, or an [`MemoryError::Io`] naming the status and a bounded slice
    /// of what the service said.
    fn ok(self, what: &str) -> Result<String, MemoryError> {
        if self.status < 400 {
            return Ok(self.body);
        }
        Err(MemoryError::Io(format!(
            "hindsight {what}: HTTP {} — {}",
            self.status,
            truncate_bytes(self.body.trim(), MAX_ERROR_BODY)
        )))
    }

    /// As [`ok`](Self::ok), but a 404 becomes [`MemoryError::NotFound`] so the
    /// caller can tell "no such fact" from "the service is unwell".
    fn ok_or_not_found(self, what: &str, missing: &str) -> Result<String, MemoryError> {
        if self.status == 404 {
            return Err(MemoryError::NotFound(missing.to_string()));
        }
        self.ok(what)
    }
}

/// Decodes a JSON body, naming the call in the error rather than leaking a bare
/// serde message with no context.
fn decode_json<T: serde::de::DeserializeOwned>(body: &str, what: &str) -> Result<T, MemoryError> {
    serde_json::from_str(body)
        .map_err(|e| MemoryError::Invalid(format!("hindsight {what}: decode response: {e}")))
}

/// `RecallResponse` — only the field this backend reads. Unknown fields (traces,
/// entities, chunks, `source_facts`) are ignored rather than rejected, so a
/// service that grows a field does not break a released daemon.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RecallResponse {
    results: Vec<RecallResult>,
}

/// `RecallResult` — the fields §5.1's provenance and §5.2's rendering need.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RecallResult {
    id: String,
    text: String,
    document_id: Option<String>,
    mentioned_at: Option<String>,
    occurred_start: Option<String>,
    metadata: Option<HashMap<String, String>>,
}

impl RecallResult {
    /// Maps one result onto the **same plain-data [`Fact`]** `local` produces —
    /// the composer must not be able to tell which backend a fact came from
    /// (§5.2, and T4's stated design test).
    ///
    /// `state` is [`STATE_VALID`] by construction: §5.3 records that hindsight's
    /// `readableByModel` "refuses **any** non-`valid` state", so a recall cannot
    /// return an invalidated fact and there is no `reason` to carry either.
    fn into_fact(self, identity: &str) -> Fact {
        let md = self.metadata.unwrap_or_default();
        let get = |k: &str| md.get(k).cloned().unwrap_or_default();
        // The bank we asked is the identity's, so `identity` is authoritative;
        // the metadata copy is only a fallback for a fact retained before the
        // stamp existed.
        let stamped = get("identity");
        Fact {
            id: self.id,
            identity: if stamped.is_empty() {
                identity.to_string()
            } else {
                stamped
            },
            document_id: self.document_id.unwrap_or_default(),
            ticket: get("ticket"),
            commit_sha: get("commit_sha"),
            pr: get("pr"),
            run_id: get("run_id"),
            at: self
                .mentioned_at
                .or(self.occurred_start)
                .unwrap_or_default(),
            state: STATE_VALID.to_string(),
            reason: String::new(),
            // The read-time cap, applied exactly where `local` applies it: the
            // caps are policy above the trait and do not vary by backend.
            content: truncate_bytes(&self.text, MAX_FACT_CONTENT_BYTES),
        }
    }
}

/// `ListMemoryUnitsResponse` — `items` is untyped in the OpenAPI, so it stays
/// [`Value`] and is mapped defensively by [`fact_from_value`].
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ListResponse {
    items: Vec<Value>,
}

/// Maps one untyped list item onto a [`Fact`], or `None` when it names neither
/// an id nor text — which is the only way to tell a memory unit from whatever
/// else a future `items` might carry.
fn fact_from_value(v: &Value, identity: &str) -> Option<Fact> {
    let id = v.get("id").and_then(Value::as_str).unwrap_or_default();
    let text = v.get("text").and_then(Value::as_str).unwrap_or_default();
    if id.is_empty() || text.is_empty() {
        return None;
    }
    let result = RecallResult {
        id: id.to_string(),
        text: text.to_string(),
        document_id: v
            .get("document_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        mentioned_at: v
            .get("mentioned_at")
            .and_then(Value::as_str)
            .map(str::to_string),
        occurred_start: v
            .get("occurred_start")
            .and_then(Value::as_str)
            .map(str::to_string),
        metadata: v
            .get("metadata")
            .and_then(|m| serde_json::from_value(m.clone()).ok()),
    };
    Some(result.into_fact(identity))
}

/// The search text one recall sends: the ticket, its title and its labels, in
/// that order.
///
/// Hindsight's recall is a hybrid semantic/keyword search over one string, so
/// [`Query`]'s three match fields are joined rather than mapped onto separate
/// filters — the same three fields `local`'s scorer reads, in the same order, so
/// the two backends are asked the same question even though they answer it
/// differently.
fn query_text(q: &Query) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(2 + q.labels.len());
    for s in [q.ticket.as_str(), q.title.as_str()] {
        if !s.trim().is_empty() {
            parts.push(s.trim());
        }
    }
    for l in &q.labels {
        if !l.trim().is_empty() {
            parts.push(l.trim());
        }
    }
    parts.join(" ")
}

/// [`Query::top_k`] with the non-positive fallback applied. Restated here rather
/// than reached through `Query`'s private helper for the reason the constant
/// itself is restated: `top_k: 0` must not mean "everything" against one backend
/// and "nothing" against another.
fn effective_top_k(q: &Query) -> usize {
    if q.top_k == 0 {
        crate::memory::FALLBACK_TOP_K
    } else {
        q.top_k
    }
}

/// Normalizes `memory.endpoint` into a request base.
///
/// Accepts both spellings the design and the deployment use: §2.2's example ends
/// in `/mcp/`, while the REST surface this module speaks lives at `/v1/…` on the
/// same origin — so a trailing `/mcp` segment is stripped rather than refused,
/// and an operator who copied the schema's example still gets a working daemon.
fn normalize_base(endpoint: &str) -> Result<String, MemoryError> {
    let e = endpoint.trim();
    if e.is_empty() {
        return Err(MemoryError::Invalid(
            "memory.endpoint is empty (backend `hindsight` needs one)".to_string(),
        ));
    }
    if !(e.starts_with("http://") || e.starts_with("https://")) {
        return Err(MemoryError::Invalid(format!(
            "memory.endpoint {endpoint:?} is not an http(s) URL"
        )));
    }
    if e.contains('?') || e.contains('#') {
        return Err(MemoryError::Invalid(format!(
            "memory.endpoint {endpoint:?} must be a base URL (no query or fragment)"
        )));
    }
    let mut base = e.trim_end_matches('/');
    if let Some(stripped) = base.strip_suffix("/mcp") {
        base = stripped.trim_end_matches('/');
    }
    if base.len() <= "https://".len() {
        return Err(MemoryError::Invalid(format!(
            "memory.endpoint {endpoint:?} names no host"
        )));
    }
    Ok(base.to_string())
}

/// The `Authorization` header value for a credential.
///
/// Empty stays empty (no header). A value that already names a scheme is sent
/// verbatim; anything else is a bare key and gets `Bearer `. The deployed
/// service documents the header only as an opaque string, so the common case is
/// defaulted and the uncommon one is left to the operator rather than guessed at
/// per-deployment.
fn authorization_value(api_key: &str) -> String {
    let k = api_key.trim();
    if k.is_empty() {
        return String::new();
    }
    let lower = k.to_ascii_lowercase();
    if KNOWN_SCHEMES.iter().any(|s| lower.starts_with(s)) {
        return k.to_string();
    }
    format!("{BEARER_PREFIX}{k}")
}

/// Refuses a fact id that could leave its bank once interpolated into a URL
/// path. The mirror of
/// [`record_path`](crate::memory)'s guard, for the same reason: `fact_id` arrives
/// from an MCP tool argument and a dashboard button.
fn checked_fact_id(fact_id: &str) -> Result<String, MemoryError> {
    let ok = !fact_id.is_empty()
        && !fact_id.contains("..")
        && fact_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        return Err(MemoryError::Invalid(format!(
            "fact_id {fact_id:?} is not a fact id (expected [A-Za-z0-9_-.]+ with no \"..\")"
        )));
    }
    Ok(fact_id.to_string())
}

#[cfg(test)]
mod tests {
    //! Hermetic: every test here answers from a loopback stub bound on an
    //! ephemeral port, in the shape `rhapsody-tracker`'s `linear::testutil`
    //! mock server established (a hand-rolled HTTP/1.1 reader + writer, no
    //! framework). Nothing in this module dials a real service — the ONE live
    //! check the design asks for is `harness/hindsight/smoke.rs`, run by hand.
    //!
    //! What these pin is the **request shape**, because that is the part the
    //! deployed OpenAPI decides and this crate can get silently wrong: the
    //! tenant, the bank id, the `types` filter, the invalidate PATCH's body with
    //! its reason. A test that only asserted on our own response mapping would
    //! stay green through a request that hindsight rejects.

    use super::*;
    use crate::teams::Identity;
    use chrono::{DateTime, Utc};
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// One request the stub saw, decomposed far enough to assert on.
    #[derive(Debug, Clone, Default)]
    struct Seen {
        method: String,
        path: String,
        query: String,
        authorization: String,
        body: Value,
    }

    /// A canned reply.
    #[derive(Debug, Clone)]
    struct Reply {
        status: u16,
        body: String,
    }

    impl Reply {
        fn ok(body: &str) -> Self {
            Reply {
                status: 200,
                body: body.to_string(),
            }
        }

        fn status(status: u16, body: &str) -> Self {
            Reply {
                status,
                body: body.to_string(),
            }
        }
    }

    type Route = Arc<dyn Fn(&Seen) -> Reply + Send + Sync>;

    /// A loopback hindsight stub that records every request and answers from
    /// `route`. Lives until dropped, which aborts its accept loop.
    struct Stub {
        url: String,
        seen: Arc<StdMutex<Vec<Seen>>>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for Stub {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    impl Stub {
        async fn start<F>(route: F) -> Self
        where
            F: Fn(&Seen) -> Reply + Send + Sync + 'static,
        {
            let route: Route = Arc::new(route);
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            let seen: Arc<StdMutex<Vec<Seen>>> = Arc::new(StdMutex::new(Vec::new()));
            let recorder = Arc::clone(&seen);
            let handle = tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let req = read_request(&mut stream).await;
                    let reply = route(&req);
                    if let Ok(mut v) = recorder.lock() {
                        v.push(req);
                    }
                    write_response(&mut stream, &reply).await;
                }
            });
            Stub {
                url: format!("http://{addr}"),
                seen,
                handle,
            }
        }

        /// A stub that 200s everything with `{}` — enough for the shape tests
        /// that only care what was SENT.
        async fn accepting() -> Self {
            Self::start(|_| Reply::ok("{}")).await
        }

        fn requests(&self) -> Vec<Seen> {
            self.seen.lock().expect("seen").clone()
        }

        /// The first recorded request whose method and path suffix match.
        fn request(&self, method: &str, path_suffix: &str) -> Seen {
            self.requests()
                .into_iter()
                .find(|r| r.method == method && r.path.ends_with(path_suffix))
                .unwrap_or_else(|| {
                    panic!(
                        "no {method} {path_suffix} in {:?}",
                        self.requests()
                            .iter()
                            .map(|r| format!("{} {}", r.method, r.path))
                            .collect::<Vec<_>>()
                    )
                })
        }
    }

    async fn read_request(stream: &mut TcpStream) -> Seen {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        let header_end = loop {
            let n = stream.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                break buf.len();
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default().to_string();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or_default().to_string();
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (target, String::new()),
        };
        let authorization = head
            .lines()
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.trim().to_string())
            .unwrap_or_default();
        let content_length = head
            .lines()
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while buf.len() - header_end < content_length {
            let n = stream.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        let end = (header_end + content_length).min(buf.len());
        let body = serde_json::from_slice(&buf[header_end..end]).unwrap_or(Value::Null);
        Seen {
            method,
            path,
            query,
            authorization,
            body,
        }
    }

    async fn write_response(stream: &mut TcpStream, reply: &Reply) {
        let payload = format!(
            "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: \
             close\r\n\r\n{}",
            reply.status,
            reply.body.len(),
            reply.body
        );
        let _ = stream.write_all(payload.as_bytes()).await;
        let _ = stream.flush().await;
    }

    fn backend(stub: &Stub) -> HindsightBackend {
        HindsightBackend::new(&stub.url, "agent-", "k-123").expect("backend")
    }

    fn at() -> DateTime<Utc> {
        DateTime::from_timestamp(1_756_000_000, 0).expect("timestamp")
    }

    fn record(identity: &str, content: &str) -> Record {
        Record {
            identity: identity.to_string(),
            document_id: "run-412".to_string(),
            ticket: "STUDIO-660".to_string(),
            commit_sha: "abc1234".to_string(),
            pr: "57".to_string(),
            run_id: "412".to_string(),
            at: at(),
            content: content.to_string(),
        }
    }

    fn ticket_query(ticket: &str) -> Query {
        Query {
            ticket: ticket.to_string(),
            top_k: 8,
            ..Query::default()
        }
    }

    /// One recall result, as the deployed `RecallResult` shapes it.
    fn recall_body(text: &str) -> String {
        json!({
            "results": [{
                "id": "fact-1",
                "text": text,
                "type": "experience",
                "document_id": "run-412",
                "mentioned_at": "2026-08-29T17:45:00Z",
                "metadata": {
                    "identity": "alice",
                    "ticket": "STUDIO-660",
                    "commit_sha": "abc1234",
                    "pr": "57",
                    "run_id": "412",
                },
                "scores": { "final": 0.9 },
            }],
            "entities": null,
            "source_facts": {},
        })
        .to_string()
    }

    // ── the request shape the deployed OpenAPI decides ──────────────────────────────────────────

    /// §5's backend table, pinned against the wire: the `default` tenant, the
    /// `<bank_prefix><name>` bank id, and the credential the deployed service
    /// refuses every `/v1/**` request without.
    #[tokio::test]
    async fn retain_posts_to_the_default_tenant_and_the_prefixed_bank() {
        let stub = Stub::accepting().await;
        let b = backend(&stub);
        let id = b.retain(&record("alice", "shipped the prefetch")).await;
        assert_eq!(
            id.expect("retain"),
            "run-412",
            "the document_id is the handle"
        );
        let req = stub.request("POST", "/memories");
        assert_eq!(req.path, "/v1/default/banks/agent-alice/memories");
        assert_eq!(req.authorization, "Bearer k-123");
        let item = &req.body["items"][0];
        assert_eq!(item["content"], "shipped the prefetch");
        assert_eq!(item["document_id"], "run-412");
        assert_eq!(
            req.body["async"], false,
            "a fire-and-forget retain would report a fact that never landed"
        );
        // §5.1's five host-stamped provenance fields, all of them, as strings.
        assert_eq!(item["metadata"]["identity"], "alice");
        assert_eq!(item["metadata"]["ticket"], "STUDIO-660");
        assert_eq!(item["metadata"]["commit_sha"], "abc1234");
        assert_eq!(item["metadata"]["pr"], "57");
        assert_eq!(item["metadata"]["run_id"], "412");
    }

    /// §5's `enable_observations: false`, applied to the bank exactly once per
    /// process — and `PATCH` before `PUT`, because the OpenAPI says `PATCH`
    /// updates "only provided fields" where `PUT` "auto-fills missing fields with
    /// defaults" and would flatten a mission a human set.
    #[tokio::test]
    async fn the_bank_is_configured_once_with_observations_off() {
        let stub = Stub::accepting().await;
        let b = backend(&stub);
        b.retain(&record("alice", "one")).await.expect("retain 1");
        b.retain(&record("alice", "two")).await.expect("retain 2");
        let cfg = stub.request("PATCH", "/banks/agent-alice");
        assert_eq!(cfg.body["enable_observations"], false);
        assert_eq!(
            stub.requests()
                .iter()
                .filter(|r| r.method == "PATCH")
                .count(),
            1,
            "the bank config is written once per process, not once per retain"
        );
        assert!(
            !stub.requests().iter().any(|r| r.method == "PUT"),
            "PUT is the 404 fallback only; it must not run when PATCH succeeded"
        );
    }

    /// A bank that does not exist yet: `PATCH` 404s, so it is created with `PUT`
    /// carrying the same body.
    #[tokio::test]
    async fn a_missing_bank_is_created_by_the_put_fallback() {
        let stub = Stub::start(|r| {
            if r.method == "PATCH" && r.path == "/v1/default/banks/agent-alice" {
                Reply::status(404, r#"{"detail":"Bank not found"}"#)
            } else {
                Reply::ok("{}")
            }
        })
        .await;
        backend(&stub)
            .retain(&record("alice", "first ever"))
            .await
            .expect("retain");
        assert_eq!(
            stub.request("PUT", "/banks/agent-alice").body["enable_observations"],
            false
        );
        stub.request("POST", "/memories");
    }

    /// §5.2's two overrides, both of which the section says are wrong by default
    /// for this use — and the absence of a `top_k` field, which is why the bound
    /// is applied to the response instead.
    #[tokio::test]
    async fn recall_asks_for_experience_facts_with_source_facts() {
        let stub =
            Stub::start(|_| Reply::ok(&recall_body("the poller skipped null attachments"))).await;
        let b = backend(&stub);
        let got = b
            .recall(
                "alice",
                &Query {
                    ticket: "STUDIO-660".to_string(),
                    title: "the hindsight memory backend".to_string(),
                    labels: vec!["rust".to_string(), "  ".to_string()],
                    top_k: 8,
                    browse: false,
                },
            )
            .await
            .expect("recall");
        let req = stub.request("POST", "/memories/recall");
        assert_eq!(req.path, "/v1/default/banks/agent-alice/memories/recall");
        assert_eq!(req.body["types"], json!(["experience"]));
        assert_eq!(req.body["include"]["source_facts"], json!({}));
        assert_eq!(req.body["max_tokens"], 4096);
        assert_eq!(
            req.body["query"], "STUDIO-660 the hindsight memory backend rust",
            "ticket, title then labels — the same three fields local scores, blank ones dropped"
        );
        // The SAME plain-data Fact `local` produces: the composer cannot tell.
        let f = &got.facts[0];
        assert_eq!(f.id, "fact-1");
        assert_eq!(f.identity, "alice");
        assert_eq!(f.ticket, "STUDIO-660");
        assert_eq!(f.commit_sha, "abc1234");
        assert_eq!(f.pr, "57");
        assert_eq!(f.run_id, "412");
        assert_eq!(f.document_id, "run-412");
        assert_eq!(f.at, "2026-08-29T17:45:00Z");
        assert_eq!(
            f.state, STATE_VALID,
            "recall cannot return a non-valid fact"
        );
        assert_eq!(f.reason, "");
        assert_eq!(f.content, "the poller skipped null attachments");
    }

    /// §5.3's correction path, verbatim — and the reason, whose absence is
    /// precisely what made the Go client's call 400.
    #[tokio::test]
    async fn invalidate_patches_state_and_reason() {
        let stub = Stub::start(|r| {
            if r.method == "GET" {
                Reply::ok(r#"{"id":"fact-1","state":"valid"}"#)
            } else {
                Reply::ok("{}")
            }
        })
        .await;
        let done = backend(&stub)
            .invalidate(
                "alice",
                "fact-1",
                "STUDIO-408 was Done five days before this ran",
            )
            .await
            .expect("invalidate");
        assert!(done, "a valid fact is invalidated");
        let req = stub.request("PATCH", "/memories/fact-1");
        assert_eq!(req.path, "/v1/default/banks/agent-alice/memories/fact-1");
        assert_eq!(req.body["state"], "invalidated");
        assert_eq!(
            req.body["reason"],
            "STUDIO-408 was Done five days before this ran"
        );
    }

    /// `Ok(false)` ⇒ already invalidated, matching `local`'s answer rather than
    /// reporting work that did not happen.
    #[tokio::test]
    async fn invalidating_twice_reports_no_change() {
        let stub = Stub::start(|r| {
            if r.method == "GET" {
                Reply::ok(r#"{"id":"fact-1","state":"invalidated","reason":"already"}"#)
            } else {
                Reply::ok("{}")
            }
        })
        .await;
        let done = backend(&stub)
            .invalidate("alice", "fact-1", "again")
            .await
            .expect("invalidate");
        assert!(!done);
        assert!(
            !stub.requests().iter().any(|r| r.method == "PATCH"),
            "an already-invalidated fact is not patched again"
        );
    }

    /// The reversal §5.3 requires and `UpdateMemoryRequest.state` documents.
    #[tokio::test]
    async fn revalidate_patches_the_state_back() {
        let stub = Stub::accepting().await;
        backend(&stub)
            .revalidate("alice", "fact-1")
            .await
            .expect("revalidate");
        let req = stub.request("PATCH", "/memories/fact-1");
        assert_eq!(req.body["state"], "valid");
        assert!(
            req.body.get("reason").is_none(),
            "a revert carries no reason"
        );
    }

    /// A fact that is not there is [`MemoryError::NotFound`], not "the service is
    /// unwell" — the dashboard button needs to tell those apart.
    #[tokio::test]
    async fn invalidating_a_missing_fact_is_not_found() {
        let stub = Stub::start(|_| Reply::status(404, r#"{"detail":"Memory not found"}"#)).await;
        let err = backend(&stub)
            .invalidate("alice", "nope", "because")
            .await
            .expect_err("missing");
        assert!(matches!(err, MemoryError::NotFound(_)), "got {err:?}");
    }

    // ── the policy above the trait, which does not vary by backend ───────────────────────────────

    /// [`MAX_FACT_CONTENT_BYTES`] is applied to what hindsight returns, exactly
    /// where `local` applies it: "every recalled byte is turn-1 prompt cost,
    /// forever" is a bound on the prompt, not on any one store.
    #[tokio::test]
    async fn a_recalled_fact_is_truncated_to_the_read_cap() {
        let long = "x".repeat(MAX_FACT_CONTENT_BYTES * 3);
        let stub = Stub::start(move |_| Reply::ok(&recall_body(&long))).await;
        let got = backend(&stub)
            .recall("alice", &ticket_query("STUDIO-660"))
            .await
            .expect("recall");
        assert!(
            got.facts[0].content.len() <= MAX_FACT_CONTENT_BYTES,
            "got {} bytes",
            got.facts[0].content.len()
        );
        assert!(got.facts[0].content.ends_with('…'), "truncation is visible");
    }

    /// [`MAX_RETAIN_CONTENT_BYTES`] is applied on the way in, for the same
    /// reason: §5.1's payload is a constructed record, never a transcript.
    #[tokio::test]
    async fn a_retained_record_is_truncated_to_the_write_cap() {
        let stub = Stub::accepting().await;
        let long = "y".repeat(MAX_RETAIN_CONTENT_BYTES * 2);
        backend(&stub)
            .retain(&record("alice", &long))
            .await
            .expect("retain");
        let sent = stub.request("POST", "/memories").body["items"][0]["content"]
            .as_str()
            .expect("content")
            .to_string();
        assert!(
            sent.len() <= MAX_RETAIN_CONTENT_BYTES,
            "got {} bytes",
            sent.len()
        );
    }

    /// `top_k` has no wire field, so it is enforced on the answer — and a
    /// non-positive `top_k` falls back rather than meaning "nothing".
    #[tokio::test]
    async fn top_k_bounds_the_answer_client_side() {
        let many = json!({
            "results": (0..50)
                .map(|i| json!({"id": format!("f{i}"), "text": "a fact"}))
                .collect::<Vec<_>>()
        })
        .to_string();
        let stub = Stub::start(move |_| Reply::ok(&many)).await;
        let b = backend(&stub);
        let got = b
            .recall(
                "alice",
                &Query {
                    ticket: "STUDIO-660".to_string(),
                    top_k: 3,
                    ..Query::default()
                },
            )
            .await
            .expect("recall");
        assert_eq!(got.facts.len(), 3);
        let fallback = b
            .recall("alice", &ticket_query("STUDIO-660"))
            .await
            .expect("recall");
        assert_eq!(
            fallback.facts.len(),
            8,
            "top_k: 0 ⇒ FALLBACK_TOP_K, never everything"
        );
    }

    /// The roster's `bank:` override is honoured by the SAME resolution
    /// [`LocalBank`](crate::memory::LocalBank) uses, so switching backends cannot
    /// move an identity's bank.
    #[tokio::test]
    async fn a_roster_bank_override_is_honoured() {
        let stub = Stub::accepting().await;
        let b = backend(&stub)
            .with_bank_overrides([("alice", "shared-bank"), ("bob", "not/label/safe")]);
        assert_eq!(b.bank_id("alice"), "shared-bank");
        assert_eq!(
            b.bank_id("bob"),
            "agent-bob",
            "an unsafe override is dropped, not joined"
        );
        let local = crate::memory::LocalBank::new("/tmp/x", "agent-")
            .with_bank_overrides([("alice", "shared-bank"), ("bob", "not/label/safe")]);
        assert_eq!(b.bank_id("alice"), local.bank_id("alice"));
        assert_eq!(b.bank_id("bob"), local.bank_id("bob"));
        b.retain(&record("alice", "x")).await.expect("retain");
        assert_eq!(
            stub.request("POST", "/memories").path,
            "/v1/default/banks/shared-bank/memories"
        );
    }

    /// An identity that is not label-safe becomes a URL path segment here, so it
    /// is refused before a request is built — the mirror of `bank_dir`'s guard.
    #[tokio::test]
    async fn an_unsafe_identity_never_reaches_the_wire() {
        let stub = Stub::accepting().await;
        let b = backend(&stub);
        assert!(matches!(
            b.recall("../../etc", &ticket_query("X")).await,
            Err(MemoryError::Invalid(_))
        ));
        assert!(matches!(
            b.invalidate("alice", "../secret", "why").await,
            Err(MemoryError::Invalid(_))
        ));
        assert!(stub.requests().is_empty(), "nothing was sent");
    }

    // ── degradation: every failure is a logged degradation, never a run failure ──────────────────

    /// A service that answers 500 is an error the caller logs, with the status
    /// and the body it said — not a panic and not a silent empty recall.
    #[tokio::test]
    async fn a_failing_service_is_a_reported_error() {
        let stub =
            Stub::start(|_| Reply::status(503, r#"{"detail":"upstream unavailable"}"#)).await;
        let err = backend(&stub)
            .recall("alice", &ticket_query("STUDIO-660"))
            .await
            .expect_err("503");
        let msg = err.to_string();
        assert!(msg.contains("503"), "{msg}");
        assert!(msg.contains("upstream unavailable"), "{msg}");
    }

    /// The tailnet is down: nothing is listening, and the call fails fast rather
    /// than parking the task that made it.
    #[tokio::test]
    async fn an_unreachable_endpoint_fails_rather_than_hangs() {
        // Bind, capture the port, drop — nothing is listening on it afterwards.
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            l.local_addr().expect("addr").port()
        };
        let b = HindsightBackend::new(&format!("http://127.0.0.1:{port}"), "agent-", "")
            .expect("backend");
        let started = std::time::Instant::now();
        let err = b
            .recall("alice", &ticket_query("STUDIO-660"))
            .await
            .expect_err("nothing listening");
        assert!(matches!(err, MemoryError::Io(_)), "got {err:?}");
        assert!(
            started.elapsed() < REQUEST_TIMEOUT * 2,
            "took {:?}",
            started.elapsed()
        );
    }

    /// A body that is not the shape we expect is an [`MemoryError::Invalid`]
    /// naming the call, not a panic — and unknown fields are ignored, so a
    /// service that grows one does not break a released daemon.
    #[tokio::test]
    async fn an_unexpected_body_is_an_error_and_extra_fields_are_ignored() {
        let stub = Stub::start(|_| Reply::ok("not json at all")).await;
        assert!(matches!(
            backend(&stub).recall("alice", &ticket_query("X")).await,
            Err(MemoryError::Invalid(_))
        ));

        let stub = Stub::start(|_| {
            Reply::ok(r#"{"results":[{"id":"f1","text":"t","brand_new_field":1}],"also_new":true}"#)
        })
        .await;
        let got = backend(&stub)
            .recall("alice", &ticket_query("X"))
            .await
            .expect("tolerant");
        assert_eq!(got.facts.len(), 1);
    }

    // ── construction ────────────────────────────────────────────────────────────────────────────

    /// Constructing dials nothing: the T1/T2 "never create anything on read"
    /// rule, carried into the remote backend.
    #[tokio::test]
    async fn constructing_sends_no_request() {
        let stub = Stub::accepting().await;
        let _b = backend(&stub);
        tokio::task::yield_now().await;
        assert!(stub.requests().is_empty());
    }

    /// §2.2 spells the endpoint with a `/mcp/` suffix and the deployed REST
    /// surface lives at `/v1/` on the same origin, so both spellings work.
    #[test]
    fn the_endpoint_accepts_both_spellings_and_refuses_a_non_url() {
        for (given, want) in [
            (
                "https://hindsight.example.ts.net",
                "https://hindsight.example.ts.net",
            ),
            (
                "https://hindsight.example.ts.net/",
                "https://hindsight.example.ts.net",
            ),
            (
                "https://hindsight.example.ts.net/mcp/",
                "https://hindsight.example.ts.net",
            ),
            ("  http://127.0.0.1:8888/mcp  ", "http://127.0.0.1:8888"),
        ] {
            assert_eq!(normalize_base(given).expect(given), want, "({given:?})");
        }
        for bad in [
            "",
            "   ",
            "hindsight.example.ts.net",
            "ftp://hindsight.example.ts.net",
            "https://hindsight.example.ts.net?x=1",
            "https://",
        ] {
            assert!(normalize_base(bad).is_err(), "({bad:?}) should be refused");
        }
    }

    /// The credential the deployed service requires. A bare key gets `Bearer`; a
    /// value that already names a scheme is sent verbatim; empty sends no header
    /// at all, which is what an unauthenticated deployment wants.
    #[test]
    fn the_authorization_header_defaults_to_bearer() {
        assert_eq!(authorization_value("k-123"), "Bearer k-123");
        assert_eq!(authorization_value("  k-123  "), "Bearer k-123");
        assert_eq!(authorization_value("Bearer k-123"), "Bearer k-123");
        assert_eq!(authorization_value("Token k-123"), "Token k-123");
        assert_eq!(authorization_value(""), "");
        assert_eq!(authorization_value("   "), "");
    }

    /// `memory.api_key: $NAME` reads the environment, the same indirection
    /// `tracker.api_key` uses — so the secret need not sit in `teams.yaml`.
    #[tokio::test]
    async fn an_api_key_var_is_resolved_from_the_environment() {
        // SAFETY: single-threaded `#[tokio::test]` runtime; no other thread is
        // reading the environment while this runs.
        unsafe { std::env::set_var("RHAPSODY_TEST_HINDSIGHT_KEY", "from-env") };
        let stub = Stub::accepting().await;
        let b = HindsightBackend::new(&stub.url, "agent-", "$RHAPSODY_TEST_HINDSIGHT_KEY")
            .expect("backend");
        b.retain(&record("alice", "x")).await.expect("retain");
        assert_eq!(
            stub.request("POST", "/memories").authorization,
            "Bearer from-env"
        );
        unsafe { std::env::remove_var("RHAPSODY_TEST_HINDSIGHT_KEY") };
    }

    /// An unauthenticated deployment: no header is sent at all, rather than an
    /// empty one the service would reject differently.
    #[tokio::test]
    async fn an_empty_api_key_sends_no_header() {
        let stub = Stub::accepting().await;
        let b = HindsightBackend::new(&stub.url, "agent-", "").expect("backend");
        b.retain(&record("alice", "x")).await.expect("retain");
        assert_eq!(stub.request("POST", "/memories").authorization, "");
    }

    // ── browse (STUDIO-652's dashboard surface, which drives the same trait) ─────────────────────

    /// "Show me what this teammate remembers" has no query to score against, and
    /// hindsight's recall requires one — so a browse goes to the list endpoint,
    /// filtered to valid experience facts and bounded by `top_k`.
    #[tokio::test]
    async fn a_browse_lists_valid_experience_facts() {
        let stub = Stub::start(|_| {
            Reply::ok(
                r#"{"items":[{"id":"f1","text":"one","metadata":{"ticket":"STUDIO-1"}},
                             {"id":"f2","text":"two"},
                             {"nonsense":true}],"total":3,"limit":100,"offset":0}"#,
            )
        })
        .await;
        let got = backend(&stub)
            .recall(
                "alice",
                &Query {
                    top_k: 5,
                    browse: true,
                    ..Query::default()
                },
            )
            .await
            .expect("browse");
        let req = stub.request("GET", "/memories/list");
        assert!(req.query.contains("type=experience"), "{}", req.query);
        assert!(req.query.contains("state=valid"), "{}", req.query);
        assert!(req.query.contains("limit=5"), "{}", req.query);
        assert_eq!(got.facts.len(), 2, "the item that names no text is skipped");
        assert_eq!(got.facts[0].ticket, "STUDIO-1");
        assert_eq!(
            got.facts[1].identity, "alice",
            "the bank we asked is authoritative"
        );
        assert_eq!(got.skipped.len(), 1, "and it is skipped LOUDLY");
    }

    /// A *search* with no terms is not a browse: it recalls nothing rather than
    /// sending hindsight an empty query it would refuse.
    #[tokio::test]
    async fn an_empty_search_sends_nothing() {
        let stub = Stub::accepting().await;
        let got = backend(&stub)
            .recall(
                "alice",
                &Query {
                    top_k: 8,
                    ..Query::default()
                },
            )
            .await
            .expect("empty");
        assert!(got.facts.is_empty());
        assert!(stub.requests().is_empty());
    }

    /// The roster type this backend is built from in `run.rs`, exercised end to
    /// end so the wiring shape stays honest.
    #[test]
    fn a_roster_builds_the_overrides() {
        let roster = [
            Identity {
                name: "alice".to_string(),
                bank: "shared".to_string(),
                ..Identity::default()
            },
            Identity {
                name: "bob".to_string(),
                ..Identity::default()
            },
        ];
        let b = HindsightBackend::new("https://x.example", "agent-", "")
            .expect("backend")
            .with_bank_overrides(roster.iter().map(|i| (i.name.clone(), i.bank.clone())));
        assert_eq!(b.bank_id("alice"), "shared");
        assert_eq!(b.bank_id("bob"), "agent-bob");
        assert_eq!(b.base(), "https://x.example");
    }
}
