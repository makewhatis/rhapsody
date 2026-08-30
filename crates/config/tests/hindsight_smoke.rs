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
//! # It is deliberately NOT in CI
//!
//! `#[ignore]`, so `cargo test --workspace` skips it. It needs a tailnet, a live
//! service and a credential; a CI job that depended on all three would fail for
//! reasons that have nothing to do with the code under test, and the first fix
//! anyone reached for would be to delete it. Run it by hand:
//!
//! ```text
//! HINDSIGHT_API_KEY=… make hindsight-smoke
//! ```
//!
//! # Knobs
//!
//! | env | default | why |
//! | --- | --- | --- |
//! | `HINDSIGHT_ENDPOINT` | `https://hindsight.yak-saturation.ts.net` | the tailnet service STUDIO-629 exposed |
//! | `HINDSIGHT_API_KEY` | — | **required**; every `/v1/**` path 401s without it |
//! | `HINDSIGHT_SMOKE_IDENTITY` | `smoke` | with the default `agent-` prefix this is bank `agent-smoke` |
//!
//! The identity is a **scratch** one on purpose: this writes a real fact into a
//! real bank, and it must never be a teammate whose memory somebody relies on.

use rhapsody_config::hindsight::HindsightBackend;
use rhapsody_config::memory::{MemoryBackend, Query, Record};

const DEFAULT_ENDPOINT: &str = "https://hindsight.yak-saturation.ts.net";
const DEFAULT_IDENTITY: &str = "smoke";

fn env_or(name: &str, default: &str) -> String {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => default.to_string(),
    }
}

#[tokio::test]
#[ignore = "live: needs the tailnet, the hindsight service and HINDSIGHT_API_KEY — run `make hindsight-smoke`"]
async fn hindsight_live_smoke() {
    let endpoint = env_or("HINDSIGHT_ENDPOINT", DEFAULT_ENDPOINT);
    let identity = env_or("HINDSIGHT_SMOKE_IDENTITY", DEFAULT_IDENTITY);
    let api_key = std::env::var("HINDSIGHT_API_KEY").unwrap_or_default();
    assert!(
        !api_key.trim().is_empty(),
        "HINDSIGHT_API_KEY is unset. Every /v1/** path on the deployed service answers 401 \
         {{\"detail\":\"Authentication failed: Invalid API key\"}} without it, so this check \
         cannot run. See `memory.api_key` in teams.yaml."
    );

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
    let rec = Record {
        identity: identity.clone(),
        document_id: format!("run-{marker}"),
        ticket: "STUDIO-660".to_string(),
        commit_sha: "0000000".to_string(),
        pr: "0".to_string(),
        run_id: marker.clone(),
        at: chrono::Utc::now(),
        content: format!(
            "Smoke check {marker}: Rhapsody's own hindsight client retained this record while \
             porting STUDIO-660. It exists only to prove the round trip and is invalidated \
             moments later."
        ),
    };
    let doc = bank.retain(&rec).await.expect("retain");
    println!("\n[1/4] retain    -> ok, document_id={doc}");

    // ── 2. recall ───────────────────────────────────────────────────────────────
    let q = Query {
        ticket: "STUDIO-660".to_string(),
        title: format!("smoke check {marker}"),
        top_k: 8,
        ..Query::default()
    };
    let recalled = bank.recall(&identity, &q).await.expect("recall");
    println!("[2/4] recall    -> {} fact(s)", recalled.facts.len());
    for f in &recalled.facts {
        println!(
            "        id={} ticket={:?} run_id={:?} state={}\n          {}",
            f.id, f.ticket, f.run_id, f.state, f.content
        );
    }
    let target = recalled
        .facts
        .iter()
        .find(|f| f.content.contains(&marker))
        .or_else(|| recalled.facts.first())
        .unwrap_or_else(|| {
            panic!(
                "recall returned nothing for a fact retained moments ago — the round trip is \
                 broken, not merely slow"
            )
        })
        .clone();

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
        "\n== all four steps passed: retain -> recall -> invalidate(reason) -> recall (gone) =="
    );
    println!(
        "note: the record is invalidated, not deleted. `HindsightBackend::revalidate` restores it \
         ({}).",
        target.id
    );
}
