//! Issue normalization — parity port of `internal/tracker/linear/normalize.go` (upstream §11.3).
//!
//! [`RawIssue`] mirrors Linear's GraphQL issue shape (the schema-sensitive surface);
//! [`Client::normalize_issue`] converts one into a [`core::Issue`](rhapsody_core::Issue). It is a
//! method on [`Client`] because it reads the configured summon token when computing the newest
//! summons. Field mapping, blocker-edge selection (`type == "blocks"`), GitHub-PR attachment
//! parsing, and RFC3339 time handling are byte-for-byte behavior — mirrored test-by-test below.

use super::Client;
use chrono::{DateTime, Utc};
use regex::Regex;
use rhapsody_core::{BlockerRef, Issue, LinkedPRRef, normalize_state};
use serde::Deserialize;
use std::sync::LazyLock;

// GitHub-PR url matchers (normalize.go's `prURLRe` / `prParseRe` / `prNumberRe`). Compiled once;
// `None` only if the constant pattern failed to compile (impossible), in which case PR detection
// degrades to "no PR" rather than panicking — the same no-`MustCompile` decision as
// `core::compile_summon_re`. `PR_NUMBER_RE` + `pr_number_from_attachments` back the graphite
// stacking hint (FetchIssueBranchByID, P3 T4).
static PR_URL_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"/pull/[0-9]+").ok());
static PR_PARSE_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"github\.com/([^/]+)/([^/]+)/pull/(\d+)").ok());
static PR_NUMBER_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"/pull/([0-9]+)").ok());

/// One `{ name }` GraphQL node (an issue/relation state, or a label).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawName {
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    name: String,
}

