//! teams — Rhapsody Teams **dispatch routing** (STUDIO-643, slice T3a; design
//! record `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`).
//!
//! [`route`] is the whole feature on this path, and its shape is the review
//! (§3.1): a **pure, synchronous function**, called ONCE inside
//! [`dispatch_issue`](crate::orchestrator::Orchestrator::dispatch_issue) AFTER
//! the issue has been selected and a slot taken. Everything that would turn a
//! router into a second engineering manager is absent by construction:
//!
//! * **It has no store and no clock.** Plain data in, a decision out.
//! * **It cannot enlarge or reorder the work.** It runs downstream of
//!   `select_dispatch` / `eligible` / `global_slots` — delete every line of it
//!   and the identical set of issues dispatches in the identical order, with
//!   only `identity` unset (§2.4 row 9; pinned by
//!   `routing_only_decorates_a_run_already_decided`).
//! * **It cannot say "not yet."** [`Routed`] is `{ identity, reason }`. There is
//!   **no `Defer`, no `Queue`, no `Retry` variant, and there never may be** —
//!   that missing variant is the entire defence against Teams becoming a second
//!   queue, and it is checkable by reading one enum (§3.1, §3.4).
//! * **It performs no I/O of any kind, and above all no network call.** The
//!   adversarial design review (`~/.rhapsody/docs/STUDIO-572-design-review.md`)
//!   forbade a model call or any network round-trip on the dispatch path, which
//!   runs inline on the single control task; §0.11.2 moved the model turn
//!   off-loop into T3b's triage task. `route`'s signature is the proof: it takes
//!   `&Teams`, `&Issue` and `&LoadSnapshot` and returns a `Routed`. There is
//!   nothing in it that could reach the network.
//!
//! Resolving a routed identity's **profile** text is a separate step
//! ([`Orchestrator::route_teams`]) deliberately kept OUTSIDE `route`, because it
//! reads a local file: keeping it out is what lets `route` stay a pure function
//! a reviewer can clear by reading its signature. That read is local-filesystem
//! only — never the network — and a failure degrades to dispatching without the
//! section rather than blocking the run.
//!
//! Off costs nothing (§2.4 rows 5 and 6). When Teams is disabled, `route` is
//! never called and `WorkerDeps::teammate_section` stays `String::new()`, which
//! the `if !x.is_empty()` guard in
//! [`build_turn_prompt`](crate::worker::build_turn_prompt) skips — the exact
//! mechanism BO-12 proved for `capabilities_section`, so a turn-1 prompt is
//! byte-identical to today.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use rhapsody_config::teams::{Identity, ManagerMode, Teams};
use rhapsody_core::Issue;
use rhapsody_store as store;

use crate::orchestrator::{Orchestrator, RunningEntry};

/// The Tier-0 label prefix: `rhapsody:@alice` names an identity outright (§3.2).
///
/// ⚠️ **`rhapsody:` is a SHARED namespace, and this is the discriminator.**
/// [`dispatch_issue`](crate::orchestrator::Orchestrator::dispatch_issue) already
/// strips `rhapsody:` from every ticket label and looks the remainder up in the
/// BO-11 capabilities registry, where an unknown name is a documented silent
/// no-op. `@` cannot be a capability name, which makes the split clean — but
/// that no-op is now **load-bearing for two consumers**, so anyone tempted to
/// turn an unknown `rhapsody:*` label into a hard error or a warning would
/// silently break Teams routing. Capabilities landed first, so this slice owns
/// the test pinning the split (`rhapsody_namespace_splits_between_routing_and_capabilities`).
pub(crate) const IDENTITY_LABEL_PREFIX: &str = "rhapsody:@";

/// The `events` row kind for a routed run (§3.4). A **data** value in the
/// existing `kind` column — no schema change, no new column, no golden move.
pub(crate) const EVENT_ROUTE: &str = "teams.route";
/// The `events` row kind when nobody fit and there was no `default_identity`
/// (§3.4). The run dispatches byte-identically to Teams-off; this row is the
/// only trace, so misroutes and non-routes stay countable after the fact.
pub(crate) const EVENT_UNROUTED: &str = "teams.unrouted";

/// Why [`route`] decided what it decided — recorded on the `teams.route` /
/// `teams.unrouted` event so a decision is auditable after the fact (§3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteReason {
    /// Tier 0 (§3.2): a `rhapsody:@<name>` ticket label named a roster member.
    /// The assignment IS the label (§0.11.1), so this is the same artifact
    /// T3b's triage writes and a human can write by hand.
    Label,
    /// The deterministic fallback (§3.2 Tier 1): roster-labels ∩ ticket-labels,
    /// highest wins, ties broken by fewest live runs then roster order.
    LabelOverlap,
    /// `manager.default_identity` took a ticket nothing else matched — the
    /// never-refuse fallback (§3.4), and every ticket under `mode: off` (§3.5).
    Default,
    /// Teams is off, or `manager.mode: off` with no `default_identity` (§3.5) —
    /// "nothing routes and nothing is prepended, behaviour identical to
    /// `enabled: false`". Nothing is recorded either: an extra `events` row
    /// would be exactly the behavioural delta §3.5 promises is absent.
    Off,
    /// Nobody fit and there is no `default_identity`: the run dispatches
    /// **byte-identically to Teams-off** and a `teams.unrouted` event is
    /// recorded. Never a refusal — a Teams feature that can withhold work is a
    /// second queue (§3.4).
    Unrouted,
}

impl RouteReason {
    /// The stable token written into the event text. Kept short and snake_case
    /// so `symphony_events --kind teams.route` output greps cleanly.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RouteReason::Label => "label",
            RouteReason::LabelOverlap => "label_overlap",
            RouteReason::Default => "default_identity",
            RouteReason::Off => "off",
            RouteReason::Unrouted => "no_match",
        }
    }
}

/// [`route`]'s answer. **The variants this type does NOT have are the point**
/// (§3.1): no `Defer`, no `Queue`, no `Retry`. The router either names an
/// identity or names nobody, and either way the run dispatches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Routed {
    /// The routed identity, or `None` for a run that dispatches exactly as it
    /// does with Teams off.
    pub identity: Option<String>,
    pub reason: RouteReason,
}

impl Routed {
    fn to(identity: String, reason: RouteReason) -> Self {
        Routed {
            identity: Some(identity),
            reason,
        }
    }

