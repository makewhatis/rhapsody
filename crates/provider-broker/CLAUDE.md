# CLAUDE.md — crates/provider-broker

Rhapsody-only crate (NOT a Go parity port): the protocol-neutral provider-broker core, slice PB1 of
`~/.rhapsody/docs/provider-broker-design.md` (approved 2026-09-19). Read `src/lib.rs`'s top-of-file
doc comment first — it is the ownership map. There is no Go package to mirror here; the design record
is authoritative.

## Layout

- `lib.rs` — crate doc + public re-exports.
- `error.rs` — `BrokerError`, `CredentialRejection`, `LimitViolation` (typed refusals, no strings).
- `clock.rs` — `Clock` / `SystemClock` / `ManualClock` and the opaque `MonotonicTime`.
- `random.rs` — `RandomSource` / `OsRandom` / `ScriptedRandom` (deterministic tests).
- `secret.rs` — `ZeroizingBytes` and `CapabilityToken` (non-`Clone`, non-`Serialize`, redacting).
- `policy.rs` — `BrokerLimits` + the `DEFAULT_BROKER_LIMITS` / `HARD_BROKER_LIMITS` constants and
  whole-block validation; `BrokerProtocol`; `SessionPolicy`.
- `binding.rs` — `CredentialBinding`, its non-secret `BindingFingerprint`, and the move-only
  `BoundCredentialLease` (API-key shape validation).
- `reservations.rs` — the atomic admission transaction and the releasable `ConcurrencyPermit`.
- `ledger.rs` — `TurnOutcome` / `TurnLedger` (the finalized, non-secret receipt).
- `state.rs` — private shared internals: `SessionInner`, `TurnInner`, `ReceiptSlot`, `CapacityOne`,
  `Registry`, `SessionId`, `TokenDigest`.
- `session.rs` — `BrokerSession` (custody) and `BrokerLedgerReceiver::arm_turn`.
- `turn.rs` — `BrokerTurnAttempt` / `TurnAccess` / `TurnReceipt` / `CapabilityGrant`.
- `broker.rs` — `Broker`, `BrokerRegistrationPlan`, `BrokerRegistration`, `lookup_capability`.

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
  Finalization is exactly-once via the `TurnInner::finalized` mutex.
- **No panics on production paths.** `lock()` recovers poisoned mutexes; every error is a returned
  value; checked arithmetic everywhere.
- **Dependency direction.** This crate must not depend on `rhapsody-agent`, `rhapsody-orchestrator`,
  `rhapsody-httpapi`, `rhapsody-config`, or the desktop crate, and no HTTP stack yet
  (`tests/dependency_guard.rs` enforces this against the manifest).

## Test patterns

- Unit tests live in-module and may inspect private state (digest canary, zeroize, receipt slot).
- `tests/lifecycle.rs` drives the public API with `ManualClock` + `ScriptedRandom`; script RNG bytes
  in the exact order the code consumes them (`register` -> `mint`), and keep a minted `TurnAccess`
  alive when a later collision depends on its reserved digest.
- `tests/compile_guards.rs` uses `static_assertions::assert_not_impl_any!` for the non-`Clone` /
  non-`Serialize` / non-`Display` guarantees.
- The mutation discipline is real: each named bad implementation must redden a test. When changing a
  guarantee, mutate it locally, watch the test fail, revert.
