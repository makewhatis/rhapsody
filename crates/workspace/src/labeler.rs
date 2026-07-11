//! The built-in post-run PR labeler (`labeler.go`, AIE-301).
//!
//! After a run, [`Manager::label_run_prs`] tags every PR in the run's Graphite stack with the
//! configured label, using the agent's inherited `gh` auth. It is best-effort cleanup: it
//! enumerates the stack from git (the local branches that are ancestors of HEAD but not of the
//! trunk), ensures the label exists once, and adds it to each discovered PR. EVERY git/gh failure is
//! logged-and-swallowed in Go; this crate elides the best-effort logging (a W1 decision — no
//! mirrored test asserts observable log output) and simply swallows, so a failure never affects the
//! run outcome, teardown, or dispatch.
//!
//! Go threads `ctx context.Context`; this crate models cancellation as drop-based async
//! cancellation (the W1 convention), so `label_run_prs` takes no ctx. Go's caller passes
//! `context.Background()` (a per-run terminate must not skip labeling), which makes the total
//! [`LABEL_TOTAL_TIMEOUT`] the sole bound — reproduced here by wrapping the whole pass in a
//! `tokio::time::timeout`, with each `gh` call layering its own shorter [`GH_CALL_TIMEOUT`].

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::Error;
use crate::Manager;

/// PR-label color used only when the labeler auto-creates the label.
const PR_LABEL_COLOR: &str = "8B5CF6";
/// PR-label description used only when the labeler auto-creates the label.
const PR_LABEL_DESCRIPTION: &str = "Authored by the Symphony agent";
/// Bounds each `gh` subprocess so a hung network call can never wedge run teardown.
const GH_CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Caps the whole labeling pass (git enumeration + every gh call); set to match AfterRun's 60s hook
/// timeout for consistent teardown. An overrun is partial; idempotent re-runs finish the set.
const LABEL_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);

impl Manager {
    /// Tags every PR in this run's Graphite stack with `label`, using the agent's inherited `gh`
    /// auth (best-effort post-run cleanup, AIE-301). It enumerates the stack from git (the local
    /// branches in `worktree_dir` that are ancestors of HEAD but not of the trunk), ensures the
    /// label exists once (an "already exists" error is expected and ignored), then adds the label to
    /// each discovered PR. Every git/gh failure is swallowed. A legacy (empty `repo_url`) workspace,
    /// an empty `worktree_dir`, or an empty `label` is a no-op.
    pub async fn label_run_prs(&self, repo_url: &str, worktree_dir: &str, label: &str) {
        let label = label.trim();
        if repo_url.is_empty() || worktree_dir.is_empty() || label.is_empty() {
            return; // legacy/unconfigured path: nothing to label
        }
        // Cap the whole pass (git enumeration AND every gh call); a hung tool can never wedge the
        // worker. The result is discarded — labeling is best-effort.
        let _ = tokio::time::timeout(
            LABEL_TOTAL_TIMEOUT,
            self.label_run_prs_inner(worktree_dir, label),
        )
        .await;
    }

    /// The bounded body of [`Self::label_run_prs`] (split out so the total timeout wraps it).
    async fn label_run_prs_inner(&self, worktree_dir: &str, label: &str) {
        // Resolve the trunk from the worktree itself (it shares the mirror's remote-tracking refs),
        // then enumerate the stack relative to it.
        let base = match self.default_branch(worktree_dir).await {
            Ok(b) => b,
            Err(_) => return, // could not resolve default branch; skip labeling
        };
        let branches = match self.stack_branches(worktree_dir, &base).await {
            Ok(b) => b,
            Err(_) => return, // could not enumerate stack branches; skip labeling
        };
        if branches.is_empty() {
            return; // no run-local branches (e.g. a run that opened no PR)
        }

        // Ensure the label exists (idempotent). An "already exists" error is the normal case after
        // the first run; we cannot distinguish it without parsing gh's message, so we log+ignore and
        // proceed — `gh pr edit --add-label` is what actually matters.
        let _ = self
            .gh(
                worktree_dir,
                &[
                    "label",
                    "create",
                    label,
                    "--description",
                    PR_LABEL_DESCRIPTION,
                    "--color",
                    PR_LABEL_COLOR,
                ],
            )
            .await;

        for br in &branches {
            let nums = match self.pr_numbers_for_branch(worktree_dir, br).await {
                Ok(n) => n,
                Err(_) => continue, // gh pr list failed; skip this branch
            };
            for n in nums {
                let (_out, err) = self
                    .gh(worktree_dir, &["pr", "edit", &n, "--add-label", label])
                    .await;
                if err.is_some() {
                    continue; // gh pr edit --add-label failed; skip
                }
            }
        }
    }

