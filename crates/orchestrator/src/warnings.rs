//! warnings — parity port of Go `internal/orchestrator/warnings.go` (the two per-project advisory
//! producers surfaced on `GET /api/v1/projects`).
//!
//! Two producers with DIFFERENT trust domains, kept in separate maps so each refreshes independently
//! and merges at read ([`WarningsState::merged_for`]):
//!   * **slug** (INF-277) — a configured project slug that matches no Linear project (its dispatch
//!     query can never hit). Linear-gated: a transient `list_projects` failure PRESERVES the prior map.
//!   * **file** (INF-279) — a repo-relative `prompt_file` absent from its synced mirror (the run soft-
//!     falls-back to the inline prompt). A local git check, always trustworthy, so it is ALWAYS stored.
//!
//! The resolver runs OFF the control task (it makes a Linear call + git reads), so its state lives in
//! an [`Arc<WarningsState>`](WarningsState) the spawned resolver tasks share while the control task
//! reads it in `project_statuses` — the Rust stand-in for Go guarding `projectWarnings` /
//! `projectFileWarnings` under `warningsMu` with two generation counters. A stale-generation write is
//! dropped so a slower older pass never clobbers a newer reload's warnings.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rhapsody_tracker::Tracker;

use crate::effective::Effective;
use crate::orchestrator::Orchestrator;

/// Bounds the best-effort resolver's Linear call + git reads (Go `projectWarningTimeout`). It runs
/// off the control task, so this only caps how long the background pass lives; the daemon lifetime
/// ctx (`o.ctx`) still aborts it on shutdown.
const PROJECT_WARNING_TIMEOUT: Duration = Duration::from_secs(30);

/// How many CONSECUTIVE failed candidate fetches a project must accumulate before the failure is
/// surfaced as a warning (STUDIO-406). One failed tick is ordinary (a Linear blip, a timeout) and
/// self-heals; a sustained streak means no ticket in that project can dispatch at all, which is
/// indistinguishable from "nothing is queued" unless it is surfaced. Three ticks is long enough to
/// ride out a transient outage and short enough that a human sees the real thing within minutes.
pub(crate) const FETCH_FAILURE_WARN_AFTER: u32 = 3;

/// The per-project snapshot the warning resolver works from, captured on the control task (from
/// `eff.projects`) BEFORE the async resolver runs, so the resolver never reads loop-owned state
/// off-loop. Mirrors Go `projectWarnInput`.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProjectWarnInput {
    pub group: String,
    pub slug: String,
    pub name: String,
    /// `repo` + `prompt_file` back the missing-prompt-file producer (INF-279). `prompt_file` is set
    /// ONLY when the project's effective prompt_file is repo-relative + non-empty; absolute/`~` host
    /// paths hard-fail at run time and get no pre-flight flag, so they stay empty.
    pub repo: String,
    pub prompt_file: String,
}

/// A future resolving to `(exists, checked)` for one mirror check.
type CheckFut = Pin<Box<dyn Future<Output = (bool, bool)> + Send>>;

/// Reports whether a repo-relative `prompt_file` exists at the default-branch tip of a repo's synced
/// mirror. `checked=false` means "cannot verify" (mirror not synced yet / infra error) and never
/// produces a flag. It is captured from the workspace `Manager` on the control task and passed into
/// the off-loop resolver, so the producers never read loop-owned `eff` state off-loop (INF-279).
/// Mirrors Go `promptFileChecker` (async here because `Manager::prompt_file_in_repo` is async).
pub(crate) type PromptFileChecker = Box<dyn Fn(String, String) -> CheckFut + Send + Sync>;

/// The off-loop-shared warning state (Go's `warningsMu` + `projectWarnings` / `projectFileWarnings` +
/// `warningsGen` / `fileWarningsGen`). Held behind [`Arc`] so the resolver tasks store into it while
/// the control task reads it.
#[derive(Default)]
pub(crate) struct WarningsState {
    /// Both maps under one lock (Go's single `warningsMu` guarding both). `slug` is INF-277, `file`
    /// is INF-279.
    maps: RwLock<WarningMaps>,
    /// The slug-map generation (Go `warningsGen`): a pass writes only if its captured gen is still the
    /// latest, so a slower older pass never clobbers a newer one.
    slug_gen: AtomicU64,
    /// The file-map generation (Go `fileWarningsGen`) — SEPARATE so a file-only worker-exit refresh
    /// never invalidates a concurrent reload's in-flight slug store, and vice versa.
    file_gen: AtomicU64,
}

#[derive(Default)]
struct WarningMaps {
    slug: HashMap<String, Vec<String>>,
    file: HashMap<String, Vec<String>>,
    /// Producer 3 (STUDIO-406) — the consecutive-candidate-fetch-failure streak per project group.
    /// Unlike the other two this is NOT recomputed by a background pass: the poll loop records each
    /// tick's verdict directly, so it carries no generation guard (there is no slower-older-pass to
    /// lose a race to) and is keyed/cleared per group rather than replaced wholesale.
    fetch: HashMap<String, FetchFailure>,
}

