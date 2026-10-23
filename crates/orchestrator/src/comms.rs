//! comms — the communication-adapter seam (STUDIO-1503).
//!
//! **Communication is bidirectional; notification is one-way.** This module is the inbound axis:
//! a backend polls a surface, detects an inbound message, and folds it onto the candidate that
//! carries it, from which a run is re-engaged. Outbound notification is the separate, deliberately
//! unentangled [`crate::notify`] axis; nothing here sends.
//!
//! # What the seam abstracts, and what it deliberately does not
//!
//! Two backends already exist and work, and this is extracted from them rather than guessed at:
//!
//! * **GitHub** — [`GitHubComms`], the `gh`-exec poll of unmerged linked pull-request comments
//!   (the transport is [`crate::ghsummons::SummonSource`] / [`crate::ghenrich`]).
//! * **Linear** — [`LinearComms`], whose transport is the tracker's own candidate query: the
//!   Linear adapter folds a tokened issue comment onto `Issue::latest_summon_*` inside
//!   `normalize_issue`, so the adapter re-admits that already-normalized observation.
//!
//! The dimension that genuinely differs between backends is **how an agent's identity is
//! represented**, and it is declared per backend by
//! [`CommsBackend::identity`](CommsBackend::identity) rather than assumed by a caller: GitHub and
//! Linear both post through ONE shared account, so a name they report is a CLAIM and never a
//! verified identity; Buzz (5/5) holds a keypair per agent and its attribution is proven at the
//! transport. Message FORMATTING looks like the interesting problem and is not — it varies least,
//! and is downstream in [`crate::message`].
//!
//! # One chokepoint, not one per backend
//!
//! [`merge_inbound`] is the single place any backend's detection reaches a candidate. It carries
//! 1502's cross-source dedup: the field is **max-only**, so a ticket carrying both a Linear-comment
//! summons and a GitHub PR-comment summons is re-engaged once, not twice. And every existing
//! REFUSAL — `rhapsody:human`, non-terminal blockers, concurrency caps — stays where it already is,
//! in [`crate::select`]/[`crate::dispatch`], NOT per backend: this module detects and merges, and
//! deliberately decides nothing about eligibility.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rhapsody_config::Config;
use rhapsody_config::comms::{
    CommsIdentity, IdentityRepresentation, ResolvedComms, comms_backend_identity,
};
use rhapsody_core::Issue;

use crate::ghenrich::{DaemonPrLinks, UnlinkedSummons};
use crate::ghsummons::SummonHit;

/// GitHub's declared identity: one shared account posts every comment, and there is no author field
/// to read — so a GitHub detection carries no author and must not fabricate one.
const GITHUB_IDENTITY: CommsIdentity = CommsIdentity {
    representation: IdentityRepresentation::Textual,
    surfaces_author: false,
};

/// Linear's declared identity: a comment carries its author's display name, but through the same
/// shared token, so the name is a claim rather than a proof.
const LINEAR_IDENTITY: CommsIdentity = CommsIdentity {
    representation: IdentityRepresentation::Textual,
    surfaces_author: true,
};

/// One inbound message a backend detected for one candidate.
///
/// `author` is the sender's CLAIMED display name, or `None` when the backend cannot surface one.
/// A backend whose [`CommsBackend::identity`] reports `surfaces_author: false` must always send
/// `None` here — the seam never lets such a backend claim a sender it cannot see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// The candidate's opaque tracker id (`Issue::id`).
    pub issue_id: String,
    /// When the message was posted.
    pub at: DateTime<Utc>,
    /// The message body, carried as context for the re-engaged run.
    pub body: String,
    /// The sender's claimed display name, or `None` when the backend cannot surface one.
    pub author: Option<String>,
}