/// A `{ nodes: [...] }` GraphQL connection.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawNodes<T> {
    nodes: Vec<T>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawTeam {
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawAssignee {
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    id: String,
    #[serde(rename = "displayName")]
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    display_name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawMilestone {
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    id: String,
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawRelIssue {
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    id: String,
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    identifier: String,
    state: RawName,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawRelation {
    #[serde(rename = "type")]
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    rel_type: String,
    issue: RawRelIssue,
}

/// Linear's per-integration attachment metadata (a JSONObject). Only the fields normalize reads
/// are typed; for a GitHub PR it carries the PR url (`.../pull/N`) + status/timestamps.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawMetadata {
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    url: String,
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    status: String,
    #[serde(rename = "updatedAt")]
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    updated_at: String,
    #[serde(rename = "mergedAt")]
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    merged_at: String,
    #[serde(rename = "createdAt")]
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    created_at: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawAttachment {
    #[serde(rename = "sourceType")]
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    source_type: String,
    metadata: RawMetadata,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawComment {
    #[serde(rename = "createdAt")]
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    created_at: String,
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    body: String,
}

/// Linear's GraphQL issue shape (normalize.go's `rawIssue`). Fields absent from a response fall
/// back to their zero value (the container `#[serde(default)]`), mirroring Go's `encoding/json`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RawIssue {
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    id: String,
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    identifier: String,
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    title: String,
    description: Option<String>,
    priority: Option<f64>,
    url: Option<String>,
    #[serde(rename = "branchName")]
    branch_name: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<String>,
    state: RawName,
    team: RawTeam,
    assignee: Option<RawAssignee>,
    #[serde(rename = "projectMilestone")]
    project_milestone: Option<RawMilestone>,
    labels: RawNodes<RawName>,
    #[serde(rename = "inverseRelations")]
    inverse_relations: RawNodes<RawRelation>,
    attachments: RawNodes<RawAttachment>,
    comments: RawNodes<RawComment>,
}

impl RawIssue {
    /// The issue's Linear `gitBranchName`, or `None` when unset — the `n.BranchName` access in
    /// FetchIssueBranchByID. Exposed for the sibling `backlog` module because `RawIssue`'s fields
    /// are private to this module.
    pub(in crate::linear) fn git_branch_name(&self) -> Option<&str> {
        self.branch_name.as_deref()
    }

    /// The issue's Linear `identifier` (e.g. `STUDIO-406`). Exposed for the sibling `decode` module,
    /// whose tests assert which issues survived a partially-undecodable page.
    #[cfg(test)]
    pub(in crate::linear) fn identifier(&self) -> &str {
        &self.identifier
    }
}

impl Client {
    /// Converts a [`RawIssue`] into a [`core::Issue`](rhapsody_core::Issue) (normalize.go's
    /// `normalizeIssue`). Labels are lowercased; only `type == "blocks"` relations become
    /// [`BlockedBy`](rhapsody_core::Issue::blocked_by) edges (empty nodes skipped); linked GitHub
    /// PRs (open OR merged) set `linked_pr`/`linked_prs`/`latest_pr_activity_at`; and the newest
    /// comment whose body contains the summon token sets `latest_summon_at`/`latest_summon_body`.
    pub fn normalize_issue(&self, r: RawIssue) -> Issue {
        let mut iss = Issue {
            id: r.id,
            identifier: r.identifier,
            title: r.title,
            description: r.description,
            url: r.url,
            branch_name: r.branch_name,
            state: r.state.name,
            team_id: r.team.id,
            priority: int_priority(r.priority),
            created_at: parse_time(r.created_at.as_deref()),
            updated_at: parse_time(r.updated_at.as_deref()),
            ..Issue::default()
        };
        if let Some(a) = r.assignee {
            iss.assignee_id = a.id;
            iss.assignee_name = a.display_name;
        }
        if let Some(m) = r.project_milestone {
            iss.milestone_id = m.id;
            iss.milestone_name = m.name;
        }
        for l in r.labels.nodes {
            iss.labels
                .get_or_insert_with(Vec::new)
                .push(normalize_state(&l.name)); // lowercase
        }
        for rel in r.inverse_relations.nodes {
            if rel.rel_type != "blocks" {
                continue;
            }
            // Skip empty blocker nodes so we don't emit a junk BlockerRef of empty-string values.
            if rel.issue.id.is_empty() {
                continue;
            }
            iss.blocked_by
                .get_or_insert_with(Vec::new)
                .push(BlockerRef {
                    id: Some(rel.issue.id),
                    identifier: Some(rel.issue.identifier),
                    state: Some(rel.issue.state.name),
                });
        }
        // Linked GitHub PRs (open OR merged): track whether any PR is linked and the latest PR
        // activity time (the comment-reopen watermark).
        for a in r.attachments.nodes {
            if !is_github_pr(&a.source_type, &a.metadata.url) {
                continue; // not a PR (e.g. a doc/commit attachment, or a non-GitHub source)
            }
            iss.linked_pr = true;
            if let Some((owner, repo, number)) = parse_pr_url(&a.metadata.url) {
                let merged = a.metadata.status == "merged" || !a.metadata.merged_at.is_empty();
                iss.linked_prs
                    .get_or_insert_with(Vec::new)
                    .push(LinkedPRRef {
                        owner,
                        repo,
                        number,
                        merged,
                    });
            }
            if let Some(t) = pr_activity_at(
                &a.metadata.updated_at,
                &a.metadata.merged_at,
                &a.metadata.created_at,
            ) && iss.latest_pr_activity_at.is_none_or(|cur| t > cur)
            {
                iss.latest_pr_activity_at = Some(t);
            }
        }
        // LatestSummonAt = the newest time of a comment whose body contains the summon token as a
        // standalone mention (word-boundary match, case-insensitive — see compile_summon_re). We
        // take the MAX over the fetched window. A nil matcher (impossible) means no summon detected,
        // mirroring normalize.go's `c.summonRe == nil` guard.
        if let Some(re) = self.summon_re.as_ref() {
            for cm in r.comments.nodes {
                if !re.is_match(&cm.body) {
                    continue;
                }
                if let Some(t) = parse_time(Some(&cm.created_at))
                    && iss.latest_summon_at.is_none_or(|cur| t > cur)
                {
                    iss.latest_summon_at = Some(t);
                    iss.latest_summon_body = cm.body; // body of the SAME (newest) summons (INF-448)
                }
            }
        }
        iss
    }
}

/// Parses an ISO-8601 (RFC3339) timestamp; nil/empty/unparseable → `None` (normalize.go's
/// `parseTime`). The result is normalized to UTC, mirroring Go's `t.UTC()`. Shared with the sibling
/// `claim` module, which parses each comment's `createdAt` into a [`core::Comment`](rhapsody_core::Comment).
pub(in crate::linear) fn parse_time(s: Option<&str>) -> Option<DateTime<Utc>> {
    let s = s?;
    if s.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Returns a priority only when `p` is a whole number (normalize.go's `intPriority`).
fn int_priority(p: Option<f64>) -> Option<i64> {
    let p = p?;
    if p.trunc() != p {
        return None;
    }
    Some(p as i64)
}

/// Reports whether an attachment is a linked GitHub pull request (normalize.go's `isGithubPR`):
/// GitHub-sourced and whose url is a PR url (`.../pull/<number>`).
fn is_github_pr(source_type: &str, url: &str) -> bool {
    source_type == "github" && PR_URL_RE.as_ref().is_some_and(|re| re.is_match(url))
}

/// Extracts (owner, repo, number) from a GitHub PR url; `None` if it doesn't match
/// (normalize.go's `parsePRURL`).
fn parse_pr_url(u: &str) -> Option<(String, String, i64)> {
    let caps = PR_PARSE_RE.as_ref()?.captures(u)?;
    let owner = caps.get(1)?.as_str().to_string();
    let repo = caps.get(2)?.as_str().to_string();
    let number = caps.get(3)?.as_str().parse::<i64>().ok()?;
    Some((owner, repo, number))
}

/// The most representative PR activity time, preferring `updatedAt`, then `mergedAt`, then
/// `createdAt`; `None` if none parse (normalize.go's `prActivityAt`).
fn pr_activity_at(updated_at: &str, merged_at: &str, created_at: &str) -> Option<DateTime<Utc>> {
    parse_time(Some(updated_at))
        .or_else(|| parse_time(Some(merged_at)))
        .or_else(|| parse_time(Some(created_at)))
}

/// Returns the PR number of the first linked GitHub PR attachment on `r`, or 0 when none parses
/// (normalize.go's `prNumberFromAttachments`). Best-effort: the number is decorative graphite
/// stacking context, never load-bearing (FetchIssueBranchByID). INF-318.
pub(super) fn pr_number_from_attachments(r: &RawIssue) -> i64 {
    for a in &r.attachments.nodes {
        if !is_github_pr(&a.source_type, &a.metadata.url) {
            continue;
        }
        if let Some(re) = PR_NUMBER_RE.as_ref()
            && let Some(caps) = re.captures(&a.metadata.url)
            && let Some(m) = caps.get(1)
            && let Ok(n) = m.as_str().parse::<i64>()
        {
            return n;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::super::{Config, new};
    use super::*;
    use chrono::TimeZone;

    /// Builds a Client carrying the given summon token (empty => the default `@symphony` applied
    /// by `new`) for exercising `normalize_issue` directly (mirrors Go's `normClient`).
    fn norm_client(token: &str) -> Client {
        new(Config {
            endpoint: "http://x".into(),
            api_key: "k".into(),
            project_slug: "p".into(),
            summon_token: token.into(),
            ..Config::default()
        })
    }

    fn parse(raw: &str) -> RawIssue {
        serde_json::from_str(raw).expect("rawIssue JSON")
    }

    // Mirrors Go TestNormalizeFullIssue.
    #[test]
    fn normalize_full_issue() {
        let raw = r#"{
          "id": "uuid-1",
          "identifier": "MT-9",
          "title": "Fix login",
          "description": "broken",
          "priority": 2,
          "url": "https://linear.app/x/MT-9",
          "branchName": "feature/mt-9",
          "createdAt": "2026-02-24T20:10:12.000Z",
          "updatedAt": "2026-02-25T08:00:00.000Z",
          "state": { "name": "In Progress" },
          "team": { "id": "team-uuid-1" },
          "labels": { "nodes": [ { "name": "Bug" }, { "name": "AUTH" } ] },
          "inverseRelations": { "nodes": [
            { "type": "blocks", "issue": { "id": "b1", "identifier": "MT-1", "state": { "name": "Todo" } } },
            { "type": "related", "issue": { "id": "r1", "identifier": "MT-2", "state": { "name": "Done" } } }
          ] }
        }"#;
        let iss = norm_client("").normalize_issue(parse(raw));

        assert_eq!(iss.id, "uuid-1");
        assert_eq!(iss.identifier, "MT-9");
        assert_eq!(iss.title, "Fix login");
        assert_eq!(iss.state, "In Progress");
        assert_eq!(iss.team_id, "team-uuid-1");
        assert_eq!(iss.description.as_deref(), Some("broken"));
        assert_eq!(iss.priority, Some(2));
        assert_eq!(iss.url.as_deref(), Some("https://linear.app/x/MT-9"));
        assert_eq!(iss.branch_name.as_deref(), Some("feature/mt-9"));
        assert_eq!(
            iss.labels.as_deref(),
            Some(["bug".to_string(), "auth".to_string()].as_slice())
        );

        let blockers = iss.blocked_by.as_deref().unwrap_or_default();
        assert_eq!(
            blockers.len(),
            1,
            "expected exactly one blocker (type=blocks)"
        );
        assert_eq!(blockers[0].identifier.as_deref(), Some("MT-1"));
        assert_eq!(blockers[0].state.as_deref(), Some("Todo"));

        assert_eq!(
            iss.created_at,
            Some(Utc.with_ymd_and_hms(2026, 2, 24, 20, 10, 12).unwrap())
        );
        assert!(iss.updated_at.is_some(), "updatedAt nil");
    }

    // Mirrors Go TestNormalizeOptionalFieldsNil.
    #[test]
    fn normalize_optional_fields_nil() {
        let raw = r#"{
          "id": "u2", "identifier": "MT-3", "title": "t",
          "description": null, "priority": null, "url": null, "branchName": null,
          "createdAt": null, "updatedAt": "not-a-date",
          "state": { "name": "Todo" },
          "labels": { "nodes": [] },
          "inverseRelations": { "nodes": [] }
        }"#;
        let iss = norm_client("").normalize_issue(parse(raw));
        assert!(
            iss.description.is_none()
                && iss.priority.is_none()
                && iss.url.is_none()
                && iss.branch_name.is_none()
        );
        assert!(iss.created_at.is_none(), "createdAt should be nil");
        assert!(
            iss.updated_at.is_none(),
            "unparseable updatedAt should be nil"
        );
        assert!(
            iss.labels.is_none() && iss.blocked_by.is_none(),
            "labels/blockers should be empty (None)"
        );
    }

    // Mirrors Go TestNormalizePriorityNonInteger.
    #[test]
    fn normalize_priority_non_integer() {
        let raw =
            r#"{"id":"x","identifier":"MT-5","title":"t","priority":2.5,"state":{"name":"Todo"}}"#;
        let iss = norm_client("").normalize_issue(parse(raw));
        assert!(
            iss.priority.is_none(),
            "non-integer priority should be None"
        );
    }

    // Mirrors Go TestNormalizePRAttachmentsAndComments.
    #[test]
    fn normalize_pr_attachments_and_comments() {
        let raw = r#"{
          "id": "u", "identifier": "INF-191", "title": "t", "state": { "name": "In Review" },
          "labels": { "nodes": [] },
          "inverseRelations": { "nodes": [] },
          "attachments": { "nodes": [
            { "sourceType": "github", "metadata": { "url": "https://github.com/o/r/pull/1785", "status": "merged", "updatedAt": "2026-06-03T17:58:29.000Z", "mergedAt": "2026-06-03T17:58:28.000Z", "createdAt": "2026-06-03T17:43:48.000Z" } },
            { "sourceType": "github", "metadata": { "url": "https://github.com/o/r/commit/deadbeef" } },
            { "sourceType": "linear", "metadata": { "url": "https://uploads.linear.app/spec.md" } }
          ] },
          "comments": { "nodes": [ { "createdAt": "2026-06-03T18:10:00.000Z", "body": "ping @Symphony please look" }, { "createdAt": "2026-06-03T19:00:00.000Z", "body": "just a plain follow-up, no token" }, { "createdAt": "2026-06-03T17:30:00.000Z", "body": "earlier note" } ] }
        }"#;
        let iss = norm_client("").normalize_issue(parse(raw));
        assert!(
            iss.linked_pr,
            "expected LinkedPR=true (one github /pull/ attachment)"
        );
        // LatestPRActivityAt = the PR's updatedAt; the commit + linear-doc attachments are ignored.
        assert_eq!(
            iss.latest_pr_activity_at,
            Some(Utc.with_ymd_and_hms(2026, 6, 3, 17, 58, 29).unwrap()),
            "LatestPRActivityAt should be the PR updatedAt"
        );
        // LatestSummonAt = the newest comment whose BODY contains the token. The 19:00 comment has
        // no token (ignored), so the 18:10 "@Symphony" comment is the summon.
        assert_eq!(
            iss.latest_summon_at,
            Some(Utc.with_ymd_and_hms(2026, 6, 3, 18, 10, 0).unwrap()),
            "LatestSummonAt should be the newest tokened comment (18:10)"
        );
    }

    // Mirrors Go TestNormalizeSummonDetection.
    #[test]
    fn normalize_summon_detection() {
        // Two comments, NEITHER containing the token → no summons.
        let raw = r#"{
          "id": "u", "identifier": "MT-7", "title": "t", "state": { "name": "In Review" },
          "labels": { "nodes": [] }, "inverseRelations": { "nodes": [] },
          "comments": { "nodes": [
            { "createdAt": "2026-06-03T10:00:00.000Z", "body": "looks good" },
            { "createdAt": "2026-06-03T11:00:00.000Z", "body": "thanks" }
          ] }
        }"#;
        assert!(
            norm_client("")
                .normalize_issue(parse(raw))
                .latest_summon_at
                .is_none(),
            "no tokened comment → LatestSummonAt should be None"
        );

        // A custom token, matched case-insensitively in the body.
        let raw2 = r#"{
          "id": "u", "identifier": "MT-8", "title": "t", "state": { "name": "In Review" },
          "labels": { "nodes": [] }, "inverseRelations": { "nodes": [] },
          "comments": { "nodes": [
            { "createdAt": "2026-06-03T12:30:00.000Z", "body": "hey @BOT can you take this" }
          ] }
        }"#;
        assert_eq!(
            norm_client("@bot")
                .normalize_issue(parse(raw2))
                .latest_summon_at,
            Some(Utc.with_ymd_and_hms(2026, 6, 3, 12, 30, 0).unwrap()),
            "custom token @bot should match @BOT case-insensitively"
        );

        // False positives: the token EMBEDDED in a larger token (URL path, suffixed word, email)
        // must NOT count — the match is word-boundary, not raw substring.
        let raw3 = r#"{
          "id": "u", "identifier": "MT-9", "title": "t", "state": { "name": "In Review" },
          "labels": { "nodes": [] }, "inverseRelations": { "nodes": [] },
          "comments": { "nodes": [
            { "createdAt": "2026-06-03T13:00:00.000Z", "body": "see https://github.com/@symphony/repo for context" },
            { "createdAt": "2026-06-03T13:01:00.000Z", "body": "ask @symphonybot, not us" },
            { "createdAt": "2026-06-03T13:02:00.000Z", "body": "mail foo@symphony.example" }
          ] }
        }"#;
        assert!(
            norm_client("")
                .normalize_issue(parse(raw3))
                .latest_summon_at
                .is_none(),
            "embedded-token comments (URL / suffix / email) must NOT count as a summons"
        );

        // Positive sanity: a real mention at start-of-body DOES match.
        let raw4 = r#"{
          "id": "u", "identifier": "MT-10", "title": "t", "state": { "name": "In Review" },
          "labels": { "nodes": [] }, "inverseRelations": { "nodes": [] },
          "comments": { "nodes": [
            { "createdAt": "2026-06-03T14:00:00.000Z", "body": "@symphony please re-run the failing test" }
          ] }
        }"#;
        assert!(
            norm_client("")
                .normalize_issue(parse(raw4))
                .latest_summon_at
                .is_some(),
            "a real @symphony mention at start-of-body must be detected"
        );
    }

    // Mirrors Go TestNormalizeCapturesNewestSummonBody.
    #[test]
    fn normalize_captures_newest_summon_body() {
        let raw = r#"{
          "id": "u", "identifier": "MT-11", "title": "t", "state": { "name": "In Progress" },
          "labels": { "nodes": [] }, "inverseRelations": { "nodes": [] },
          "comments": { "nodes": [
            { "createdAt": "2026-06-03T10:00:00.000Z", "body": "@symphony old ask" },
            { "createdAt": "2026-06-03T14:00:00.000Z", "body": "@symphony please also fix the MTU config" },
            { "createdAt": "2026-06-03T15:00:00.000Z", "body": "unrelated chatter, no token" }
          ] }
        }"#;
        let iss = norm_client("").normalize_issue(parse(raw));
        assert_eq!(
            iss.latest_summon_at,
            Some(Utc.with_ymd_and_hms(2026, 6, 3, 14, 0, 0).unwrap()),
            "LatestSummonAt should be the 14:00 summons"
        );
        assert_eq!(
            iss.latest_summon_body, "@symphony please also fix the MTU config",
            "LatestSummonBody should be the newest summons' body"
        );

        // No summons at all → empty body.
        let iss2 = norm_client("").normalize_issue(parse(
            r#"{"id":"u","identifier":"MT-12","title":"t","state":{"name":"Todo"}}"#,
        ));
        assert_eq!(
            iss2.latest_summon_body, "",
            "no summons → LatestSummonBody must be empty"
        );
    }

    // Mirrors Go TestNormalizeMilestone.
    #[test]
    fn normalize_milestone() {
        let c = norm_client("");
        let with_ms = c.normalize_issue(RawIssue {
            id: "1".into(),
            identifier: "MT-1".into(),
            project_milestone: Some(RawMilestone {
                id: "ms-uuid".into(),
                name: "v2.0".into(),
            }),
            ..RawIssue::default()
        });
        assert_eq!(with_ms.milestone_id, "ms-uuid");
        assert_eq!(with_ms.milestone_name, "v2.0");

        let none = c.normalize_issue(RawIssue {
            id: "2".into(),
            identifier: "MT-2".into(),
            ..RawIssue::default()
        });
        assert_eq!(none.milestone_id, "");
        assert_eq!(none.milestone_name, "");
    }

    // Mirrors Go TestNormalizeAssignee.
    #[test]
    fn normalize_assignee() {
        let c = norm_client("");
        let with_assignee = c.normalize_issue(RawIssue {
            id: "1".into(),
            identifier: "MT-1".into(),
            assignee: Some(RawAssignee {
                id: "u-uuid".into(),
                display_name: "David Johansen".into(),
            }),
            ..RawIssue::default()
        });
        assert_eq!(with_assignee.assignee_id, "u-uuid");
        assert_eq!(with_assignee.assignee_name, "David Johansen");

        let none = c.normalize_issue(RawIssue {
            id: "2".into(),
            identifier: "MT-2".into(),
            ..RawIssue::default()
        });
        assert_eq!(none.assignee_id, "");
        assert_eq!(none.assignee_name, "");
    }

    // Mirrors Go TestNormalizeLinkedPRs.
    #[test]
    fn normalize_linked_prs() {
        let raw = r#"{
            "id":"i1","identifier":"AIE-1","title":"t","state":{"name":"In Review"},
            "attachments":{"nodes":[
                {"sourceType":"github","metadata":{"url":"https://github.com/o/r/pull/100","status":"merged","mergedAt":"2026-06-03T17:58:28.000Z","updatedAt":"2026-06-03T17:58:29.000Z"}},
                {"sourceType":"github","metadata":{"url":"https://github.com/o/r/pull/101","updatedAt":"2026-06-04T10:00:00.000Z"}},
                {"sourceType":"github","metadata":{"url":"https://github.com/o/r/commit/deadbeef"}}
            ]},
            "comments":{"nodes":[]}
        }"#;
        let iss = norm_client("").normalize_issue(parse(raw));
        let prs = iss.linked_prs.as_deref().unwrap_or_default();
        assert_eq!(
            prs.len(),
            2,
            "LinkedPRs len should be 2 (commit attachment excluded)"
        );
        let pr100 = prs.iter().find(|p| p.number == 100).expect("PR 100");
        assert!(
            pr100.owner == "o" && pr100.repo == "r" && pr100.merged,
            "PR 100 = {pr100:?}"
        );
        let pr101 = prs.iter().find(|p| p.number == 101).expect("PR 101");
        assert!(!pr101.merged, "PR 101 should be merged=false");
    }

    // STUDIO-406: Linear sends `null` for nullable Strings, and Go's encoding/json decodes a JSON
    // null into a `string` field as the zero value "". A plain Rust `String` REJECTS it, which made
    // one attachment fail the whole page decode — silently disabling every project holding an
    // in-review issue with a PR attachment. Every plain-String field in the response structs must
    // therefore tolerate null, exactly like Go.
    #[test]
    fn normalize_tolerates_null_strings_like_go() {
        let raw = r#"{
          "id": "u", "identifier": "STUDIO-398", "title": "t", "state": { "name": "In Review" },
          "team": { "id": null },
          "labels": { "nodes": [ { "name": null } ] },
          "inverseRelations": { "nodes": [ { "type": null, "issue": { "id": null, "identifier": null, "state": { "name": null } } } ] },
          "attachments": { "nodes": [
            { "sourceType": null, "metadata": { "url": null, "status": null, "updatedAt": null, "mergedAt": null, "createdAt": null } },
            { "sourceType": "github", "metadata": { "url": "https://github.com/makewhatis/flux/pull/58", "status": null, "updatedAt": "2026-08-14T04:55:31.000Z", "mergedAt": null, "createdAt": null } }
          ] },
          "comments": { "nodes": [ { "createdAt": null, "body": null } ] }
        }"#;
        let iss = norm_client("").normalize_issue(
            serde_json::from_str(raw)
                .expect("a null-bearing payload must decode, as it does in Go"),
        );
        // The null-sourceType attachment is simply not a GitHub PR; the real one still registers.
        assert!(iss.linked_pr, "the github PR attachment must still be seen");
        let prs = iss.linked_prs.as_deref().unwrap_or_default();
        assert_eq!(prs.len(), 1, "exactly one linked PR");
        assert_eq!(prs[0].number, 58);
        assert!(!prs[0].merged, "null mergedAt/status → not merged");
        assert_eq!(
            iss.latest_pr_activity_at,
            Some(Utc.with_ymd_and_hms(2026, 8, 14, 4, 55, 31).unwrap())
        );
        // Null strings land as the Go zero value, not an error.
        assert_eq!(iss.team_id, "");
        assert_eq!(iss.labels.as_deref(), Some([String::new()].as_slice()));
        assert!(
            iss.blocked_by.is_none(),
            "a null relation type is not \"blocks\""
        );
    }

    // Mirrors Go TestNormalizeNoLinkedPR.
    #[test]
    fn normalize_no_linked_pr() {
        let raw = r#"{
          "id": "u", "identifier": "MT-1", "title": "t", "state": { "name": "Todo" },
          "labels": { "nodes": [] }, "inverseRelations": { "nodes": [] },
          "attachments": { "nodes": [
            { "sourceType": "github", "metadata": { "url": "https://github.com/o/r/commit/abc" } },
            { "sourceType": "linear", "metadata": { "url": "https://uploads.linear.app/x" } }
          ] },
          "comments": { "nodes": [] }
        }"#;
        let iss = norm_client("").normalize_issue(parse(raw));
        assert!(
            !iss.linked_pr && iss.latest_pr_activity_at.is_none(),
            "expected no linked PR"
        );
        assert!(
            iss.latest_summon_at.is_none(),
            "expected None LatestSummonAt"
        );
    }
}