/// One project's live candidate-fetch failure streak.
#[derive(Debug, Clone)]
struct FetchFailure {
    /// Consecutive failed fetches; reset to 0 by the first success.
    streak: u32,
    /// The project slug whose fetch is failing (a multi-slug project names the slug that broke).
    slug: String,
    /// The most recent error, quoted into the warning so the cause is visible without the log.
    err: String,
}

impl WarningsState {
    /// Advances + returns the new slug generation (Go `warningsGen.Add(1)`), claimed before a pass.
    pub(crate) fn bump_slug_gen(&self) -> u64 {
        self.slug_gen.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Advances + returns the new file generation (Go `fileWarningsGen.Add(1)`).
    pub(crate) fn bump_file_gen(&self) -> u64 {
        self.file_gen.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Replaces the slug-warning map ONLY if `gen` is still the latest slug generation, so a slower
    /// OLDER pass finishing after a newer reload cannot overwrite it. Mirrors Go `storeSlugWarnings`.
    fn store_slug(&self, generation: u64, w: HashMap<String, Vec<String>>) {
        let mut m = self.maps.write().unwrap_or_else(|e| e.into_inner());
        if generation == self.slug_gen.load(Ordering::SeqCst) {
            m.slug = w;
        }
    }

    /// Replaces the prompt-file-warning map with the same stale-generation guard against the SEPARATE
    /// file generation. Mirrors Go `storePromptFileWarnings`.
    fn store_file(&self, generation: u64, w: HashMap<String, Vec<String>>) {
        let mut m = self.maps.write().unwrap_or_else(|e| e.into_inner());
        if generation == self.file_gen.load(Ordering::SeqCst) {
            m.file = w;
        }
    }

    /// The currently-stored prompt-file warnings for a group (empty when none). `compute_prompt_file`
    /// uses it to carry a flag forward when a check is inconclusive. Mirrors Go `priorFileWarning`.
    fn prior_file(&self, group: &str) -> Vec<String> {
        self.maps
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .file
            .get(group)
            .cloned()
            .unwrap_or_default()
    }

    /// Records one FAILED candidate fetch for a project group, advancing its streak. Once the streak
    /// reaches [`FETCH_FAILURE_WARN_AFTER`] the project carries a warning (see [`merged_for`]).
    /// Called from the poll loop on every failing tick. STUDIO-406.
    pub(crate) fn record_fetch_failure(&self, group: &str, slug: &str, err: &str) {
        let mut m = self.maps.write().unwrap_or_else(|e| e.into_inner());
        let e = m.fetch.entry(group.to_string()).or_insert(FetchFailure {
            streak: 0,
            slug: slug.to_string(),
            err: String::new(),
        });
        e.streak = e.streak.saturating_add(1);
        e.slug = slug.to_string();
        e.err = err.to_string();
    }

    /// Clears a project group's fetch-failure streak — called on every SUCCESSFUL candidate fetch, so
    /// a recovered project drops its warning on the next tick. STUDIO-406.
    pub(crate) fn clear_fetch_failures(&self, group: &str) {
        let mut m = self.maps.write().unwrap_or_else(|e| e.into_inner());
        m.fetch.remove(group);
    }

    /// The merged warnings for a group (empty when none): missing-prompt-file flags first, then the
    /// unmatched-slug advisories, then the fetch-failure warning (STUDIO-406). A fresh slice so
    /// callers never alias the stored maps. Mirrors Go `projectWarningsFor`, plus the fetch producer.
    pub(crate) fn merged_for(&self, group: &str) -> Vec<String> {
        let m = self.maps.read().unwrap_or_else(|e| e.into_inner());
        let file = m.file.get(group);
        let slug = m.slug.get(group);
        let mut out = Vec::new();
        if let Some(f) = file {
            out.extend_from_slice(f);
        }
        if let Some(s) = slug {
            out.extend_from_slice(s);
        }
        // Appended LAST so the pre-existing producers keep their golden ordering.
        if let Some(f) = m.fetch.get(group)
            && f.streak >= FETCH_FAILURE_WARN_AFTER
        {
            out.push(format!(
                "candidate fetch for slug {:?} has failed {} times in a row — no issue in this project can dispatch (last error: {})",
                f.slug, f.streak, f.err
            ));
        }
        out
    }

    /// Producer 2 (INF-279): a repo-relative `prompt_file` absent from its synced mirror. A LOCAL,
    /// always-trustworthy git check, so it returns its map unconditionally (no trust bool) and the
    /// caller ALWAYS stores it. Mirrors Go `computePromptFileWarnings`.
    async fn compute_prompt_file(
        &self,
        inputs: &[ProjectWarnInput],
        checker: Option<&PromptFileChecker>,
    ) -> HashMap<String, Vec<String>> {
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        let Some(check) = checker else {
            return out;
        };
        // Warnings are keyed by the shared project GROUP, but a multi-slug project fans into one input
        // per slug — all sharing the same group/repo/prompt_file. Process each group at most once so an
        // N-slug project gets ONE flag, not N identical ones.
        let mut done: HashSet<String> = HashSet::new();
        for in_ in inputs {
            if in_.prompt_file.is_empty() || in_.repo.is_empty() || done.contains(&in_.group) {
                continue;
            }
            done.insert(in_.group.clone());
            let (exists, checked) = check(in_.repo.clone(), in_.prompt_file.clone()).await;
            if !checked {
                // Inconclusive (mirror not synced yet / a transient git error): the result map REPLACES
                // the whole stored map, so omitting this group would DROP a flag a prior conclusive pass
                // already surfaced. Carry the prior flag forward so "cannot verify" preserves, never
                // clears.
                let prior = self.prior_file(&in_.group);
                if !prior.is_empty() {
                    out.insert(in_.group.clone(), prior);
                }
            } else if !exists {
                let msg = format!(
                    "prompt_file {:?} not found or empty in {} — runs use the inline prompt",
                    in_.prompt_file,
                    repo_label(&in_.repo)
                );
                out.entry(in_.group.clone()).or_default().push(msg.clone());
                tracing::debug!(repo = %in_.repo, project_name = %in_.name, prompt_file = %in_.prompt_file, "{msg}");
            }
            // exists && checked: file is present → emit nothing (a fixed file clears its prior flag).
        }
        out
    }
}

/// Producer 1 (INF-277): a configured slug that matches no Linear project. Resolved against
/// `list_projects`. The bool return is "resolution was trustworthy": false on a nil tracker or a
/// `list_projects` error, so the caller PRESERVES the prior slug warnings instead of letting a
/// transient Linear failure wipe a real advisory. Mirrors Go `computeSlugWarnings`.
async fn compute_slug(
    inputs: &[ProjectWarnInput],
    tr: Option<Arc<dyn Tracker>>,
) -> (HashMap<String, Vec<String>>, bool) {
    let Some(tr) = tr else {
        return (HashMap::new(), false);
    };
    let projs = match tr.list_projects().await {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(err = %e, "project-warning resolution skipped: listing Linear projects failed");
            return (HashMap::new(), false);
        }
    };
    let known: HashSet<String> = projs.into_iter().map(|p| p.slug).collect();
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for in_ in inputs {
        if in_.slug.is_empty() || known.contains(&in_.slug) {
            continue;
        }
        let msg = format!(
            "project slug {:?} matches no Linear project — its agent will never dispatch",
            in_.slug
        );
        out.entry(in_.group.clone()).or_default().push(msg.clone());
        tracing::warn!(project_slug = %in_.slug, project_name = %in_.name, "{msg}");
    }
    (out, true)
}

/// Reports whether a `prompt_file` path resolves on the daemon HOST (absolute or `~`-prefixed) rather
/// than repo-relative — the single source of truth for the relative-vs-host split. Mirrors Go
/// `isHostPromptPath`.
fn is_host_prompt_path(p: &str) -> bool {
    let p = p.trim();
    std::path::Path::new(p).is_absolute() || p == "~" || p.starts_with("~/")
}

/// Renders a concise repo identifier for a warning message: the final path segment with a trailing
/// slash and any `.git` suffix stripped, falling back to the trimmed raw value. Mirrors Go `repoLabel`.
fn repo_label(repo_url: &str) -> String {
    let trimmed = repo_url.trim();
    let s = trimmed.strip_suffix('/').unwrap_or(trimmed);
    let s = s.strip_suffix(".git").unwrap_or(s);
    let seg = match s.rfind(['/', ':']) {
        Some(i) if i + 1 < s.len() => &s[i + 1..],
        _ => s,
    };
    if seg.is_empty() {
        return trimmed.to_string();
    }
    seg.to_string()
}

/// Reports whether any project carries a repo-relative prompt_file worth a mirror check — lets the
/// worker-exit path skip spawning a resolver when nothing can flag. Mirrors Go `hasPromptFileInputs`.
fn has_prompt_file_inputs(inputs: &[ProjectWarnInput]) -> bool {
    inputs
        .iter()
        .any(|in_| !in_.prompt_file.is_empty() && !in_.repo.is_empty())
}

/// Captures the warning resolver's inputs from an effective config, on the control task (reload)
/// before handing the slice to the off-loop resolver. Only repo-relative prompt_files get the
/// pre-flight flag (absolute/`~` host paths hard-fail a run, so they are not soft-fallback
/// candidates). Mirrors Go `projectWarnInputs`.
pub(crate) fn project_warn_inputs(eff: &Effective) -> Vec<ProjectWarnInput> {
    let mut inputs = Vec::with_capacity(eff.projects.len());
    for p in &eff.projects {
        let mut in_ = ProjectWarnInput {
            group: p.group.clone(),
            slug: p.slug.clone(),
            name: p.name.clone(),
            repo: p.repo.clone(),
            prompt_file: String::new(),
        };
        let pf = p.prompt_file.trim();
        if !pf.is_empty() && !is_host_prompt_path(pf) {
            in_.prompt_file = pf.to_string();
        }
        inputs.push(in_);
    }
    inputs
}

impl Orchestrator {
    /// The merged per-group warnings (file flags then slug advisories), read on the control task in
    /// `project_statuses`. Mirrors Go `projectWarningsFor`. (The compute / store producers are the free
    /// `compute_slug` + [`WarningsState`] methods the off-loop resolver + the tests call directly — a
    /// spawned resolver task cannot hold `&self`, so unlike Go they are not `Orchestrator` methods.)
    pub(crate) fn project_warnings_for(&self, group: &str) -> Vec<String> {
        self.warnings.merged_for(group)
    }