/// **The one chokepoint** every backend folds its detections through (STUDIO-1503).
///
/// Applies the 1502 cross-source rule: `latest_summon_at` is MAX-only, and the body, the author and
/// the time always describe the SAME message — a strictly-newer detection replaces all three
/// together; an equal-or-older one replaces nothing. A detection whose `author` is `None` (a backend
/// that cannot name the sender) CLEARS the author rather than inheriting the previous message's,
/// which is the STUDIO-1502 B3 rule applied once, for every backend.
///
/// Returns how many candidates advanced (the tests' seam; production ignores it). A detection naming
/// an id not in `issues` is ignored — a backend may report a candidate the caller has already
/// dropped, and that is not an error.
pub fn merge_inbound(issues: &mut [Issue], detected: &[Detected]) -> usize {
    let mut advanced = 0;
    for d in detected {
        let Some(iss) = issues.iter_mut().find(|i| i.id == d.issue_id) else {
            continue;
        };
        if iss.latest_summon_at.is_none_or(|current| d.at > current) {
            iss.latest_summon_at = Some(d.at);
            iss.latest_summon_body = d.body.clone();
            iss.latest_summon_author = d.author.clone().unwrap_or_default();
            advanced += 1;
        }
    }
    advanced
}

/// A communication backend (STUDIO-1503) — the seam both GitHub and Linear implement.
///
/// A backend DETECTS inbound messages and folds them through [`merge_inbound`]. It does not decide
/// eligibility: refusals (`rhapsody:human`, blockers, concurrency caps) are enforced downstream, in
/// one place, for every backend.
pub trait CommsBackend: Send + Sync {
    /// The backend's name, as `comms.backends` spells it.
    fn name(&self) -> &'static str;

    /// What identity this backend can express. A caller must not assume more than
    /// [`IdentityRepresentation::is_verified`] admits, and Rhapsody's own identity model stays
    /// authoritative regardless.
    fn identity(&self) -> CommsIdentity;

    /// Detect this backend's newest inbound message for each candidate and fold it through the one
    /// chokepoint. Returns every detected message that reached NO candidate, for the caller's drop
    /// report (a backend with nothing to report returns an empty vector).
    fn admit(&self, issues: &mut [Issue]) -> Vec<UnlinkedSummons>;
}

/// The GitHub backend: the `gh`-exec poll of unmerged linked pull-request comments.
///
/// Holds the per-tick fetched hits and the repository coordinates, so the (async) fetch stays split
/// from the (pure) apply, exactly as [`crate::ghenrich`] already had it.
pub struct GitHubComms<'a> {
    by_pr: &'a HashMap<i64, SummonHit>,
    owner: &'a str,
    repo: &'a str,
    links: &'a DaemonPrLinks,
}

impl<'a> GitHubComms<'a> {
    /// Builds the backend over one repository's pre-fetched hits.
    pub fn new(
        by_pr: &'a HashMap<i64, SummonHit>,
        owner: &'a str,
        repo: &'a str,
        links: &'a DaemonPrLinks,
    ) -> Self {
        Self {
            by_pr,
            owner,
            repo,
            links,
        }
    }
}