    fn none(reason: RouteReason) -> Self {
        Routed {
            identity: None,
            reason,
        }
    }
}

/// Live per-identity run counts, **derived at call time** from
/// [`Orchestrator::running`] (§3.1). It is a read of Rhapsody's own state, never
/// a copy the router owns and never a thing the router can mutate — the router
/// "holds no idea of what is in flight" beyond this borrow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LoadSnapshot(HashMap<String, i64>);

impl LoadSnapshot {
    /// Counts the live runs already stamped with each identity. The run being
    /// dispatched is NOT in `running` yet, so it never counts itself.
    pub(crate) fn from_running(running: &HashMap<String, RunningEntry>) -> Self {
        let mut counts: HashMap<String, i64> = HashMap::new();
        for re in running.values() {
            if !re.identity.is_empty() {
                *counts.entry(re.identity.clone()).or_default() += 1;
            }
        }
        LoadSnapshot(counts)
    }

    /// Live runs for `name`; absent ⇒ 0.
    fn live(&self, name: &str) -> i64 {
        self.0.get(name).copied().unwrap_or(0)
    }
}

/// Routes one already-selected, already-slotted issue to a teammate (§3.1).
///
/// Decision order, cheapest first — and every step is a comparison over data
/// already in hand:
///
/// 1. **`manager.mode: off`** (§3.5): no routing is ever performed.
///    `default_identity` takes every ticket (single-identity Teams); without one,
///    nothing routes.
/// 2. **Tier 0** (§3.2, §0.11.1): a `rhapsody:@<name>` label naming a roster
///    member **wins outright**. §0.11.1 is explicit that a present label,
///    *whoever wrote it*, is authoritative — so this tier deliberately ignores
///    `max_concurrent`; see [`at_capacity`].
/// 3. **The deterministic fallback** (§3.2 Tier 1): score each identity by
///    `|ticket.labels ∩ identity.labels|`, highest wins, ties broken by fewest
///    live runs and then roster order — a **total** order, so the same inputs
///    always produce the same answer.
/// 4. **Never refuse** (§3.4): `manager.default_identity`, else no identity at
///    all and a `teams.unrouted` event.
///
/// `ManagerMode::LabelsModel` routes identically to `ManagerMode::Labels` here.
/// The model turn it enables is T3b's **off-loop triage** task, which writes a
/// `rhapsody:@` label this function later reads as Tier 0 (§0.11.2) — it is
/// never consulted on the dispatch path.
pub(crate) fn route(teams: &Teams, iss: &Issue, load: &LoadSnapshot) -> Routed {
    // Defence in depth: the sole production caller (`route_teams`) already gates
    // on `enabled`, so §2.4 row 6's "route() is not called" holds mechanically.
    // Answering `Off` here too means no test or future caller can accidentally
    // route against a disabled roster.
    if !teams.enabled || teams.roster.is_empty() {
        return Routed::none(RouteReason::Off);
    }
    if teams.manager.mode == ManagerMode::Off {
        return match default_identity(teams) {
            Some(name) => Routed::to(name, RouteReason::Default),
            None => Routed::none(RouteReason::Off),
        };
    }
    if let Some(name) = tier0(teams, iss) {
        return Routed::to(name, RouteReason::Label);
    }
    if let Some(name) = best_by_label_overlap(teams, iss, load) {
        return Routed::to(name, RouteReason::LabelOverlap);
    }
    match default_identity(teams) {
        Some(name) => Routed::to(name, RouteReason::Default),
        None => Routed::none(RouteReason::Unrouted),
    }
}

/// `manager.default_identity`, but only when it actually names a roster member.
/// T1's `Teams::validate` already rejects a file that fails this, so the re-check
/// costs a roster scan and closes the gap for a `Teams` built in memory — and
/// §0.11.5 requires that any chosen identity be validated against the roster
/// before it is used.
fn default_identity(teams: &Teams) -> Option<String> {
    let name = &teams.manager.default_identity;
    if name.is_empty() {
        return None;
    }
    teams
        .roster
        .iter()
        .find(|i| &i.name == name)
        .map(|i| i.name.clone())
}

/// Tier 0: the first ROSTER member (not the first label) carrying a matching
/// `rhapsody:@<name>` label.
///
/// Iterating the roster rather than the ticket's labels is what makes this
/// deterministic: Linear does not promise a stable label order, so a ticket
/// wearing two `rhapsody:@` labels would otherwise resolve differently between
/// ticks. Roster order is the operator's own, written down in `teams.yaml`.
///
/// A `rhapsody:@` label naming someone who is NOT on the roster matches nothing
/// and falls through to the deterministic fallback (§0.11.5: an unknown identity
/// is never trusted).
fn tier0(teams: &Teams, iss: &Issue) -> Option<String> {
    teams
        .roster
        .iter()
        .find(|i| has_identity_label(iss, &i.name))
        .map(|i| i.name.clone())
}

/// Whether `iss` carries the `rhapsody:@<name>` label for exactly `name`.
fn has_identity_label(iss: &Issue, name: &str) -> bool {
    iss.labels
        .iter()
        .flatten()
        .any(|l| l.strip_prefix(IDENTITY_LABEL_PREFIX) == Some(name))
}

/// The deterministic fallback (§3.2 Tier 1): highest label overlap, ties broken
/// by fewest live runs, then roster order. Identities with no overlap at all are
/// not candidates, and identities at their `max_concurrent` cap are skipped for
/// the next-best candidate rather than held (§3.4).
fn best_by_label_overlap(teams: &Teams, iss: &Issue, load: &LoadSnapshot) -> Option<String> {
    let ticket: HashSet<&str> = iss.labels.iter().flatten().map(String::as_str).collect();
    if ticket.is_empty() {
        return None;
    }
    teams
        .roster
        .iter()
        .enumerate()
        .filter(|(_, i)| !at_capacity(i, load))
        .filter_map(|(order, i)| {
            // Set intersection, so a roster entry that lists the same label
            // twice cannot inflate its own score.
            let own: HashSet<&str> = i.labels.iter().map(String::as_str).collect();
            let score = own.intersection(&ticket).count();
            (score > 0).then(|| (score, load.live(&i.name), order, i.name.clone()))
        })
        // Highest score first (hence `Reverse`), then fewest live runs, then
        // roster order — a TOTAL order, so this is deterministic given the same
        // inputs. `min_by_key` keeps the first minimum, but `order` is unique
        // per entry so the key never ties anyway.
        .min_by_key(|(score, live, order, _)| (Reverse(*score), *live, *order))
        .map(|(_, _, _, name)| name)
}