    /// Binds the workspace `Manager`'s read-only mirror check for the off-loop resolver. Mirrors Go
    /// `promptFileCheckerFor` (the Rust `Effective` always has a workspace, so — unlike Go's
    /// nil-workspace guard — this is always `Some`).
    pub(crate) fn prompt_file_checker_for(&self, eff: &Effective) -> Option<PromptFileChecker> {
        let ws = Arc::clone(&eff.workspace);
        Some(Box::new(move |repo, rel| {
            let ws = Arc::clone(&ws);
            Box::pin(async move { ws.prompt_file_in_repo(&repo, &rel).await })
        }))
    }

    /// Recomputes BOTH per-project warning producers OFF the control task (called from reload). The
    /// slug producer makes a Linear call and the prompt-file producer shells out to git, which must
    /// never block the control loop — so reload kicks this off and returns. Gated on a live daemon
    /// (`o.ctx` is set only by Run, so the direct-reload unit-test path skips it). Mirrors Go
    /// `refreshProjectWarnings`.
    pub(crate) fn refresh_project_warnings(
        &self,
        inputs: Vec<ProjectWarnInput>,
        tr: Option<Arc<dyn Tracker>>,
        checker: Option<PromptFileChecker>,
    ) {
        let Some(ctx) = self.ctx.clone() else {
            return;
        };
        if inputs.is_empty() {
            return;
        }
        let gen_file = self.warnings.bump_file_gen();
        let gen_slug = self.warnings.bump_slug_gen();
        let guard = self.wg.add();
        let warnings = Arc::clone(&self.warnings);
        let mut ctx = ctx;
        tokio::spawn(async move {
            let _guard = guard;
            let work = async {
                // File warnings are always trustworthy → always store. Slug warnings preserve the prior
                // map on a Linear failure (ok=false) so a transient outage never hides a real advisory.
                let file = warnings
                    .compute_prompt_file(&inputs, checker.as_ref())
                    .await;
                warnings.store_file(gen_file, file);
                let (slug, ok) = compute_slug(&inputs, tr).await;
                if ok {
                    warnings.store_slug(gen_slug, slug);
                }
            };
            tokio::select! {
                _ = tokio::time::timeout(PROJECT_WARNING_TIMEOUT, work) => {}
                _ = ctx.cancelled() => {}
            }
        });
    }

