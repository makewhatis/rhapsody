//! The **live** hindsight smoke check (STUDIO-660, slice T8; design record
//! `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §5.3).
//!
//! §5.3 makes this an acceptance criterion of the memory slice rather than an
//! open design question, and is specific about why. STUDIO-569 measured the
//! correction path working live on 2026-08-24 — `PATCH …/memories/{id}` with
//! `{"state":"invalidated","reason":…}` removed the fact from recall, stored the
//! reason and was reversible — while the ticket had seen a 400. The two are not
//! in conflict: the 400 came from the **Go client**, which sends no `reason`.
//! But, in the design's own words, *"confirmed from a probe script" is not
//! "confirmed from Rhapsody's client"*, and nothing is allowed to depend on the
//! path until that check passes.
//!
//! So this drives [`HindsightBackend`] — the real client the daemon ships, not a
//! `curl` — through retain → recall → invalidate → recall against a scratch
//! bank, and prints what each step saw.
//!
//! # STUDIO-1036: the extractor is asynchronous, so recall must be polled
//!
//! Retain now sends `async: true` (Hindsight 0.10.1), so the call returns as soon
//! as the service accepts the document while LLM fact extraction runs in the
//! background — measured at roughly 30s. Recall immediately after retain therefore
//! returns nothing *by design*, and this check polls every 5s for up to 90s until
//! the marker fact appears before invalidating it. It also recalls both
//! `experience` and `world` facts now, because on 0.10.1 the extractor files most
//! of a teammate's own note as `world`.
//!
//! # It is deliberately NOT in CI
//!
//! `#[ignore]`, so `cargo test --workspace` skips it. It needs a live service;
//! a CI job that depended on one would fail for reasons that have nothing to do
//! with the code under test, and the first fix anyone reached for would be to
//! delete it. Run it by hand:
//!
//! ```text
//! make hindsight-smoke
//! ```
//!
//! # Knobs
//!
//! | env | default | why |
//! | --- | --- | --- |
//! | `HINDSIGHT_ENDPOINT` | `http://localhost:8888` | the operator's local Hindsight 0.10.1 |
//! | `HINDSIGHT_API_KEY` | — | **optional**; empty sends no `Authorization` header, which an unauthenticated local deployment wants |
//! | `HINDSIGHT_SMOKE_IDENTITY` | `smoke` | with the default `agent-` prefix this is bank `agent-smoke` |
//!
//! The identity is a **scratch** one on purpose: this writes a real fact into a
//! real bank, and it must never be a teammate whose memory somebody relies on.

use std::time::{Duration, Instant};

use rhapsody_config::hindsight::HindsightBackend;
use rhapsody_config::memory::{MemoryBackend, Query, Record};

const DEFAULT_ENDPOINT: &str = "http://localhost:8888";
const DEFAULT_IDENTITY: &str = "smoke";

/// How long to wait between recall polls while the extractor works.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How long to poll before giving up on the extracted fact appearing.
const POLL_TIMEOUT: Duration = Duration::from_secs(90);

fn env_or(name: &str, default: &str) -> String {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => default.to_string(),
    }
}