/// Whether `i` is at its per-identity cap. `max_concurrent: 0` ⇒ unlimited
/// (§2.2), which is the default and the overwhelmingly common case.
///
/// The cap is an **escape hatch for a user who wants a teammate serialised**
/// (§3.4), and it applies to the deterministic fallback only — never to Tier 0,
/// which §0.11.1 makes authoritative ("a present label, whoever wrote it, is
/// authoritative Tier 0", and the ticket's own decision order says Tier 0 "wins
/// outright"), and never to `default_identity`, which is the never-refuse floor
/// rather than a candidate. Capping either of those could only ever move an
/// explicitly-assigned ticket to somebody else; it could never make the work
/// wait, because there is no variant of [`Routed`] that can hold work.
fn at_capacity(i: &Identity, load: &LoadSnapshot) -> bool {
    i.max_concurrent > 0 && load.live(&i.name) >= i.max_concurrent
}

/// The turn-1 teammate section: the identity header plus the identity's resolved
/// profile text (§0.11.6's fixed order is capabilities → teammate header → room
/// catch-up → memory recall; the last two are T5's, and the composer that owns
/// the byte budget is T5's too — for now this simply joins the existing prepend
/// chain).
///
/// An empty `profile_prompt` still yields the header: the run genuinely IS being
/// worked as that identity, and saying so costs one paragraph. The caller decides
/// whether there is a section at all — a profile that fails to RESOLVE produces
/// no section whatsoever (see [`Orchestrator::route_teams`]).
pub(crate) fn teammate_section(identity: &str, profile_prompt: &str) -> String {
    let mut out = format!(
        "## You are working as {identity}\n\n\
         You are working as **{identity}**, a named teammate on this Rhapsody team. \
         Work this ticket as {identity}, and follow the profile below for the whole run.\n\n"
    );
    out.push_str(profile_prompt);
    out.trim_end().to_string()
}

/// What dispatch stamps on a run once Teams has had its say.
pub(crate) struct TeamsDispatch {
    /// The routed identity; **empty** when nobody was routed, in which case the
    /// run is byte-identical to a Teams-off dispatch (§3.4).
    pub identity: String,
    /// The turn-1 prepend. Empty ⇒ the `if !x.is_empty()` guard in
    /// `build_turn_prompt` skips it and the prompt is byte-identical (§2.4 row 5).
    pub section: String,
    /// [`EVENT_ROUTE`] or [`EVENT_UNROUTED`].
    pub kind: &'static str,
    /// The event text: the reason, and the identity when there is one.
    pub text: String,
}

impl Orchestrator {
    /// The whole Teams contribution to one dispatch: route, resolve the routed
    /// identity's profile, and describe the `events` row to record.
    ///
    /// `None` means Teams contributes **nothing at all** to this dispatch — no
    /// identity, no prompt section, and no event. That covers Teams being off
    /// (§2.4 row 6, where `route` is not even called) and §3.5's `mode: off`
    /// without a `default_identity`, which the design says is "behaviour
    /// identical to `enabled: false`" — an extra `events` row would be precisely
    /// the behavioural delta that promise rules out.
    pub(crate) fn route_teams(&self, iss: &Issue) -> Option<TeamsDispatch> {
        let teams = self.teams.as_ref().filter(|t| t.enabled)?;
        let routed = route(teams, iss, &LoadSnapshot::from_running(&self.running));
        let Some(identity) = routed.identity else {
            if routed.reason == RouteReason::Off {
                return None;
            }
            return Some(TeamsDispatch {
                identity: String::new(),
                section: String::new(),
                kind: EVENT_UNROUTED,
                text: format!("reason={}", routed.reason.as_str()),
            });
        };
        let section = self.teammate_section_for(teams, &identity);
        Some(TeamsDispatch {
            kind: EVENT_ROUTE,
            text: format!("identity={identity} reason={}", routed.reason.as_str()),
            identity,
            section,
        })
    }

    /// Renders the routed identity's turn-1 section, resolving its profile
    /// through T2's [`rhapsody_config::profiles::resolve`].
    ///
    /// Two degradations, both deliberate and neither able to block work:
    ///
    /// * **The identity names no profile, or there is no runtime home to resolve
    ///   profiles against** — render the header alone. The run really is being
    ///   worked as that identity; there is simply no profile prose to add.
    /// * **The profile fails to resolve** (unknown name, unreadable or malformed
    ///   file) — log loudly and render **nothing**, so the run dispatches without
    ///   the section. A broken profile must not block work; the boot-time
    ///   `report_profile_issues` warning is where an operator is meant to catch
    ///   this, and this is the backstop.
    fn teammate_section_for(&self, teams: &Teams, identity: &str) -> String {
        let profile = teams
            .roster
            .iter()
            .find(|i| i.name == identity)
            .map(|i| i.profile.clone())
            .unwrap_or_default();
        // This identity names no profile: the header alone.
        if profile.is_empty() {
            return teammate_section(identity, "");
        }
        // Nowhere to resolve one from: the header alone. `teams_profiles_dir` is
        // `None` only when the daemon has no on-disk runtime home, which is also
        // the only way `self.teams` could have been set without one — in
        // production the two are resolved together at boot.
        let Some(dir) = self.teams_profiles_dir.as_ref() else {
            return teammate_section(identity, "");
        };
        match rhapsody_config::profiles::resolve(dir, &profile) {
            Ok(p) => teammate_section(identity, &p.prompt),
            Err(e) => {
                tracing::error!(
                    identity = %identity,
                    profile = %profile,
                    dir = %dir.display(),
                    error = %e,
                    "teams profile failed to resolve; dispatching this run WITHOUT the teammate \
                     section (a broken profile must not block work)"
                );
                String::new()
            }
        }
    }