    /// Recomputes ONLY the prompt-file producer off the control task. The worker-exit path uses it so
    /// the missing-prompt-file flag surfaces after the first mirror sync WITHOUT re-running the Linear
    /// slug producer. A no-op when nothing carries a repo-relative prompt_file. Mirrors Go
    /// `refreshPromptFileWarnings`.
    pub(crate) fn refresh_prompt_file_warnings(
        &self,
        inputs: Vec<ProjectWarnInput>,
        checker: Option<PromptFileChecker>,
    ) {
        let Some(ctx) = self.ctx.clone() else {
            return;
        };
        if checker.is_none() || !has_prompt_file_inputs(&inputs) {
            return;
        }
        let generation = self.warnings.bump_file_gen();
        let guard = self.wg.add();
        let warnings = Arc::clone(&self.warnings);
        let mut ctx = ctx;
        tokio::spawn(async move {
            let _guard = guard;
            let work = async {
                let file = warnings
                    .compute_prompt_file(&inputs, checker.as_ref())
                    .await;
                warnings.store_file(generation, file);
            };
            tokio::select! {
                _ = tokio::time::timeout(PROJECT_WARNING_TIMEOUT, work) => {}
                _ = ctx.cancelled() => {}
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_loop::{CancelWait, WaitGroup};
    use crate::snapshot::ProjectStatus;
    use crate::testsupport::empty_resolved_project;
    use rhapsody_core::Project;
    use rhapsody_tracker::TrackerError;
    use rhapsody_tracker::fake::Fake;

    fn project(slug: &str, name: &str) -> Project {
        Project {
            id: String::new(),
            name: name.to_string(),
            slug: slug.to_string(),
            team: String::new(),
            color: String::new(),
        }
    }

    fn warn_input(group: &str, slug: &str, name: &str) -> ProjectWarnInput {
        ProjectWarnInput {
            group: group.to_string(),
            slug: slug.to_string(),
            name: name.to_string(),
            ..Default::default()
        }
    }

    fn warn_input_file(group: &str, name: &str, repo: &str, prompt_file: &str) -> ProjectWarnInput {
        ProjectWarnInput {
            group: group.to_string(),
            name: name.to_string(),
            repo: repo.to_string(),
            prompt_file: prompt_file.to_string(),
            ..Default::default()
        }
    }

    /// Wraps a synchronous `repo -> (exists, checked)` verdict into an async [`PromptFileChecker`].
    fn checker_from(f: impl Fn(&str) -> (bool, bool) + Send + Sync + 'static) -> PromptFileChecker {
        Box::new(move |repo, _rel| {
            let verdict = f(&repo);
            let fut: CheckFut = Box::pin(async move { verdict });
            fut
        })
    }

    /// Waits out the off-loop resolver (Go `waitWG`), failing the test on a 5s stall.
    async fn wait_wg(wg: &WaitGroup) {
        tokio::time::timeout(Duration::from_secs(5), wg.wait())
            .await
            .expect("timed out waiting for background warning resolver");
    }

    // Mirrors Go `TestComputeProjectWarnings_UnmatchedSlug`.
    #[tokio::test]
    async fn compute_project_warnings_unmatched_slug() {
        let mut f = Fake::new();
        f.projects = vec![project("872639248532", "Symphony App")];
        let tr: Arc<dyn Tracker> = Arc::new(f);
        let inputs = vec![
            warn_input("872639248532", "872639248532", "Symphony App"),
            warn_input(
                "symphony-app-872639248532",
                "symphony-app-872639248532",
                "Bad",
            ),
        ];
        let (got, ok) = compute_slug(&inputs, Some(tr)).await;
        assert!(ok, "a successful list_projects should report ok=true");
        assert!(
            !got.contains_key("872639248532"),
            "matched slug → no warning"
        );
        let w = &got["symphony-app-872639248532"];
        assert_eq!(w.len(), 1);
        assert_eq!(
            w[0],
            "project slug \"symphony-app-872639248532\" matches no Linear project — its agent will never dispatch"
        );
    }

    // Mirrors Go `TestComputeProjectWarnings_LinearErrorSkipsQuietly`.
    #[tokio::test]
    async fn compute_project_warnings_linear_error_skips_quietly() {
        let mut f = Fake::new();
        f.projects_err = Some(TrackerError::Other("boom".to_string()));
        let tr: Arc<dyn Tracker> = Arc::new(f);
        let (got, ok) = compute_slug(&[warn_input("g", "anything", "n")], Some(tr)).await;
        assert!(!ok, "a Linear failure must report ok=false");
        assert!(got.is_empty(), "Linear failure → no warnings");
    }

    // Mirrors Go `TestComputeProjectWarnings_NilTracker`.
    #[tokio::test]
    async fn compute_project_warnings_nil_tracker() {
        let (got, ok) = compute_slug(&[warn_input("g", "s", "")], None).await;
        assert!(!ok, "a nil tracker must report ok=false");
        assert!(got.is_empty());
    }

    // Mirrors Go `TestComputePromptFileWarnings`.
    #[tokio::test]
    async fn compute_prompt_file_warnings() {
        let o = Orchestrator::new("WORKFLOW.md");
        let inputs = vec![
            warn_input_file(
                "absent",
                "Absent",
                "git@github.com:org/absent.git",
                ".symphony/PROMPT.md",
            ),
            warn_input_file(
                "present",
                "Present",
                "git@github.com:org/present.git",
                ".symphony/PROMPT.md",
            ),
            warn_input_file(
                "unsynced",
                "Unsynced",
                "git@github.com:org/unsynced.git",
                ".symphony/PROMPT.md",
            ),
            warn_input_file(
                "noprompt",
                "NoPrompt",
                "git@github.com:org/noprompt.git",
                "",
            ),
        ];
        let check = checker_from(|repo| {
            if repo.contains("absent") {
                (false, true)
            } else if repo.contains("present") {
                (true, true)
            } else {
                (false, false)
            }
        });
        let got = o.warnings.compute_prompt_file(&inputs, Some(&check)).await;
        assert_eq!(got["absent"].len(), 1);
        assert_eq!(
            got["absent"][0],
            "prompt_file \".symphony/PROMPT.md\" not found or empty in absent — runs use the inline prompt"
        );
        assert!(!got.contains_key("present"), "present → no warning");
        assert!(!got.contains_key("unsynced"), "unverifiable → no warning");
        assert!(
            !got.contains_key("noprompt"),
            "no relative prompt_file → not checked"
        );
        // A nil checker produces nothing.
        assert!(
            o.warnings
                .compute_prompt_file(&inputs, None)
                .await
                .is_empty()
        );
    }

    // Mirrors Go `TestComputePromptFileWarnings_MultiSlugDedupes`.
    #[tokio::test]
    async fn compute_prompt_file_warnings_multi_slug_dedupes() {
        let o = Orchestrator::new("WORKFLOW.md");
        let inputs = vec![
            warn_input_file(
                "g",
                "P",
                "git@github.com:org/repo.git",
                ".symphony/PROMPT.md",
            ),
            warn_input_file(
                "g",
                "P",
                "git@github.com:org/repo.git",
                ".symphony/PROMPT.md",
            ),
            warn_input_file(
                "g",
                "P",
                "git@github.com:org/repo.git",
                ".symphony/PROMPT.md",
            ),
        ];
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        let check = checker_from(move |_repo| {
            calls2.fetch_add(1, Ordering::SeqCst);
            (false, true)
        });
        let got = o.warnings.compute_prompt_file(&inputs, Some(&check)).await;
        assert_eq!(
            got["g"].len(),
            1,
            "a multi-slug project must produce exactly one flag"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the mirror check should run once per group"
        );
    }

    // Mirrors Go `TestComputePromptFileWarnings_InconclusivePreservesPriorFlag`.
    #[tokio::test]
    async fn compute_prompt_file_warnings_inconclusive_preserves_prior_flag() {
        let o = Orchestrator::new("WORKFLOW.md");
        // A prior conclusive pass recorded a missing-prompt flag for "g".
        o.warnings.store_file(
            o.warnings.bump_file_gen(),
            HashMap::from([("g".to_string(), vec!["prior flag".to_string()])]),
        );
        let inputs = vec![warn_input_file(
            "g",
            "P",
            "git@github.com:org/repo.git",
            ".symphony/PROMPT.md",
        )];
        // Inconclusive now → the prior flag must be carried forward.
        let inconclusive = checker_from(|_| (false, false));
        let got = o
            .warnings
            .compute_prompt_file(&inputs, Some(&inconclusive))
            .await;
        assert_eq!(got["g"], vec!["prior flag".to_string()]);
        // File now present (conclusive) → the flag clears.
        let present = checker_from(|_| (true, true));
        assert!(
            !o.warnings
                .compute_prompt_file(&inputs, Some(&present))
                .await
                .contains_key("g")
        );
    }

    // Mirrors Go `TestRefreshProjectWarnings_FileWarningSurvivesAndClearsAcrossLinearOutage`.
    #[tokio::test]
    async fn refresh_file_warning_survives_and_clears_across_linear_outage() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.ctx = Some(CancelWait::default());
        let inputs = vec![warn_input_file(
            "g",
            "P",
            "git@github.com:org/repo.git",
            ".symphony/PROMPT.md",
        )];
        let mut tr_down = Fake::new();
        tr_down.projects_err = Some(TrackerError::Other("boom".to_string()));
        let tr_down: Arc<dyn Tracker> = Arc::new(tr_down);

        // Pass 1: Linear DOWN, file ABSENT → the file flag must surface despite the outage.
        let absent = checker_from(|_| (false, true));
        o.refresh_project_warnings(inputs.clone(), Some(Arc::clone(&tr_down)), Some(absent));
        wait_wg(&o.wg).await;
        assert_eq!(
            o.project_warnings_for("g").len(),
            1,
            "file flag must surface while Linear is down"
        );

        // Pass 2: Linear STILL DOWN, file now PRESENT → the stale flag must clear.
        let present = checker_from(|_| (true, true));
        o.refresh_project_warnings(inputs, Some(tr_down), Some(present));
        wait_wg(&o.wg).await;
        assert!(
            o.project_warnings_for("g").is_empty(),
            "a fixed prompt_file must clear its flag"
        );
    }

    // Mirrors Go `TestRefreshPromptFileWarnings_FileOnlyPathStoresAndSkips`.
    #[tokio::test]
    async fn refresh_prompt_file_only_path_stores_and_skips() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.ctx = Some(CancelWait::default());
        // Seed a prior slug warning the file-only refresh must NOT disturb.
        o.warnings.store_slug(
            o.warnings.bump_slug_gen(),
            HashMap::from([("g".to_string(), vec!["slug advisory".to_string()])]),
        );
        let absent = checker_from(|_| (false, true));
        o.refresh_prompt_file_warnings(
            vec![warn_input_file(
                "g",
                "P",
                "git@github.com:org/repo.git",
                ".symphony/PROMPT.md",
            )],
            Some(absent),
        );
        wait_wg(&o.wg).await;
        assert_eq!(
            o.project_warnings_for("g").len(),
            2,
            "file-only refresh merges file + prior slug"
        );

        // No relative prompt_file → no resolver spawned, slug warnings untouched.
        let absent2 = checker_from(|_| (false, true));
        let mut skip = warn_input("g", "g", "");
        skip.repo = "r".to_string();
        o.refresh_prompt_file_warnings(vec![skip], Some(absent2));
        wait_wg(&o.wg).await;
        assert_eq!(
            o.project_warnings_for("g").len(),
            2,
            "a no-prompt-file refresh must be a no-op"
        );
    }

    // Mirrors Go `TestProjectWarnInputs_OnlyRelativePromptFiles`.
    #[test]
    fn project_warn_inputs_only_relative_prompt_files() {
        let tr: Arc<dyn Tracker> = Arc::new(Fake::new());
        let mut eff = crate::testsupport::empty_effective(Arc::clone(&tr));
        let mk = |group: &str, repo: &str, pf: &str| {
            let mut p = empty_resolved_project(group, Arc::clone(&tr));
            p.repo = repo.to_string();
            p.prompt_file = pf.to_string();
            p
        };
        eff.projects = vec![
            mk("rel", "r1", ".symphony/PROMPT.md"),
            mk("abs", "r2", "/etc/prompt.md"),
            mk("home", "r3", "~/prompt.md"),
            mk("none", "r4", ""),
        ];
        let by_group: HashMap<String, ProjectWarnInput> = project_warn_inputs(&eff)
            .into_iter()
            .map(|i| (i.group.clone(), i))
            .collect();
        assert_eq!(by_group["rel"].prompt_file, ".symphony/PROMPT.md");
        assert_eq!(
            by_group["abs"].prompt_file, "",
            "absolute prompt_file must not be flagged"
        );
        assert_eq!(
            by_group["home"].prompt_file, "",
            "~ prompt_file must not be flagged"
        );
        assert_eq!(
            by_group["none"].prompt_file, "",
            "absent prompt_file must stay empty"
        );
    }

    // Mirrors Go `TestRepoLabel`.
    #[test]
    fn repo_label_forms() {
        let cases = [
            ("git@github.com:org/repo.git", "repo"),
            ("https://github.com/org/repo.git", "repo"),
            ("https://github.com/org/repo", "repo"),
            ("https://github.com/org/repo/", "repo"),
            ("repo", "repo"),
            ("", ""),
        ];
        for (input, want) in cases {
            assert_eq!(repo_label(input), want, "repo_label({input:?})");
        }
    }

    // Mirrors Go `TestRefreshProjectWarnings_LinearFailurePreservesPrior`.
    #[tokio::test]
    async fn refresh_linear_failure_preserves_prior() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.ctx = Some(CancelWait::default());
        let inputs = vec![warn_input("bad", "bad", "Bad")];

        // First pass: Linear up, slug unmatched → warning recorded.
        let mut tr_ok = Fake::new();
        tr_ok.projects = vec![project("good", "")];
        o.refresh_project_warnings(inputs.clone(), Some(Arc::new(tr_ok)), None);
        wait_wg(&o.wg).await;
        assert_eq!(o.project_warnings_for("bad").len(), 1);

        // Second pass: Linear down → the prior warning must survive.
        let mut tr_err = Fake::new();
        tr_err.projects_err = Some(TrackerError::Other("boom".to_string()));
        o.refresh_project_warnings(inputs, Some(Arc::new(tr_err)), None);
        wait_wg(&o.wg).await;
        assert_eq!(
            o.project_warnings_for("bad").len(),
            1,
            "a transient Linear failure must preserve the prior warning"
        );
    }

    // Mirrors Go `TestRefreshProjectWarnings_AsyncStoreAndSurface`.
    #[tokio::test]
    async fn refresh_async_store_and_surface() {
        let tr: Arc<dyn Tracker> = Arc::new(Fake::new());
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.ctx = Some(CancelWait::default());
        let mut tr_ok = Fake::new();
        tr_ok.projects = vec![project("good", "Good")];
        let mut eff = crate::testsupport::empty_effective(Arc::clone(&tr));
        let mut good = empty_resolved_project("good", Arc::clone(&tr));
        good.name = "Good".to_string();
        let mut bad = empty_resolved_project("bad", Arc::clone(&tr));
        bad.name = "Bad".to_string();
        eff.projects = vec![good, bad];
        let inputs = project_warn_inputs(&eff);
        o.eff = Some(eff);

        o.refresh_project_warnings(inputs, Some(Arc::new(tr_ok)), None);
        wait_wg(&o.wg).await;

        assert!(
            o.project_warnings_for("good").is_empty(),
            "matched project → no warnings"
        );
        assert_eq!(
            o.project_warnings_for("bad").len(),
            1,
            "unmatched project → one warning"
        );

        let statuses = o.project_statuses();
        let by_group: HashMap<String, ProjectStatus> =
            statuses.into_iter().map(|s| (s.slug.clone(), s)).collect();
        assert!(
            by_group["good"].warnings.is_empty(),
            "good status → no warnings"
        );
        assert_eq!(by_group["bad"].warnings.len(), 1, "bad status → 1 warning");
    }

    // Mirrors Go `TestRefreshProjectWarnings_SkippedWithoutLiveCtx`.
    #[tokio::test]
    async fn refresh_skipped_without_live_ctx() {
        let o = Orchestrator::new("WORKFLOW.md"); // o.ctx is None (direct-reload path)
        let mut tr = Fake::new();
        tr.projects = vec![project("good", "")];
        o.refresh_project_warnings(vec![warn_input("bad", "bad", "")], Some(Arc::new(tr)), None);
        wait_wg(&o.wg).await;
        assert!(
            o.project_warnings_for("bad").is_empty(),
            "resolver skipped without a live ctx"
        );
    }

    // STUDIO-406: a project whose candidate fetch keeps failing is invisible — no ticket in it can
    // dispatch, yet nothing surfaces outside an ERROR log line. Repeated failure must reach the
    // per-project warnings the dashboard already renders.
    #[test]
    fn repeated_fetch_failure_surfaces_as_a_project_warning() {
        let o = Orchestrator::new("WORKFLOW.md");
        let err = "linear_unknown_payload: decode data: invalid type: null, expected a string";

        // Below the threshold a blip stays quiet — a single failed tick is normal.
        for _ in 1..FETCH_FAILURE_WARN_AFTER {
            o.warnings.record_fetch_failure("g", "f01292d2f808", err);
            assert!(
                o.project_warnings_for("g").is_empty(),
                "a transient failure must not raise a warning"
            );
        }

        // Crossing it surfaces one warning naming the slug, the streak, and the error.
        o.warnings.record_fetch_failure("g", "f01292d2f808", err);
        let w = o.project_warnings_for("g");
        assert_eq!(w.len(), 1, "expected exactly one fetch warning: {w:?}");
        assert!(w[0].contains("f01292d2f808"), "names the slug: {}", w[0]);
        assert!(
            w[0].contains(&FETCH_FAILURE_WARN_AFTER.to_string()),
            "reports the streak: {}",
            w[0]
        );
        assert!(w[0].contains("invalid type: null"), "quotes it: {}", w[0]);

        // The streak keeps counting up, still as ONE warning (not one per tick).
        o.warnings.record_fetch_failure("g", "f01292d2f808", err);
        let w = o.project_warnings_for("g");
        assert_eq!(w.len(), 1, "the warning must not accumulate: {w:?}");
        assert!(
            w[0].contains(&(FETCH_FAILURE_WARN_AFTER + 1).to_string()),
            "the streak advances: {}",
            w[0]
        );

        // A successful fetch clears it immediately.
        o.warnings.clear_fetch_failures("g");
        assert!(
            o.project_warnings_for("g").is_empty(),
            "a recovered project must clear its warning"
        );
    }

    // The fetch producer must be independent of the other two: it neither clobbers them nor is
    // clobbered by a reload's slug/file store.
    #[test]
    fn fetch_warnings_merge_with_the_other_producers() {
        let o = Orchestrator::new("WORKFLOW.md");
        o.warnings.store_slug(
            o.warnings.bump_slug_gen(),
            HashMap::from([("g".to_string(), vec!["slug advisory".to_string()])]),
        );
        for _ in 0..FETCH_FAILURE_WARN_AFTER {
            o.warnings.record_fetch_failure("g", "s", "boom");
        }
        assert_eq!(
            o.project_warnings_for("g").len(),
            2,
            "slug advisory + fetch warning"
        );
        // A later reload re-storing slug warnings must not drop the fetch warning.
        o.warnings.store_slug(
            o.warnings.bump_slug_gen(),
            HashMap::from([("g".to_string(), vec!["slug advisory".to_string()])]),
        );
        assert_eq!(
            o.project_warnings_for("g").len(),
            2,
            "fetch warning survives"
        );
    }

    // Two projects fail independently: clearing one must not clear the other.
    #[test]
    fn fetch_failures_are_tracked_per_project() {
        let o = Orchestrator::new("WORKFLOW.md");
        for _ in 0..FETCH_FAILURE_WARN_AFTER {
            o.warnings
                .record_fetch_failure("flux", "f01292d2f808", "boom");
            o.warnings
                .record_fetch_failure("tally", "aabbccddeeff", "boom");
        }
        assert_eq!(o.project_warnings_for("flux").len(), 1);
        assert_eq!(o.project_warnings_for("tally").len(), 1);
        o.warnings.clear_fetch_failures("flux");
        assert!(o.project_warnings_for("flux").is_empty());
        assert_eq!(
            o.project_warnings_for("tally").len(),
            1,
            "an unrelated project's warning must be untouched"
        );
    }

    // Mirrors Go `TestStoreSlugWarnings_DropsStaleGeneration`.
    #[test]
    fn store_slug_warnings_drops_stale_generation() {
        let o = Orchestrator::new("WORKFLOW.md");
        // A newer reload has advanced the generation to 2.
        o.warnings.slug_gen.store(2, Ordering::SeqCst);

        // An OLDER pass (gen 1) finishing late must not write.
        o.warnings.store_slug(
            1,
            HashMap::from([("g".to_string(), vec!["stale".to_string()])]),
        );
        assert!(
            o.project_warnings_for("g").is_empty(),
            "stale-generation write should be dropped"
        );

        // The current pass (gen 2) writes.
        o.warnings.store_slug(
            2,
            HashMap::from([("g".to_string(), vec!["fresh".to_string()])]),
        );
        assert_eq!(o.project_warnings_for("g"), vec!["fresh".to_string()]);

        // The stale-generation guard applies to the prompt-file map too (file_gen is 0 here).
        o.warnings.store_file(
            1,
            HashMap::from([("g".to_string(), vec!["stale file".to_string()])]),
        );
        assert_eq!(o.project_warnings_for("g"), vec!["fresh".to_string()]);
    }
}