    /// Returns the local branches in `worktree_dir` that form THIS run's stack: those reachable from
    /// HEAD (ancestors of the current tip) but not already merged into the trunk (`origin/<base>`).
    /// git ANDs the `--merged`/`--no-merged` filters, so this is the stack in one call. The base
    /// branch is also dropped defensively. Branch names come back clean via `--format`.
    pub(crate) async fn stack_branches(
        &self,
        worktree_dir: &str,
        base: &str,
    ) -> Result<Vec<String>, Error> {
        let origin_base = format!("origin/{base}");
        let (out, err) = self
            .git(
                worktree_dir,
                &[
                    "branch",
                    "--format=%(refname:short)",
                    "--merged",
                    "HEAD",
                    "--no-merged",
                    &origin_base,
                ],
            )
            .await;
        if let Some(err) = err {
            return Err(err);
        }
        let mut branches = Vec::new();
        for ln in out.split('\n') {
            let b = ln.trim();
            if b.is_empty() || b == base {
                continue;
            }
            branches.push(b.to_string());
        }
        Ok(branches)
    }

    /// Returns the open PR number(s) whose head is `branch` (normally one, but gh returns a list).
    /// Output is one number per line via `--jq`.
    pub(crate) async fn pr_numbers_for_branch(
        &self,
        worktree_dir: &str,
        branch: &str,
    ) -> Result<Vec<String>, Error> {
        let (out, err) = self
            .gh(
                worktree_dir,
                &[
                    "pr",
                    "list",
                    "--head",
                    branch,
                    "--json",
                    "number",
                    "--jq",
                    ".[].number",
                ],
            )
            .await;
        if let Some(err) = err {
            return Err(err);
        }
        let mut nums = Vec::new();
        for ln in out.split('\n') {
            let n = ln.trim();
            if !n.is_empty() {
                nums.push(n.to_string());
            }
        }
        Ok(nums)
    }