    /// Records the routing decision as an `events` row on the run (§3.4).
    ///
    /// The kind is a **data** value in the existing `kind` column — no schema
    /// change, no new table, no golden moves. Past tense by construction (§3.1):
    /// it is a row on a run, so it cannot exist unless the run does, and
    /// `enqueue_event` no-ops on the zero `run_id` a disabled store leaves
    /// behind. The DURABLE work history lives in the room log (§0.11.7); these
    /// rows are pruned with their runs and are the per-run timeline copy.
    pub(crate) fn record_route_event(&self, re: &mut RunningEntry, td: &TeamsDispatch) {
        re.event_seq += 1;
        self.enqueue_event(
            re.run_id,
            store::EventRow {
                seq: re.event_seq,
                at: crate::persist::rfc3339(re.started_at),
                kind: td.kind.to_string(),
                tool: String::new(),
                text: td.text.clone(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::testsupport::{issue, orch_for_retry};
    use rhapsody_config::teams::{Manager, Teams};
    use rhapsody_store::{Sqlite, Store, StorePath};
    use rhapsody_tracker::fake::Fake;

    /// A roster entry: `name`, its matching labels, and its `max_concurrent`.
    fn ident(name: &str, labels: &[&str], max_concurrent: i64) -> Identity {
        Identity {
            name: name.to_string(),
            profile: String::new(),
            labels: labels.iter().map(|s| (*s).to_string()).collect(),
            bank: String::new(),
            max_concurrent,
        }
    }

    /// An ENABLED Teams with `mode: labels` (the §2.2 default) and this roster.
    fn teams_with(roster: Vec<Identity>) -> Teams {
        Teams {
            enabled: true,
            roster,
            ..Teams::disabled()
        }
    }

    fn with_labels(labels: &[&str]) -> Issue {
        Issue {
            labels: Some(labels.iter().map(|s| (*s).to_string()).collect()),
            ..issue("1", "MT-1", "Todo")
        }
    }

    fn load_of(counts: &[(&str, i64)]) -> LoadSnapshot {
        LoadSnapshot(counts.iter().map(|(n, c)| ((*n).to_string(), *c)).collect())
    }

    /// §3.2 Tier 0: a `rhapsody:@<name>` label naming a roster member wins
    /// outright — ahead of a rival with a strictly better label overlap.
    #[test]
    fn tier0_identity_label_wins_outright() {
        let teams = teams_with(vec![
            ident("alice", &[], 0),
            ident("bob", &["rust", "config"], 0),
        ]);
        let iss = with_labels(&["rhapsody:@alice", "rust", "config"]);
        let r = route(&teams, &iss, &LoadSnapshot::default());
        assert_eq!(r.identity.as_deref(), Some("alice"));
        assert_eq!(r.reason, RouteReason::Label);
    }

    /// §0.11.5: a model- or human-written identity is validated against the
    /// roster. `rhapsody:@nobody` names no one, so it matches nothing and the
    /// deterministic fallback answers instead — never an error, never a refusal.
    #[test]
    fn tier0_ignores_a_label_naming_nobody_on_the_roster() {
        let teams = teams_with(vec![ident("bob", &["rust"], 0)]);
        let iss = with_labels(&["rhapsody:@nobody", "rust"]);
        let r = route(&teams, &iss, &LoadSnapshot::default());
        assert_eq!(r.identity.as_deref(), Some("bob"));
        assert_eq!(r.reason, RouteReason::LabelOverlap);
    }

    /// Two `rhapsody:@` labels resolve by ROSTER order, not label order — Linear
    /// promises no stable label order, so routing must not depend on one.
    #[test]
    fn tier0_with_two_identity_labels_is_roster_ordered() {
        let teams = teams_with(vec![ident("alice", &[], 0), ident("bob", &[], 0)]);
        for labels in [
            vec!["rhapsody:@alice", "rhapsody:@bob"],
            vec!["rhapsody:@bob", "rhapsody:@alice"],
        ] {
            let r = route(&teams, &with_labels(&labels), &LoadSnapshot::default());
            assert_eq!(
                r.identity.as_deref(),
                Some("alice"),
                "roster order must decide, not label order ({labels:?})"
            );
        }
    }

    /// §3.2 Tier 1: highest `|ticket.labels ∩ identity.labels|` wins.
    #[test]
    fn fallback_picks_the_highest_label_overlap() {
        let teams = teams_with(vec![
            ident("alice", &["rust"], 0),
            ident("bob", &["rust", "config"], 0),
            ident("carol", &["web"], 0),
        ]);
        let r = route(
            &teams,
            &with_labels(&["rust", "config"]),
            &LoadSnapshot::default(),
        );
        assert_eq!(r.identity.as_deref(), Some("bob"));
        assert_eq!(r.reason, RouteReason::LabelOverlap);
    }

    /// A roster entry that lists the same label twice cannot inflate its score:
    /// the overlap is a SET intersection (§3.2's `|ticket ∩ identity|`).
    #[test]
    fn fallback_score_is_a_set_intersection_not_a_multiset_count() {
        let teams = teams_with(vec![
            ident("alice", &["rust", "rust", "rust"], 0),
            ident("bob", &["rust", "config"], 0),
        ]);
        let r = route(
            &teams,
            &with_labels(&["rust", "config"]),
            &LoadSnapshot::default(),
        );
        assert_eq!(
            r.identity.as_deref(),
            Some("bob"),
            "alice's duplicated `rust` must score 1, not 3"
        );
    }

    /// §3.2's tie-break is TOTAL: equal scores go to the fewest live runs, and
    /// only then to roster order — so the function is deterministic given the
    /// same inputs.
    #[test]
    fn fallback_ties_break_by_live_runs_then_roster_order() {
        let teams = teams_with(vec![
            ident("alice", &["rust"], 0),
            ident("bob", &["rust"], 0),
            ident("carol", &["rust"], 0),
        ]);
        let iss = with_labels(&["rust"]);

        // All idle ⇒ roster order decides.
        let r = route(&teams, &iss, &LoadSnapshot::default());
        assert_eq!(r.identity.as_deref(), Some("alice"));

        // Alice busy ⇒ the next-fewest-runs candidate wins, ahead of roster order.
        let r = route(&teams, &iss, &load_of(&[("alice", 2), ("bob", 1)]));
        assert_eq!(r.identity.as_deref(), Some("carol"));

        // Equal live runs ⇒ back to roster order.
        let r = route(
            &teams,
            &iss,
            &load_of(&[("alice", 1), ("bob", 1), ("carol", 1)]),
        );
        assert_eq!(r.identity.as_deref(), Some("alice"));
    }

    /// §3.4: an identity at its `max_concurrent` cap is skipped for the
    /// NEXT-BEST candidate. It is never held, and there is no variant of
    /// [`Routed`] that could hold it — the work dispatches either way.
    #[test]
    fn max_concurrent_skips_to_the_next_best_and_never_queues() {
        let teams = teams_with(vec![
            ident("alice", &["rust", "config"], 1),
            ident("bob", &["rust"], 0),
        ]);
        let iss = with_labels(&["rust", "config"]);

        // Idle: alice's better overlap wins.
        let r = route(&teams, &iss, &LoadSnapshot::default());
        assert_eq!(r.identity.as_deref(), Some("alice"));

        // At her cap: bob takes it despite the WORSE overlap. The decision is
        // still an identity — never a defer.
        let r = route(&teams, &iss, &load_of(&[("alice", 1)]));
        assert_eq!(r.identity.as_deref(), Some("bob"));
        assert_eq!(r.reason, RouteReason::LabelOverlap);
    }

    /// `max_concurrent: 0` is unlimited (§2.2) — the default, and the case that
    /// must never accidentally read as "capped at zero".
    #[test]
    fn max_concurrent_zero_is_unlimited() {
        let teams = teams_with(vec![ident("alice", &["rust"], 0)]);
        let r = route(&teams, &with_labels(&["rust"]), &load_of(&[("alice", 99)]));
        assert_eq!(r.identity.as_deref(), Some("alice"));
    }

    /// §0.11.1: "a present label, whoever wrote it, is authoritative Tier 0",
    /// and the decision order says Tier 0 wins *outright* — so the cap, an
    /// escape hatch for the router's own choices, does not override an explicit
    /// assignment. (Capping it could only ever hand an explicitly-assigned
    /// ticket to someone else; it could never make the work wait.)
    #[test]
    fn max_concurrent_does_not_override_an_explicit_tier0_label() {
        let teams = teams_with(vec![ident("alice", &[], 1), ident("bob", &["rust"], 0)]);
        let iss = with_labels(&["rhapsody:@alice", "rust"]);
        let r = route(&teams, &iss, &load_of(&[("alice", 5)]));
        assert_eq!(r.identity.as_deref(), Some("alice"));
        assert_eq!(r.reason, RouteReason::Label);
    }

    /// §3.4 "fall back, never refuse": nobody overlaps ⇒ `default_identity`.
    #[test]
    fn nobody_fits_falls_back_to_the_default_identity() {
        let mut teams = teams_with(vec![
            ident("alice", &["rust"], 0),
            ident("bob", &["web"], 0),
        ]);
        teams.manager.default_identity = "bob".to_string();
        let r = route(&teams, &with_labels(&["docs"]), &LoadSnapshot::default());
        assert_eq!(r.identity.as_deref(), Some("bob"));
        assert_eq!(r.reason, RouteReason::Default);
    }

    /// A `default_identity` that is not on the roster is not trusted (§0.11.5);
    /// the ticket is unrouted rather than routed to a name nobody can resolve.
    #[test]
    fn a_default_identity_off_the_roster_is_not_trusted() {
        let mut teams = teams_with(vec![ident("alice", &["rust"], 0)]);
        teams.manager.default_identity = "ghost".to_string();
        let r = route(&teams, &with_labels(&["docs"]), &LoadSnapshot::default());
        assert_eq!(r.identity, None);
        assert_eq!(r.reason, RouteReason::Unrouted);
    }

    /// §3.4: nobody fits and there is no default ⇒ NO identity and a
    /// `teams.unrouted` reason. The run still dispatches — refusal is not an
    /// option under consideration.
    #[test]
    fn nobody_fits_and_no_default_is_unrouted_never_refused() {
        let teams = teams_with(vec![ident("alice", &["rust"], 0)]);
        for iss in [with_labels(&["docs"]), issue("1", "MT-1", "Todo")] {
            let r = route(&teams, &iss, &LoadSnapshot::default());
            assert_eq!(r.identity, None);
            assert_eq!(r.reason, RouteReason::Unrouted);
        }
    }

    /// Even every candidate being at its cap cannot produce a "not yet": the
    /// router falls through to the default, exactly as "nobody fits" does.
    #[test]
    fn every_candidate_capped_falls_through_rather_than_holding_work() {
        let mut teams = teams_with(vec![
            ident("alice", &["rust"], 1),
            ident("bob", &["rust"], 1),
        ]);
        teams.manager.default_identity = "bob".to_string();
        let r = route(
            &teams,
            &with_labels(&["rust"]),
            &load_of(&[("alice", 1), ("bob", 1)]),
        );
        assert_eq!(r.identity.as_deref(), Some("bob"));
        assert_eq!(r.reason, RouteReason::Default);
    }

    /// §3.5 single-identity Teams: `mode: off` with a `default_identity` gives
    /// EVERY ticket to that one teammate — no routing decision is ever made, so
    /// even an explicit `rhapsody:@` label for someone else is not consulted.
    #[test]
    fn mode_off_with_a_default_gives_every_ticket_to_one_identity() {
        let mut teams = teams_with(vec![
            ident("alice", &["rust"], 0),
            ident("bob", &["web"], 0),
        ]);
        teams.manager.mode = ManagerMode::Off;
        teams.manager.default_identity = "bob".to_string();
        for iss in [
            with_labels(&["rust"]),
            with_labels(&["rhapsody:@alice"]),
            issue("1", "MT-1", "Todo"),
        ] {
            let r = route(&teams, &iss, &LoadSnapshot::default());
            assert_eq!(r.identity.as_deref(), Some("bob"));
            assert_eq!(r.reason, RouteReason::Default);
        }
    }

    /// §3.5: `mode: off` with NO default routes nothing — "behaviour identical
    /// to `enabled: false`", which is why the reason is [`RouteReason::Off`]
    /// (the caller records no event and prepends nothing for it).
    #[test]
    fn mode_off_without_a_default_routes_nothing() {
        let mut teams = teams_with(vec![ident("alice", &["rust"], 0)]);
        teams.manager.mode = ManagerMode::Off;
        let r = route(&teams, &with_labels(&["rust"]), &LoadSnapshot::default());
        assert_eq!(r.identity, None);
        assert_eq!(r.reason, RouteReason::Off);
    }

    /// `labels+model` routes IDENTICALLY to `labels` on the dispatch path: the
    /// model turn it enables is T3b's off-loop triage, which writes a
    /// `rhapsody:@` label this function later reads as Tier 0 (§0.11.2). Nothing
    /// here may consult a model.
    #[test]
    fn labels_model_mode_routes_identically_to_labels_on_the_dispatch_path() {
        let roster = vec![ident("alice", &["rust"], 0), ident("bob", &["web"], 0)];
        let labels_mode = teams_with(roster.clone());
        let model_mode = Teams {
            manager: Manager {
                mode: ManagerMode::LabelsModel,
                ..Manager::default()
            },
            ..teams_with(roster)
        };
        for iss in [
            with_labels(&["rust"]),
            with_labels(&["rhapsody:@bob"]),
            with_labels(&["docs"]),
        ] {
            assert_eq!(
                route(&labels_mode, &iss, &LoadSnapshot::default()),
                route(&model_mode, &iss, &LoadSnapshot::default()),
                "labels+model must not change the dispatch-path decision"
            );
        }
    }

    /// Defence in depth (§2.4 row 6): a disabled Teams — or an enabled one with
    /// an empty roster — routes nothing, even though the production caller never
    /// calls `route` in that state at all.
    #[test]
    fn disabled_or_empty_roster_routes_nothing() {
        let disabled = Teams {
            roster: vec![ident("alice", &["rust"], 0)],
            ..Teams::disabled()
        };
        let empty = teams_with(vec![]);
        for t in [disabled, empty] {
            let r = route(&t, &with_labels(&["rust"]), &LoadSnapshot::default());
            assert_eq!(r.identity, None);
            assert_eq!(r.reason, RouteReason::Off);
        }
    }

    /// The load snapshot is derived from `Orchestrator.running` and counts only
    /// entries that were actually stamped with an identity.
    #[test]
    fn load_snapshot_counts_live_runs_per_identity() {
        let mut running = HashMap::new();
        for (id, identity) in [("1", "alice"), ("2", "alice"), ("3", "bob"), ("4", "")] {
            let mut re = RunningEntry::empty(issue(id, "MT-1", "Todo"));
            re.identity = identity.to_string();
            running.insert(id.to_string(), re);
        }
        let load = LoadSnapshot::from_running(&running);
        assert_eq!(load.live("alice"), 2);
        assert_eq!(load.live("bob"), 1);
        assert_eq!(
            load.live("nobody"),
            0,
            "an unknown identity is 0, never a panic"
        );
    }

    /// The section names the identity and carries the resolved profile text.
    #[test]
    fn teammate_section_renders_header_and_profile() {
        let s = teammate_section("alice", "You are a software engineer on this codebase.");
        assert!(
            s.starts_with("## You are working as alice\n\n"),
            "s = {s:?}"
        );
        assert!(s.contains("**alice**"), "s = {s:?}");
        assert!(
            s.ends_with("You are a software engineer on this codebase."),
            "the resolved profile text must be included: {s:?}"
        );
        // No trailing whitespace: the prompt builder adds its own separator.
        assert_eq!(s, s.trim_end(), "the section must be trimmed");

        // No profile text ⇒ the header alone, still naming the identity.
        let bare = teammate_section("bob", "");
        assert!(
            bare.starts_with("## You are working as bob\n\n"),
            "{bare:?}"
        );
        assert_eq!(bare, bare.trim_end());
    }

    // ---- dispatch-level acceptance (§2.4 rows 5, 6 and 9) -------------------

    /// An orchestrator with Teams ENABLED, this roster, and a real in-memory
    /// store so `teams.route` rows are readable back.
    fn orch_with_teams(teams: Teams) -> (Orchestrator, Arc<dyn Store + Send + Sync>) {
        let (mut o, _) = orch_for_retry(Arc::new(Fake::new()), 10);
        let store: Arc<dyn Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("open in-memory store"));
        o.set_store(Arc::clone(&store));
        o.start_event_writer();
        o.teams = Some(teams);
        (o, store)
    }

    /// Drains the batched event writer so the rows it queued are readable.
    fn flush_events(o: &mut Orchestrator) {
        o.stop_event_writer();
    }

    fn events_of(store: &dyn Store, run_id: i64) -> Vec<(String, String)> {
        store
            .run_events(run_id)
            .expect("run events")
            .into_iter()
            .map(|e| (e.kind, e.text))
            .collect()
    }

    /// **The T3a acceptance criterion (§0.11.8, §3.1): manager deleted ⇒
    /// byte-identical dispatch.**
    ///
    /// The same issue set is dispatched through two orchestrators — one with a
    /// full Teams roster, one with Teams absent entirely — and every dispatch
    /// observable is compared: the ORDER issues reached the spawn seam, and each
    /// running entry's issue, project routing, model, capability section and
    /// stack context. Routing may only ever DECORATE a run that was already
    /// decided, so the ONLY permitted difference is `identity` /
    /// `teammate_section`, which the final assertions pin explicitly.
    #[test]
    fn routing_only_decorates_a_run_already_decided() {
        let issues = vec![
            with_labels(&["rust"]),
            Issue {
                labels: Some(vec!["rhapsody:@bob".to_string()]),
                ..issue("2", "MT-2", "Todo")
            },
            Issue {
                labels: Some(vec!["docs".to_string()]),
                ..issue("3", "MT-3", "Todo")
            },
            issue("4", "MT-4", "Todo"),
        ];

        let teams = teams_with(vec![
            ident("alice", &["rust"], 0),
            ident("bob", &["web"], 0),
        ]);
        let (mut with, dispatched_with) = orch_for_retry(Arc::new(Fake::new()), 10);
        with.teams = Some(teams);
        let (mut without, dispatched_without) = orch_for_retry(Arc::new(Fake::new()), 10);
        without.teams = None;

        for iss in &issues {
            with.dispatch_issue(iss.clone(), None, None, String::new());
            without.dispatch_issue(iss.clone(), None, None, String::new());
        }

        // The identical set, in the identical order.
        assert_eq!(
            *dispatched_with.lock().expect("lock"),
            *dispatched_without.lock().expect("lock"),
            "routing must not change WHICH issues dispatch, or in what order"
        );
        assert_eq!(dispatched_with.lock().expect("lock").len(), issues.len());

        // And every other dispatch observable is untouched.
        for iss in &issues {
            let a = &with.running[&iss.id];
            let b = &without.running[&iss.id];
            assert_eq!(a.issue, b.issue, "{}", iss.identifier);
            assert_eq!(a.project_slug, b.project_slug, "{}", iss.identifier);
            assert_eq!(a.project_group, b.project_group, "{}", iss.identifier);
            assert_eq!(a.project_repo, b.project_repo, "{}", iss.identifier);
            assert_eq!(a.model, b.model, "{}", iss.identifier);
            assert_eq!(a.retry_attempt, b.retry_attempt, "{}", iss.identifier);
            assert_eq!(
                a.capabilities_section, b.capabilities_section,
                "the capability section is BO-12's, not Teams' — it must not move ({})",
                iss.identifier
            );
            assert_eq!(a.stack_context, b.stack_context, "{}", iss.identifier);
            // The Teams-off side is inert everywhere.
            assert_eq!(b.identity, "", "{}", iss.identifier);
            assert_eq!(b.teammate_section, "", "{}", iss.identifier);
        }

        // The permitted difference, and the proof routing actually ran.
        assert_eq!(with.running["1"].identity, "alice", "tier-1 overlap");
        assert_eq!(with.running["2"].identity, "bob", "tier-0 label");
        assert_eq!(with.running["3"].identity, "", "nobody fits, no default");
        assert_eq!(with.running["4"].identity, "", "no labels at all");
    }

    /// §2.4 row 5, the inertness mechanism: an unrouted run carries an EMPTY
    /// teammate section, so the `if !x.is_empty()` guard skips it and the
    /// turn-1 prompt is byte-identical to one built before Teams existed.
    #[test]
    fn an_unrouted_run_leaves_the_turn_one_prompt_byte_identical() {
        let teams = teams_with(vec![ident("alice", &["rust"], 0)]);
        let (mut o, _) = orch_for_retry(Arc::new(Fake::new()), 10);
        o.teams = Some(teams);
        let iss = with_labels(&["docs"]);
        o.dispatch_issue(iss.clone(), None, None, String::new());
        assert_eq!(o.running["1"].identity, "");
        assert_eq!(o.running["1"].teammate_section, "");

        // The guard's own proof: an empty section renders the same bytes as the
        // pre-Teams call did.
        let baseline = crate::worker::build_turn_prompt(
            "Work {{ issue.identifier }}",
            "",
            "",
            "",
            &iss,
            None,
            1,
        )
        .expect("render");
        let with_empty_section = crate::worker::build_turn_prompt(
            "Work {{ issue.identifier }}",
            "",
            &o.running["1"].teammate_section,
            "",
            &iss,
            None,
            1,
        )
        .expect("render");
        assert_eq!(baseline, with_empty_section);
        assert_eq!(baseline, "Work MT-1");
    }

    /// A routed run's section lands in the turn-1 prompt AFTER the capability
    /// section (§0.11.6's fixed order) and BEFORE the rendered template.
    #[test]
    fn a_routed_run_prepends_the_teammate_section_after_capabilities() {
        let teams = teams_with(vec![ident("alice", &["rust"], 0)]);
        let (mut o, _) = orch_for_retry(Arc::new(Fake::new()), 10);
        o.teams = Some(teams);
        o.dispatch_issue(with_labels(&["rust"]), None, None, String::new());
        let section = o.running["1"].teammate_section.clone();
        assert!(
            section.starts_with("## You are working as alice"),
            "section = {section:?}"
        );

        let caps = "## Required practices for this ticket\n\nReview your own diff.";
        let prompt = crate::worker::build_turn_prompt(
            "Work {{ issue.identifier }}",
            caps,
            &section,
            "",
            &with_labels(&["rust"]),
            None,
            1,
        )
        .expect("render");
        assert_eq!(prompt, format!("{caps}\n\n{section}\n\nWork MT-1"));
    }

    /// **The §3.2 ⚠️ namespace-split test, which this slice owns.**
    ///
    /// `rhapsody:` is a shared prefix: `retry.rs` strips it from every ticket
    /// label and looks the remainder up in the BO-11 capabilities registry,
    /// where an unknown name is a documented silent no-op — and that no-op is
    /// now load-bearing for two consumers. This pins the split in BOTH
    /// directions on one dispatch: `rhapsody:@alice` reaches routing and
    /// contributes NOTHING to capabilities, `rhapsody:code-review` reaches
    /// capabilities and contributes NOTHING to routing, and neither breaks the
    /// other. Turning an unknown `rhapsody:*` label into an error or a warning
    /// would fail here rather than silently breaking Teams routing in
    /// production.
    #[test]
    fn rhapsody_namespace_splits_between_routing_and_capabilities() {
        let teams = teams_with(vec![
            ident("alice", &[], 0),
            // `code-review` is a CAPABILITY name, deliberately also declared as
            // a roster match label: the capability label must not route to bob.
            ident("bob", &["code-review"], 0),
        ]);
        let (mut o, _) = orch_for_retry(Arc::new(Fake::new()), 10);
        o.teams = Some(teams);
        o.capabilities_registry = Some(rhapsody_config::capabilities::default_capabilities());
        let iss = Issue {
            labels: Some(vec![
                "rhapsody:@alice".to_string(),
                "rhapsody:code-review".to_string(),
            ]),
            ..issue("1", "MT-1", "Todo")
        };
        o.dispatch_issue(iss, None, None, String::new());
        let re = &o.running["1"];

        // → routing: the `@` label named alice, and the capability label did not
        //   route to bob (whose roster label is the bare capability name).
        assert_eq!(re.identity, "alice");

        // → capabilities: `code-review` rendered, and the `@alice` label
        //   contributed nothing (an unknown `rhapsody:*` name is a silent no-op,
        //   and `@` cannot be a capability name — that is the discriminator).
        assert!(
            re.capabilities_section.contains("review your own diff"),
            "rhapsody:code-review must still reach the capabilities registry: {:?}",
            re.capabilities_section
        );
        assert!(
            !re.capabilities_section.contains("alice"),
            "the identity label must contribute nothing to capabilities: {:?}",
            re.capabilities_section
        );

        // Neither consumer damaged the other's output.
        assert!(
            re.capabilities_section
                .starts_with("## Required practices for this ticket"),
            "{:?}",
            re.capabilities_section
        );
        assert!(
            re.teammate_section
                .starts_with("## You are working as alice"),
            "{:?}",
            re.teammate_section
        );
    }

    /// §3.4: the decision is recorded as an `events` row on the run — a DATA
    /// value in the existing `kind` column, so no schema moves.
    #[test]
    fn dispatch_records_the_route_decision_as_an_event() {
        let teams = teams_with(vec![ident("alice", &["rust"], 0)]);
        let (mut o, store) = orch_with_teams(teams);
        o.dispatch_issue(with_labels(&["rust"]), None, None, String::new());
        let run_id = o.running["1"].run_id;
        assert_ne!(run_id, 0, "the run row must exist before the event");
        flush_events(&mut o);
        assert_eq!(
            events_of(store.as_ref(), run_id),
            vec![(
                "teams.route".to_string(),
                "identity=alice reason=label_overlap".to_string()
            )]
        );
    }

    /// §3.4: nobody fits and there is no default ⇒ a `teams.unrouted` row. The
    /// run still dispatched — the event is the only trace, which is what keeps
    /// non-routes countable after the fact.
    #[test]
    fn dispatch_records_teams_unrouted_when_nobody_fits() {
        let teams = teams_with(vec![ident("alice", &["rust"], 0)]);
        let (mut o, store) = orch_with_teams(teams);
        o.dispatch_issue(with_labels(&["docs"]), None, None, String::new());
        let run_id = o.running["1"].run_id;
        assert_eq!(o.running["1"].identity, "", "unrouted");
        flush_events(&mut o);
        assert_eq!(
            events_of(store.as_ref(), run_id),
            vec![("teams.unrouted".to_string(), "reason=no_match".to_string())]
        );
    }

    /// Teams OFF contributes nothing at all to a dispatch: no identity, no
    /// section, and — the part a golden would notice — NO event row (§2.4 row 4:
    /// no new row *kind* when off).
    #[test]
    fn teams_off_records_no_event_at_all() {
        let (mut o, store) = orch_with_teams(Teams::disabled());
        o.dispatch_issue(with_labels(&["rust"]), None, None, String::new());
        let run_id = o.running["1"].run_id;
        assert_eq!(o.running["1"].identity, "");
        assert_eq!(o.running["1"].teammate_section, "");
        flush_events(&mut o);
        assert!(
            events_of(store.as_ref(), run_id).is_empty(),
            "a Teams-off dispatch must write no teams event"
        );
    }

    /// §3.5: `mode: off` with no `default_identity` is "behaviour identical to
    /// `enabled: false`" — so it too writes no event and prepends nothing, even
    /// though `enabled` is true.
    #[test]
    fn mode_off_without_a_default_contributes_nothing_to_dispatch() {
        let mut teams = teams_with(vec![ident("alice", &["rust"], 0)]);
        teams.manager.mode = ManagerMode::Off;
        let (mut o, store) = orch_with_teams(teams);
        o.dispatch_issue(with_labels(&["rust"]), None, None, String::new());
        let run_id = o.running["1"].run_id;
        assert_eq!(o.running["1"].identity, "");
        assert_eq!(o.running["1"].teammate_section, "");
        flush_events(&mut o);
        assert!(
            events_of(store.as_ref(), run_id).is_empty(),
            "§3.5's `mode: off` with no default must be indistinguishable from `enabled: false`"
        );
    }

    /// The live-run tie-break reads the identities dispatch itself stamped: two
    /// equally-matched teammates alternate rather than piling onto the first.
    #[test]
    fn live_run_counts_come_from_previously_dispatched_runs() {
        let teams = teams_with(vec![
            ident("alice", &["rust"], 0),
            ident("bob", &["rust"], 0),
        ]);
        let (mut o, _) = orch_for_retry(Arc::new(Fake::new()), 10);
        o.teams = Some(teams);
        for (id, ident_) in [("1", "MT-1"), ("2", "MT-2"), ("3", "MT-3")] {
            let iss = Issue {
                labels: Some(vec!["rust".to_string()]),
                ..issue(id, ident_, "Todo")
            };
            o.dispatch_issue(iss, None, None, String::new());
        }
        assert_eq!(o.running["1"].identity, "alice", "roster order first");
        assert_eq!(o.running["2"].identity, "bob", "alice now has a live run");
        assert_eq!(o.running["3"].identity, "alice", "1 each ⇒ roster order");
    }

    /// A routed identity whose profile CANNOT be resolved dispatches WITHOUT the
    /// section rather than failing: a broken profile must not block work. The
    /// identity is still stamped and still recorded, so the decision stays
    /// visible.
    #[test]
    fn an_unresolvable_profile_drops_the_section_but_never_blocks_the_run() {
        let dir = crate::testsupport::TempDir::new();
        let mut teams = teams_with(vec![ident("alice", &["rust"], 0)]);
        teams.roster[0].profile = "no-such-profile".to_string();
        let (mut o, store) = orch_with_teams(teams);
        o.teams_profiles_dir = Some(std::path::Path::new(&dir.child("profiles")).to_path_buf());
        o.dispatch_issue(with_labels(&["rust"]), None, None, String::new());
        let run_id = o.running["1"].run_id;

        assert_eq!(o.running["1"].identity, "alice", "the run is still routed");
        assert_eq!(
            o.running["1"].teammate_section, "",
            "an unresolvable profile must contribute no section"
        );
        flush_events(&mut o);
        assert_eq!(
            events_of(store.as_ref(), run_id),
            vec![(
                "teams.route".to_string(),
                "identity=alice reason=label_overlap".to_string()
            )]
        );
    }

    /// The happy path for profiles: a roster entry naming a real built-in gets
    /// that profile's resolved prose in its section (T2's `profiles::resolve`).
    #[test]
    fn a_resolvable_profile_renders_into_the_teammate_section() {
        let dir = crate::testsupport::TempDir::new();
        let mut teams = teams_with(vec![ident("alice", &["rust"], 0)]);
        teams.roster[0].profile = "reviewer".to_string();
        let (mut o, _) = orch_with_teams(teams);
        // The directory does not exist: every profile resolves to its built-in.
        o.teams_profiles_dir = Some(std::path::Path::new(&dir.child("profiles")).to_path_buf());
        o.dispatch_issue(with_labels(&["rust"]), None, None, String::new());
        let section = o.running["1"].teammate_section.clone();
        assert!(
            section.starts_with("## You are working as alice"),
            "{section:?}"
        );
        assert!(
            section.contains("You are a code reviewer on this codebase."),
            "the resolved profile prose must be included: {section:?}"
        );
    }
}
