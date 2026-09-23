# CLAUDE.md — crates/provider-broker

Rhapsody-only crate (NOT a Go parity port): the protocol-neutral provider-broker core (slice PB1),
the private OpenAI-compatible loopback adapter (slice PB2), and the ledger/reservation/budget slice
(PB3) of `~/.rhapsody/docs/provider-broker-design.md` (approved 2026-09-19). Read `src/lib.rs`'s
top-of-file doc comment first — it is the ownership map. There is no Go package to mirror here; the
design record is authoritative.

## Layout

PB1 (protocol-neutral custody/capabilities/receipts):

- `lib.rs` — crate doc + public re-exports.
- `error.rs` — `BrokerError`, `CredentialRejection`, `LimitViolation` (typed refusals, no strings).
- `clock.rs` — `Clock` / `SystemClock` / `ManualClock` and the opaque `MonotonicTime`.
- `random.rs` — `RandomSource` / `OsRandom` / `ScriptedRandom` (deterministic tests).
- `secret.rs` — `ZeroizingBytes` and `CapabilityToken` (non-`Clone`, non-`Serialize`, redacting).
- `policy.rs` — `BrokerLimits` + the `DEFAULT_BROKER_LIMITS` / `HARD_BROKER_LIMITS` constants and
  whole-block validation; `BrokerProtocol`; `SessionPolicy`.
- `binding.rs` — `CredentialBinding`, its non-secret `BindingFingerprint`, and the move-only
  `BoundCredentialLease` (API-key shape validation; `expose_for_upstream` is the one crate-internal
  key borrow).
- `reservations.rs` — the atomic admission transaction and the releasable `ConcurrencyPermit`.
- `ledger.rs` — `TurnOutcome` / `TurnLedger` (the finalized, non-secret receipt).
- `state.rs` — private shared internals: `SessionInner`, `TurnInner`, `ReceiptSlot`, `CapacityOne`,
  `Registry`, `SessionId`, `TokenDigest`.
- `session.rs` — `BrokerSession` (custody) and `BrokerLedgerReceiver::arm_turn`.
- `turn.rs` — `BrokerTurnAttempt` / `TurnAccess` / `TurnReceipt` / `CapabilityGrant`.
- `broker.rs` — `Broker`, `BrokerRegistrationPlan`, `BrokerRegistration`, `lookup_capability`.

PB2 (the loopback adapter):

- `schema.rs` — the bounded JSON parser and the closed Chat Completions schema
  (`validate_chat_request`); refuses unknown top-level fields, generation-control aliases and
  remote-fetch content forms, and inserts/clamps `max_tokens`.
- `upstream.rs` — `NormalizedEndpoint` (one exact base-URL join to `chat/completions`) and the fixed
  `UpstreamClient` (redirects off, ambient proxies off, HTTP/1 only, platform roots, bounded
  timeouts, no decompression). `forward_chat_completions` is `pub(crate)` on purpose — there is no
  public generic forwarding primitive.
- `redact.rs` — `StreamingRedactor`: exact-byte secret replacement that matches across chunk
  boundaries with minimal look-behind.
- `sse.rs` — `SseUsageObserver` (bounded line buffer; malformed/oversized usage becomes unknown).
- `budget.rs` — the broker-wide weighted request-memory and buffered-response budgets.
- `refusal.rs` — `PolicyRefusal` and the pinned non-retryable status/code/message table.
- `listener.rs` — `BrokerListener`: the IPv4 loopback `TcpListener`, HTTP/1-only hyper driver,
  exact `Host`/no-`Origin`/POST-only guards, bearer auth from the header block, body/memory bounds,
  the one outbound request, and the backpressured redacted response stream.

PB3 (ledger, reservations, budget enforcement — design §7.3, §8):

- `authority.rs` — the optional durable `CumulativeBudgetAuthority` contract, `UtcDay`, and
  `DayBudgetRefusal`. The broker crate owns only the trait and the per-admission call; the durable
  store implementation belongs to a later slice and is never a dependency here.
- `metrics.rs` — `BrokerMetrics` / `BrokerMetricsSnapshot`: fixed, label-free counters (no
  session/run/capability/key identifier can become a dimension, design §13).