impl CommsBackend for GitHubComms<'_> {
    fn name(&self) -> &'static str {
        "github"
    }

    fn identity(&self) -> CommsIdentity {
        GITHUB_IDENTITY
    }

    /// The ported `applyGitHubSummons` walk, now folding through [`merge_inbound`]. Behaviour is
    /// byte-identical to the pre-seam function: the same max-only advance, the same author clearing,
    /// the same per-drop and per-pass diagnostics, and the same [`UnlinkedSummons`] report.
    fn admit(&self, issues: &mut [Issue]) -> Vec<UnlinkedSummons> {
        if self.by_pr.is_empty() {
            return Vec::new();
        }
        let (by_pr, owner, repo, links) = (self.by_pr, self.owner, self.repo, self.links);
        // STUDIO-574 observability: every drop below is a `continue` with no trace, so a hit that
        // never reaches an issue is invisible. Count each drop REASON and emit one line per call.
        let issue_count = issues.len();
        let (mut linked, mut other_repo, mut merged, mut no_hit, mut matched, mut advanced) =
            (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
        // STUDIO-882: how many pull requests the DAEMON's own record contributed that the tracker
        // did not.
        let mut daemon_linked = 0usize;
        // The pull requests the hits are ON, named on every drop report.
        let mut hit_prs: Vec<i64> = by_pr.keys().copied().collect();
        hit_prs.sort_unstable();
        let mut unlinked: Vec<UnlinkedSummons> = Vec::new();
        for iss in issues.iter_mut() {
            // Clone the PR refs out so the loop can mutate the issue's summon fields without
            // aliasing the `linked_prs` borrow.
            let prs = iss.linked_prs.clone().unwrap_or_default();
            linked += prs.len();
            let mut iss_other_repo = 0usize;
            let mut iss_pr_repos: Vec<String> = Vec::new();
            let mut iss_reachable = false;
            for pr in &prs {
                // GitHub owner/repo are case-insensitive.
                if !pr.owner.eq_ignore_ascii_case(owner) || !pr.repo.eq_ignore_ascii_case(repo) {
                    other_repo += 1;
                    iss_other_repo += 1;
                    let name = format!("{}/{}", pr.owner, pr.repo);
                    if !iss_pr_repos.contains(&name) {
                        iss_pr_repos.push(name);
                    }
                    continue;
                }
                if pr.merged {
                    merged += 1;
                    continue;
                }
                iss_reachable = true;
                let Some(hit) = by_pr.get(&pr.number) else {
                    no_hit += 1;
                    continue;
                };
                matched += 1;
                if iss.latest_summon_at.is_none_or(|current| hit.at > current) {
                    advanced += 1;
                    // GitHub supplies no author (one shared account), so the seam clears any older
                    // source's author with the body (STUDIO-1502 B3).
                    let issue_id = iss.id.clone();
                    merge_inbound(
                        std::slice::from_mut(iss),
                        &[Detected {
                            issue_id,
                            at: hit.at,
                            body: hit.body.clone(),
                            author: None,
                        }],
                    );
                    tracing::info!(issue_identifier = %iss.identifier, pr = pr.number, at = %hit.at, "github-summons: advanced latest_summon_at from PR comment");
                }
            }
            // STUDIO-882: the same walk over the pull requests the DAEMON recorded for this ticket.
            for number in links.numbers_for(owner, repo, &iss.identifier) {
                if prs.iter().any(|pr| {
                    pr.number == *number
                        && pr.owner.eq_ignore_ascii_case(owner)
                        && pr.repo.eq_ignore_ascii_case(repo)
                }) {
                    continue;
                }
                daemon_linked += 1;
                iss_reachable = true;
                let Some(hit) = by_pr.get(number) else {
                    no_hit += 1;
                    continue;
                };
                matched += 1;
                if iss.latest_summon_at.is_none_or(|current| hit.at > current) {
                    advanced += 1;
                    let issue_id = iss.id.clone();
                    merge_inbound(
                        std::slice::from_mut(iss),
                        &[Detected {
                            issue_id,
                            at: hit.at,
                            body: hit.body.clone(),
                            author: None,
                        }],
                    );
                    tracing::info!(issue_identifier = %iss.identifier, pr = number, at = %hit.at, "github-summons: advanced latest_summon_at from PR comment (daemon-recorded link)");
                }
            }
            let all_in_another_repo =
                iss_other_repo > 0 && iss_other_repo == prs.len() && !iss_reachable;
            if all_in_another_repo {
                tracing::info!(
                    issue_identifier = %iss.identifier,
                    polled_repo = %format!("{owner}/{repo}"),
                    pr_repos = %iss_pr_repos.join(", "),
                    linked_prs = prs.len(),
                    "github-summons: issue's linked PRs are all in another repo; summons on them can never re-engage it"
                );
            }
            if !iss_reachable && !all_in_another_repo {
                unlinked.push(UnlinkedSummons {
                    identifier: iss.identifier.clone(),
                    state: iss.state.clone(),
                    repo_prs: hit_prs.clone(),
                });
            }
        }
        tracing::debug!(
            repo = %format!("{owner}/{repo}"),
            hits = by_pr.len(),
            issues = issue_count,
            linked_prs_total = linked,
            daemon_links = daemon_linked,
            skipped_other_repo = other_repo,
            skipped_merged = merged,
            skipped_no_hit = no_hit,
            matched,
            advanced,
            "github-summons: applied PR summons"
        );
        unlinked
    }
}

/// The Linear backend: the tracker's own candidate query.
///
/// The Linear adapter folds a tokened issue comment onto `Issue::latest_summon_*` inside
/// `normalize_issue`, so by the time a candidate reaches the orchestrator the observation is already
/// on the issue. This adapter RE-ADMITS that observation through the one chokepoint: an equal time
/// replaces nothing, so it is a production no-op, but it makes the cross-source merge a single
/// implementation both backends pass through, and it declares Linear's identity (textual, and it
/// CAN name the sender) rather than leaving it implicit.
pub struct LinearComms;