    /// Runs a `gh` CLI invocation in `dir`, bounded by [`GH_CALL_TIMEOUT`], returning the combined
    /// output and (on a non-zero exit, spawn failure, or timeout) an [`Error::GhFailed`]. It
    /// inherits the daemon's environment (mirror of Go's `cmd.Env = os.Environ()`) so gh picks up
    /// the same `GH_TOKEN`/`GITHUB_TOKEN`/login the agent uses; the (production-empty)
    /// [`Manager::gh_env_overlay`] is layered on top for tests. Callers log and swallow the error.
    pub(crate) async fn gh(&self, dir: &str, args: &[&str]) -> (String, Option<Error>) {
        let mut cmd = Command::new("gh");
        cmd.args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        // Inherit the ambient environment (no env_clear), then layer the overlay: empty in
        // production, so gh sees exactly os.Environ(); in tests it redirects PATH to a fake gh and
        // sets GH_LOG/GH_PRMAP.
        for (k, v) in &self.gh_env_overlay {
            cmd.env(k, v);
        }
        match tokio::time::timeout(GH_CALL_TIMEOUT, cmd.output()).await {
            Ok(Ok(output)) => {
                let mut combined = output.stdout;
                combined.extend_from_slice(&output.stderr);
                let out = String::from_utf8_lossy(&combined).into_owned();
                if output.status.success() {
                    (out, None)
                } else {
                    (
                        out,
                        Some(Error::GhFailed(format!(
                            "gh {}: {}",
                            args.join(" "),
                            output.status
                        ))),
                    )
                }
            }
            Ok(Err(e)) => (
                String::new(),
                Some(Error::GhFailed(format!("gh {}: {e}", args.join(" ")))),
            ),
            Err(_elapsed) => (
                String::new(),
                Some(Error::GhFailed(format!(
                    "gh {}: timed out after {GH_CALL_TIMEOUT:?}",
                    args.join(" ")
                ))),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::HookScripts;
    use crate::testutil::{
        TempDir, build_stack_worktree, init_local_origin, read_log, repo_test_manager,
        write_fake_gh, write_gh_with_script,
    };

    // Mirror of TestLabelRunPRsLabelsWholeStack: across a multi-branch stack the labeler
    // (1) creates the label once and ignores the create error, (2) enumerates exactly the run's
    // stack branches (excluding trunk + a sibling run's branch), (3) issues one add-label per
    // discovered PR, and (4) skips a stack branch that has no PR.
    #[tokio::test]
    async fn label_run_prs_labels_whole_stack() {
        let fake = write_fake_gh();
        let (mut m, _root) = repo_test_manager(HookScripts::default());
        m.gh_env_overlay = fake.overlay.clone();
        let origin = init_local_origin();
        let (wt, base) = build_stack_worktree(&m, &origin.path).await;

        // branchA, branchB have PRs; the base branch has none; sibling has a PR that must never be
        // queried because enumeration excludes it.
        std::fs::write(
            &fake.prmap_path,
            format!("branchA=101\nbranchB=102\n{base}=\nsymphony/sibling-run=999\n"),
        )
        .unwrap();

        m.label_run_prs(&origin.path, &wt, "symphony").await;

        let log = read_log(&fake.log_path);

        // (1) label created exactly once (create error was ignored — labeling continued).
        assert_eq!(
            log.matches("ARGS: label create symphony ").count(),
            1,
            "expected exactly one `label create symphony`\nlog:\n{log}"
        );

        // (2) the three stack branches are queried; the trunk and the sibling are NOT.
        let base_want = format!("ARGS: pr list --head {base} ");
        for want in [
            "ARGS: pr list --head branchA ",
            "ARGS: pr list --head branchB ",
            base_want.as_str(),
        ] {
            assert!(
                log.contains(want),
                "expected pr list for stack branch: {want:?}\nlog:\n{log}"
            );
        }
        for bad in [
            "ARGS: pr list --head main ",
            "ARGS: pr list --head symphony/sibling-run ",
        ] {
            assert!(
                !log.contains(bad),
                "did not expect pr list for excluded branch: {bad:?}\nlog:\n{log}"
            );
        }

        // (3) one add-label per discovered PR (101, 102); (4) the no-PR base yields no edit.
        assert!(
            log.contains("ARGS: pr edit 101 --add-label symphony"),
            "expected add-label on PR 101\nlog:\n{log}"
        );
        assert!(
            log.contains("ARGS: pr edit 102 --add-label symphony"),
            "expected add-label on PR 102\nlog:\n{log}"
        );
        assert_eq!(
            log.matches("ARGS: pr edit ").count(),
            2,
            "expected exactly 2 add-label edits (PRs 101,102)\nlog:\n{log}"
        );
        assert!(
            !log.contains("ARGS: pr edit 999"),
            "sibling-run PR 999 must never be labeled\nlog:\n{log}"
        );
    }

    // Mirror of TestLabelRunPRsSwallowsGhFailures: a non-zero `gh` exit on every call never
    // escalates — LabelRunPRs returns normally (no panic), and no PR is edited.
    #[tokio::test]
    async fn label_run_prs_swallows_gh_failures() {
        // A gh that records the call and fails for everything.
        let fake =
            write_gh_with_script("#!/usr/bin/env bash\necho \"ARGS: $*\" >> \"$GH_LOG\"\nexit 1\n");
        let (mut m, _root) = repo_test_manager(HookScripts::default());
        m.gh_env_overlay = fake.overlay.clone();
        let origin = init_local_origin();
        let (wt, _base) = build_stack_worktree(&m, &origin.path).await;

        // Must not panic or block; returns regardless of gh failures.
        m.label_run_prs(&origin.path, &wt, "symphony").await;

        let log = read_log(&fake.log_path);
        // It still tried (label create + at least one pr list), proving failures were swallowed.
        assert!(
            log.contains("ARGS: label create symphony "),
            "expected the labeler to attempt label create despite failures\nlog:\n{log}"
        );
        assert!(
            log.contains("ARGS: pr list --head "),
            "expected the labeler to attempt pr list despite failures\nlog:\n{log}"
        );
        // A failed `pr list` is skipped, so nothing is edited.
        assert!(
            !log.contains("ARGS: pr edit "),
            "no PR should be edited when pr list fails\nlog:\n{log}"
        );
    }

    // Mirror of TestLabelRunPRsCancelledContextNoPanic. Go pre-cancels the ctx so `defaultBranch`
    // fails before any gh call; this crate models Go's ctx as drop-based async cancellation and has
    // no ctx to pre-cancel, so it exercises the SAME observable guarantee — a git enumeration that
    // fails fast degrades LabelRunPRs to a no-op (no `pr edit`, no panic) — by pointing the labeler
    // at a worktree dir that is not a git repo, so `default_branch` fails before any gh call.
    #[tokio::test]
    async fn label_run_prs_failing_enumeration_no_panic() {
        let fake = write_fake_gh();
        let (mut m, _root) = repo_test_manager(HookScripts::default());
        m.gh_env_overlay = fake.overlay.clone();
        let origin = init_local_origin();
        let not_a_repo = TempDir::new();

        // Must not panic/block; a failed git enumeration is swallowed before any gh call.
        m.label_run_prs(&origin.path, &not_a_repo.path, "symphony")
            .await;

        let log = std::fs::read_to_string(&fake.log_path).unwrap_or_default();
        assert!(
            !log.contains("ARGS: pr edit "),
            "no PR should be edited when enumeration fails\nlog:\n{log}"
        );
    }

    // Mirror of TestLabelRunPRsNoOp: the labeler is a no-op (never invokes gh) on the legacy
    // empty-repoURL path, when the label resolves to empty, and when the worktree is empty.
    #[tokio::test]
    async fn label_run_prs_noop() {
        let fake = write_fake_gh();
        let (mut m, _root) = repo_test_manager(HookScripts::default());
        m.gh_env_overlay = fake.overlay.clone();
        let origin = init_local_origin();
        let (wt, _base) = build_stack_worktree(&m, &origin.path).await;

        // (repo_url, worktree, label): empty repoURL (legacy), empty label, empty worktree.
        m.label_run_prs("", &wt, "symphony").await;
        m.label_run_prs(&origin.path, &wt, "  ").await;
        m.label_run_prs(&origin.path, "", "symphony").await;

        let log = std::fs::read_to_string(&fake.log_path).unwrap_or_default();
        assert!(
            log.trim().is_empty(),
            "expected no gh invocations on no-op paths, got:\n{log}"
        );
    }
}