#[tokio::test]
#[ignore = "live: needs a running hindsight service — run `make hindsight-smoke`"]
async fn hindsight_live_smoke() {
    let endpoint = env_or("HINDSIGHT_ENDPOINT", DEFAULT_ENDPOINT);
    let identity = env_or("HINDSIGHT_SMOKE_IDENTITY", DEFAULT_IDENTITY);
    // Optional: an unauthenticated local deployment wants no Authorization
    // header, and the client already sends none for an empty key.
    let api_key = std::env::var("HINDSIGHT_API_KEY").unwrap_or_default();

    let bank = HindsightBackend::new(&endpoint, "agent-", &api_key).expect("build the backend");
    println!("== hindsight live smoke ==");
    println!("endpoint : {}", bank.base());
    println!("identity : {identity}");
    println!("bank     : {}", bank.bank_id(&identity));

    // A marker unique to this run, so recall can find THIS fact rather than one
    // a previous smoke left behind.
    let marker = format!(
        "rhapsody-smoke-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default()
    );

    // ── 1. retain ───────────────────────────────────────────────────────────────
    //
    // The extractor drops content-free text, so the note must be a concrete,
    // realistic record with the marker as a real token — not a description of
    // the check itself.
    let rec = Record {
        identity: identity.clone(),
        document_id: format!("run-{marker}"),
        ticket: "STUDIO-1036".to_string(),
        commit_sha: "0000000".to_string(),
        pr: "0".to_string(),
        run_id: marker.clone(),
        at: chrono::Utc::now(),
        content: format!(
            "On 2026-09-23 the Rhapsody hindsight smoke check retained a concrete record for \
             STUDIO-1036 with the unique marker token {marker}. The marker identifies this exact \
             retained note so the check can recall it once the background extractor has finished."
        ),
    };
    let doc = bank.retain(&rec).await.expect("retain");
    println!("\n[1/4] retain    -> ok (async, extraction pending), document_id={doc}");

    // ── 2. recall — poll until the extractor finishes ───────────────────────────
    //
    // `async: true` means recall immediately after retain sees nothing, so poll
    // every `POLL_INTERVAL` for up to `POLL_TIMEOUT` until a fact carries either
    // the marker or the document id.
    let q = Query {
        ticket: "STUDIO-1036".to_string(),
        title: format!("smoke check {marker}"),
        top_k: 8,
        ..Query::default()
    };
    let deadline = Instant::now() + POLL_TIMEOUT;
    let mut target = None;
    loop {
        let recalled = bank.recall(&identity, &q).await.expect("recall");
        if let Some(f) = recalled
            .facts
            .iter()
            .find(|f| f.content.contains(&marker) || f.document_id.contains(&marker))
        {
            target = Some(f.clone());
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        println!(
            "        recall    -> {} fact(s), marker not extracted yet; waiting {}s",
            recalled.facts.len(),
            POLL_INTERVAL.as_secs()
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    let target = target.unwrap_or_else(|| {
        panic!(
            "recall found no fact carrying {marker} or {doc} within {}s — the background retain \
             never became recallable",
            POLL_TIMEOUT.as_secs()
        )
    });
    println!("[2/4] recall    -> extracted, marker fact id={}", target.id);
    println!(
        "        ticket={:?} run_id={:?} state={}\n          {}",
        target.ticket, target.run_id, target.state, target.content
    );

    // ── 3. invalidate, WITH a reason ────────────────────────────────────────────
    //
    // This is §5.3's whole point: the reason is what the Go client omits and what
    // the 400 was about.
    let reason = format!("smoke check {marker}: retiring the record this run created");
    let changed = bank
        .invalidate(&identity, &target.id, &reason)
        .await
        .expect("invalidate");
    println!("[3/4] invalidate-> ok, changed={changed}, id={}", target.id);
    println!("        reason: {reason}");
    assert!(
        changed,
        "a freshly retained fact was already invalidated — that cannot be right"
    );

    // ── 4. recall again — the fact must be gone ─────────────────────────────────
    //
    // §5.3: `readableByModel` refuses ANY non-`valid` state, so an invalidated
    // fact is invisible to the model rather than merely deprioritised.
    let after = bank.recall(&identity, &q).await.expect("recall after");
    println!("[4/4] recall    -> {} fact(s)", after.facts.len());
    let still_there = after.facts.iter().any(|f| f.id == target.id);
    assert!(
        !still_there,
        "the invalidated fact {} is still recalled — §5.3's claim that an invalidated fact is \
         invisible to the model does not hold for this deployment",
        target.id
    );
    println!(
        "\n== all four steps passed: retain -> recall (extracted) -> invalidate(reason) -> recall \
         (gone) =="
    );
    println!(
        "note: the record is invalidated, not deleted. `HindsightBackend::revalidate` restores it \
         ({}).",
        target.id
    );
}