impl CommsBackend for LinearComms {
    fn name(&self) -> &'static str {
        "linear"
    }

    fn identity(&self) -> CommsIdentity {
        LINEAR_IDENTITY
    }

    fn admit(&self, issues: &mut [Issue]) -> Vec<UnlinkedSummons> {
        let detected: Vec<Detected> = issues
            .iter()
            .filter_map(|iss| {
                iss.latest_summon_at.map(|at| Detected {
                    issue_id: iss.id.clone(),
                    at,
                    body: iss.latest_summon_body.clone(),
                    // Linear CAN surface an author; an empty one (a comment with no user) is `None`
                    // rather than an empty claim.
                    author: (!iss.latest_summon_author.is_empty())
                        .then(|| iss.latest_summon_author.clone()),
                })
            })
            .collect();
        merge_inbound(issues, &detected);
        Vec::new()
    }
}

/// The resolved, build-checked set of communication backends for one config (STUDIO-1503).
///
/// `active` names the backends this build will run; `degraded` holds every resolved name that is
/// recognized-but-unimplemented (or unknown), each of which is warned about and skipped rather than
/// failing a dispatch — the `codex` behaviour on the harness axis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommsRegistry {
    /// The implemented backend names this build will run, in resolution order.
    pub active: Vec<&'static str>,
    /// Recognized-but-unimplemented (or unknown) names that were skipped, with their origin.
    pub degraded: Vec<ResolvedComms>,
}

impl CommsRegistry {
    /// Resolves `cfg`'s communication backends and separates the ones this build can run from the
    /// ones it must degrade. `none` is implemented and inert: it resolves to no active backend.
    pub fn from_config(cfg: &Config) -> Self {
        let mut active = Vec::new();
        let mut degraded = Vec::new();
        for r in cfg.resolve_comms() {
            if !r.is_implemented() {
                degraded.push(r);
                continue;
            }
            match r.backend.as_str() {
                "github" => active.push("github"),
                "linear" => active.push("linear"),
                // `none` and any future implemented-but-inert name: nothing to run.
                _ => {}
            }
        }
        Self { active, degraded }
    }

    /// Whether this build will run `name`.
    pub fn is_active(&self, name: &str) -> bool {
        self.active.contains(&name)
    }

    /// Logs one warning per degraded backend, naming the backend, its origin and what this build
    /// can run instead. Returns how many it logged (the test seam; production ignores it).
    ///
    /// The warning is the whole point of the graceful degradation: a recognized name this build
    /// cannot run is a configuration the operator must see, and a dispatch proceeds regardless.
    pub fn warn_degraded(&self) -> usize {
        let mut warned = 0usize;
        for r in &self.degraded {
            warned += 1;
            let detail = if r.is_recognized() {
                "recognized but not implemented in this build"
            } else {
                "not a recognized backend name"
            };
            tracing::warn!(
                backend = %r.backend,
                origin = %r.origin.as_str(),
                "comms: skipping a communication backend this build cannot run ({detail}); \
                 inbound summons from it will not arrive, and dispatch is unaffected"
            );
        }
        warned
    }
}