- `policy.rs` — `BrokerLimits::max_reserved_token_units_per_utc_day` (no implicit default) and
  `SessionPolicy::with_day_authority`, which refuses a day cap without an authority (and vice versa).
- `reservations.rs` — the one admission transaction now also charges the day authority atomically
  (backing out the session reservation if it refuses) and carries the usage accumulator; per-request
  settlement feeds `settle_usage` exactly once, and finalization counts any unsettled request unknown.
- `sse.rs` parses; the ledger settles. `TurnLedger` exposes separate
  `provider_reported_tokens` / `reserved_tokens` / `usage_authority` / `usage_incomplete` totals.

## Invariants a change must not break

- **Digest-only registry.** Raw tokens never appear as map keys, in `Debug`, errors, or logs. A
  `CapabilityToken` is reachable only through `expose_for_child`; the registry key is
  `SHA-256("rhapsody-provider-turn-v1\0" || presented)`. The `state.rs` unit tests are the canary.
- **Capacity-one, twice over.** `ReceiptSlot` refuses a new arm until the prior receipt is drained;
  `CapacityOne` refuses a new turn until the prior attempt/access drops. A receipt acts only on the
  ledger for its own turn ordinal, so a stale receipt cannot steal or erase a later turn's ledger.
  Dropping a receipt without draining it revokes the armed turn (a caller bug) so no capability can
  be minted or keep spending unwatched; the turn gate is still released only by the attempt/access
  drop — keep that split.
- **Synchronous RAII.** Revocation and receipt finalization happen in `Drop`, never in an async task.
  Finalization is exactly-once via the `TurnInner::finalized` mutex. Per-request usage settles
  exactly once through `RequestSettlement` (explicit observe or a conservative unknown on drop), so
  a cancelled/aborted request can never lose its charge.
- **Generic usage never releases a reservation (PB3).** A provider report is
  `provider_reported_unverified` measurement; only a request that was never admitted can have its
  reservation backed out (the day-authority refusal path). Reported tokens may exceed the
  reservation but never create another request allowance.
- **No panics on production paths.** `lock()` recovers poisoned mutexes; every error is a returned
  value; checked arithmetic everywhere.
- **Dependency direction.** This crate must not depend on `rhapsody-agent`, `rhapsody-orchestrator`,
  `rhapsody-httpapi`, `rhapsody-config`, or the desktop crate (`tests/dependency_guard.rs` enforces
  this against the manifest). PB2 owns the one HTTP stack, so `axum`/`hyper`/`reqwest`/`tokio` are
  expected dependencies and are deliberately not in that guard's forbidden list.
- **The loopback is not authentication (PB2).** Every request must present exactly one
  `Authorization: Bearer <capability>`; missing/repeated/malformed/unknown all collapse to the same
  401 that closes the connection. The capability authenticates *into the broker* and must never be
  attached to the outbound request; only the leased upstream key is. The reusable key must never
  come back to the child — every response body passes through the exact-secret streaming redactor.
- **One fixed destination (PB2).** `NormalizedEndpoint` joins the operator base exactly once to
  `chat/completions`; redirects and ambient proxies are disabled, plaintext needs the typed
  `allow_insecure_http` opt-in, and the forwarding entry point stays `pub(crate)` — no public generic
  `forward(url, headers, body)` primitive may be added.

## Test patterns

- Unit tests live in-module and may inspect private state (digest canary, zeroize, receipt slot).
- `tests/lifecycle.rs` drives the public API with `ManualClock` + `ScriptedRandom`; script RNG bytes
  in the exact order the code consumes them (`register` -> `mint`), and keep a minted `TurnAccess`
  alive when a later collision depends on its reserved digest.
- `tests/compile_guards.rs` uses `static_assertions::assert_not_impl_any!` for the non-`Clone` /
  non-`Serialize` / non-`Display` guarantees.
- The mutation discipline is real: each named bad implementation must redden a test. When changing a
  guarantee, mutate it locally, watch the test fail, revert. PB3's guards live in `tests/budget.rs`
  (a fake `CumulativeBudgetAuthority` with a controllable day and an atomic cap; concurrent-run and
  UTC-boundary oversubscription) and the PB3 block of `tests/loopback.rs` (SSE/JSON settlement,
  malformed/under-reported usage, retries), plus the unit tests in `reservations.rs`/`ledger.rs`.