/// The identity a resolved backend name declares, for callers that hold only the name.
pub fn identity_for_backend(name: &str) -> Option<CommsIdentity> {
    comms_backend_identity(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_core::LinkedPRRef;

    fn issue(id: &str) -> Issue {
        Issue {
            id: id.to_string(),
            identifier: id.to_string(),
            state: "In Review".to_string(),
            ..Issue::default()
        }
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_756_000_000 + secs, 0).expect("timestamp")
    }

    /// The chokepoint is MAX-only: a strictly-newer detection replaces time, body and author
    /// together; an equal-or-older one replaces nothing.
    #[test]
    fn merge_inbound_is_max_only() {
        let mut issues = vec![issue("A-1")];
        issues[0].latest_summon_at = Some(at(100));
        issues[0].latest_summon_body = "old".to_string();
        issues[0].latest_summon_author = "Ada".to_string();

        // Older and equal both lose.
        let n = merge_inbound(
            &mut issues,
            &[
                Detected {
                    issue_id: "A-1".into(),
                    at: at(50),
                    body: "older".into(),
                    author: Some("Bob".into()),
                },
                Detected {
                    issue_id: "A-1".into(),
                    at: at(100),
                    body: "equal".into(),
                    author: Some("Bob".into()),
                },
            ],
        );
        assert_eq!(n, 0);
        assert_eq!(issues[0].latest_summon_body, "old");
        assert_eq!(issues[0].latest_summon_author, "Ada");

        // Newer wins, and body/author ride together.
        let n = merge_inbound(
            &mut issues,
            &[Detected {
                issue_id: "A-1".into(),
                at: at(200),
                body: "new".into(),
                author: Some("Cara".into()),
            }],
        );
        assert_eq!(n, 1);
        assert_eq!(issues[0].latest_summon_at, Some(at(200)));
        assert_eq!(issues[0].latest_summon_body, "new");
        assert_eq!(issues[0].latest_summon_author, "Cara");
    }

    /// **A backend that cannot surface an author does not silently claim one** (acceptance #3).
    /// GitHub declares `surfaces_author: false`, so its detection carries `author: None`, and the
    /// chokepoint CLEARS the older source's author rather than inheriting it (STUDIO-1502 B3).
    #[test]
    fn a_textual_backend_that_cannot_surface_an_author_does_not_claim_more() {
        assert_eq!(
            GitHubComms::new(&HashMap::new(), "o", "r", &DaemonPrLinks::default()).identity(),
            GITHUB_IDENTITY
        );
        assert_eq!(
            GITHUB_IDENTITY.representation,
            IdentityRepresentation::Textual
        );
        assert!(!GITHUB_IDENTITY.representation.is_verified());
        assert!(
            !CommsBackend::identity(&GitHubComms::new(
                &HashMap::new(),
                "o",
                "r",
                &DaemonPrLinks::default()
            ))
            .surfaces_author
        );
        assert_eq!(LinearComms.identity(), LINEAR_IDENTITY);
        assert!(CommsBackend::identity(&LinearComms).surfaces_author);

        // A GitHub detection must not inherit a Linear author it cannot see.
        let mut issues = vec![issue("A-1")];
        issues[0].latest_summon_at = Some(at(10));
        issues[0].latest_summon_author = "Agent Ada".to_string();
        merge_inbound(
            &mut issues,
            &[Detected {
                issue_id: "A-1".into(),
                at: at(20),
                body: "from the PR".into(),
                author: None,
            }],
        );
        assert_eq!(
            issues[0].latest_summon_author, "",
            "a backend that cannot name the sender must not claim one"
        );
    }

    /// The GitHub and Linear backends merge onto ONE watermark (the 1502 cross-source dedup), and
    /// the newer source wins. This is the dedup test run through the seam rather than around it.
    #[test]
    fn the_two_backends_merge_onto_one_watermark() {
        let mut issues = vec![issue("A-1")];
        issues[0].linked_pr = true;
        issues[0].linked_prs = Some(vec![LinkedPRRef {
            owner: "o".into(),
            repo: "r".into(),
            number: 7,
            merged: false,
        }]);
        // Source 1 — Linear normalized a tokened issue comment onto the issue.
        issues[0].latest_summon_at = Some(at(100));
        issues[0].latest_summon_body = "@symphony from Linear".into();
        issues[0].latest_summon_author = "Agent Ada".into();
        // Source 2 — GitHub advanced the SAME field with a newer PR comment.
        let by_pr = HashMap::from([(
            7,
            SummonHit {
                at: at(200),
                body: "@symphony from the PR".into(),
            },
        )]);
        let links = DaemonPrLinks::default();
        let github = GitHubComms::new(&by_pr, "o", "r", &links);
        let unlinked = github.admit(&mut issues);
        assert!(unlinked.is_empty());
        // The Linear backend re-admits its own (older) observation and changes nothing.
        let n = merge_inbound(
            &mut issues,
            &[Detected {
                issue_id: "A-1".into(),
                at: at(100),
                body: "@symphony from Linear".into(),
                author: Some("Agent Ada".into()),
            }],
        );
        assert_eq!(
            n, 0,
            "the older Linear observation must not re-open the field"
        );
        assert_eq!(issues[0].latest_summon_at, Some(at(200)));
        assert_eq!(issues[0].latest_summon_body, "@symphony from the PR");
        assert_eq!(issues[0].latest_summon_author, "");
    }

    /// The Linear adapter re-admits what the tracker already folded: a no-op that declares the
    /// backend and routes through the one chokepoint.
    #[test]
    fn the_linear_backend_re_admits_without_changing_the_issue() {
        let mut issues = vec![issue("A-1")];
        issues[0].latest_summon_at = Some(at(100));
        issues[0].latest_summon_body = "@symphony from Linear".into();
        issues[0].latest_summon_author = "Agent Ada".into();
        let before = issues[0].clone();
        let unlinked = LinearComms.admit(&mut issues);
        assert!(unlinked.is_empty());
        assert_eq!(issues[0], before, "re-admitting an equal time is a no-op");
    }

    /// Acceptance #2: a recognized-but-unimplemented backend name degrades with a warning rather
    /// than failing a dispatch — the `codex` behaviour on the harness axis. The registry skips it
    /// (nothing is active), reports it as degraded, and the warning names it.
    #[test]
    fn a_recognized_but_unimplemented_backend_degrades_with_a_warning() {
        let cfg = crate::testsupport::config_with_comms_backends(&["buzz"]);
        let registry = CommsRegistry::from_config(&cfg);
        assert!(
            registry.active.is_empty(),
            "buzz has no transport in this build"
        );
        assert_eq!(registry.degraded.len(), 1);
        assert_eq!(registry.degraded[0].backend, "buzz");
        assert!(registry.degraded[0].is_recognized());
        assert!(!registry.degraded[0].is_implemented());

        let warned = registry.warn_degraded();
        assert_eq!(warned, 1);
        assert!(
            warned >= 1,
            "a degraded backend must be warned about, not silent"
        );
    }

    /// A mixed list keeps the runnable backends and degrades only the others.
    #[test]
    fn a_mixed_backend_list_keeps_the_implemented_ones() {
        let cfg = crate::testsupport::config_with_comms_backends(&["github", "buzz", "linear"]);
        let registry = CommsRegistry::from_config(&cfg);
        assert_eq!(registry.active, vec!["github", "linear"]);
        assert!(registry.is_active("github") && registry.is_active("linear"));
        assert_eq!(registry.degraded.len(), 1);
        assert_eq!(registry.degraded[0].backend, "buzz");
    }

    /// An unknown name degrades too, and is distinguished from a recognized one in the warning.
    #[test]
    fn an_unknown_backend_name_degrades_and_is_not_recognized() {
        let cfg = crate::testsupport::config_with_comms_backends(&["telegram"]);
        let registry = CommsRegistry::from_config(&cfg);
        assert!(registry.active.is_empty());
        assert_eq!(registry.degraded.len(), 1);
        assert!(!registry.degraded[0].is_recognized());
        assert_eq!(registry.warn_degraded(), 1);
    }

    /// `none` resolves to no active backend and nothing degraded — an explicit off switch.
    #[test]
    fn none_is_inert() {
        let cfg = crate::testsupport::config_with_comms_backends(&["none"]);
        let registry = CommsRegistry::from_config(&cfg);
        assert!(registry.active.is_empty());
        assert!(registry.degraded.is_empty());
        assert_eq!(registry.warn_degraded(), 0);
    }

    /// The seam touches no refusal: a candidate wearing `rhapsody:human` or blocked is still
    /// admitted here — the refusal lives in `select`/`dispatch`, for every backend, in one place.
    #[test]
    fn merge_inbound_decides_no_eligibility() {
        let mut issues = vec![issue("A-1")];
        issues[0].labels = Some(vec!["rhapsody:human".to_string()]);
        issues[0].state = "Todo".to_string();
        let n = merge_inbound(
            &mut issues,
            &[Detected {
                issue_id: "A-1".into(),
                at: at(10),
                body: "@symphony".into(),
                author: None,
            }],
        );
        assert_eq!(
            n, 1,
            "the seam admits the message; eligibility is decided downstream"
        );
    }

    /// The warning is a real tracing event, not merely a counter — captured the way the crate's
    /// other diagnostics are.
    #[test]
    fn the_degradation_warning_is_emitted() {
        let cfg = crate::testsupport::config_with_comms_backends(&["buzz"]);
        let registry = CommsRegistry::from_config(&cfg);
        let (warned, events) = crate::testsupport::capture_events(|| registry.warn_degraded());
        assert_eq!(warned, 1);
        assert!(
            events
                .iter()
                .any(|e| e.message.contains("skipping a communication backend")),
            "expected the degradation warning, got {events:?}"
        );
    }
}
