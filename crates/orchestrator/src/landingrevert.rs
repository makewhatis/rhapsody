//! Landing recovery PRs (STUDIO-1250); owned off-loop dependencies, no Go counterpart.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rhapsody_store::LandingRow;
use rhapsody_workspace::Manager;

use crate::ghsummons::PrStatus;
use crate::landingcheck::CheckOutcome;

#[derive(Debug, thiserror::Error)]
pub(crate) enum RevertError {
    #[error("git revert conflicts: {0}")]
    Conflict(String),
    #[error("landing revert: {0}")]
    Other(String),
}

pub(crate) struct RevertSnapshot {
    pub head: String,
    pub status: PrStatus,
    pub labelled: bool,
    pub merge_sha: String,
}

#[async_trait::async_trait]
pub(crate) trait RevertGithub: Send + Sync {
    async fn original(&self, row: &LandingRow) -> Result<(String, String), RevertError>;
    async fn find(&self, row: &LandingRow, branch: &str) -> Result<Option<i64>, RevertError>;
    async fn open(
        &self,
        row: &LandingRow,
        branch: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<i64, RevertError>;
    async fn snapshot(&self, row: &LandingRow, pr: i64) -> Result<RevertSnapshot, RevertError>;
}

pub(crate) struct Reverter {
    pub manager: Arc<Manager>,
    pub repo_url: String,
    pub github: Arc<dyn RevertGithub>,
    pub merges: Arc<crate::runautomerge::AutoMergeDeps>,
    pub auto_merge: bool,
    pub control: Option<crate::stop::ControlHandle>,
    pub reviewers: Vec<String>,
    pub holds: Arc<crate::dispatch::HumanHoldLedger>,
}

impl Reverter {
    pub(crate) fn production(
        manager: Arc<Manager>,
        repo_url: String,
        auto_merge: bool,
        control: crate::stop::ControlHandle,
        holds: Arc<crate::dispatch::HumanHoldLedger>,
        reviewers: Vec<String>,
    ) -> Self {
        let gh = Arc::new(crate::ghsummons::GH::new("", None));
        Self {
            manager,
            repo_url,
            github: gh.clone(),
            auto_merge,
            control: Some(control),
            holds,
            reviewers,
            merges: Arc::new(crate::runautomerge::AutoMergeDeps {
                prs: gh.clone(),
                mergestate: gh.clone(),
                policy: gh.clone(),
                updater: gh.clone(),
                checks: gh.clone(),
                merger: gh,
                allow: crate::ghsummons::HeadAllowlist::none(),
                approvals: None,
                ledger: Arc::new(crate::runautomerge::AutoMergeLedger::default()),
            }),
        }
    }
    pub(crate) async fn introduce_review(&self, row: &LandingRow, pr: i64) -> Result<(), String> {
        let Some(control) = &self.control else {
            return Ok(());
        };
        let outcome = control
            .introduce_review(crate::reviewintro::IntroducedPr {
                pr: crate::prstate::PrCoord::new(&row.owner, &row.repo, pr),
                repo_url: self.repo_url.clone(),
                reviewers: self.reviewers.clone(),
                author: String::new(),
                introduced_by: format!("landing-revert:{}", row.id),
                only_if_unwatched: true,
            })
            .await;
        match outcome {
            crate::reviewintro::ReviewIntroOutcome::Introduced(_)
            | crate::reviewintro::ReviewIntroOutcome::AlreadyWatched => Ok(()),
            _ => Err(format!(
                "revert peer review could not be introduced: {outcome:?}"
            )),
        }
    }
    pub(crate) async fn open_revert(
        &self,
        row: &LandingRow,
        failed: &[(String, CheckOutcome)],
    ) -> Result<i64, RevertError> {
        if row.revert_of.is_some() {
            return Err(RevertError::Other(
                "a revert landing cannot be reverted again".into(),
            ));
        }
        if let Some((owner, repo)) = crate::ghsummons::parse_repo(&self.repo_url)
            && (!owner.eq_ignore_ascii_case(&row.owner) || !repo.eq_ignore_ascii_case(&row.repo))
        {
            return Err(RevertError::Other(
                "configured repository no longer matches the landing".into(),
            ));
        }
        let key = format!("landing-revert-{}", row.id);
        let branch = format!("symphony/{key}");
        // Reconcile an ambiguous create (or a crash after it) before touching git. All states are
        // searched: a closed recovery PR requires attention, never a replacement PR.
        if let Some(pr) = self.github.find(row, &branch).await? {
            return Ok(pr);
        }
        let (title, base) = self.github.original(row).await?;
        if !(7..=64).contains(&row.merge_sha.len())
            || !row.merge_sha.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(RevertError::Other("invalid merge SHA".into()));
        }
        let workspace = self
            .manager
            .ensure_from_repo(&self.repo_url, &row.project_slug, &key)
            .await
            .map_err(|e| RevertError::Other(e.to_string()))?;
        let path = Path::new(&workspace.path);
        git(path, &["check-ref-format", "--branch", &base]).await?;
        // A recovery must use the current base, not a stale mirror tolerated by normal provisioning.
        let base_ref = format!("refs/remotes/origin/{base}");
        git(
            path,
            &["fetch", "origin", &format!("refs/heads/{base}:{base_ref}")],
        )
        .await?;
        let remote = git(
            path,
            &[
                "ls-remote",
                "--heads",
                "origin",
                &format!("refs/heads/{branch}"),
            ],
        )
        .await?;
        if !remote.trim().is_empty() {
            git(path, &["fetch", "origin", &format!("refs/heads/{branch}")]).await?;
            git(path, &["reset", "--hard", "FETCH_HEAD"]).await?;
        }
        let message = git(path, &["log", "-1", "--format=%B"]).await?;
        let marker = format!("This reverts commit {}", row.merge_sha);
        if !message.contains(&marker) {
            if !remote.trim().is_empty() {
                return Err(RevertError::Other(
                    "recovery branch exists but is not this landing's revert".into(),
                ));
            }
            git(path, &["reset", "--hard", &base_ref]).await?;
            git(
                path,
                &["merge-base", "--is-ancestor", &row.merge_sha, "HEAD"],
            )
            .await?;
            let parents = git(path, &["rev-list", "--parents", "-n", "1", &row.merge_sha]).await?;
            let mut args = vec!["revert", "--no-edit"];
            if parents.split_whitespace().count() > 2 {
                args.extend(["-m", "1"]);
            }
            args.push(&row.merge_sha);
            if let Err(error) = git(path, &args).await {
                let conflicted = !git(path, &["ls-files", "-u"]).await?.trim().is_empty();
                if conflicted {
                    git(path, &["revert", "--abort"]).await?;
                    return Err(RevertError::Conflict(error.to_string()));
                }
                return Err(error);
            }
        }
        git(
            path,
            &["push", "origin", &format!("HEAD:refs/heads/{branch}")],
        )
        .await?;
        let first = failed
            .first()
            .map(|(name, _)| name.as_str())
            .unwrap_or("acceptance");
        let title = format!(
            "revert: {} (landing failed: {})",
            crate::landing::cell(&title),
            crate::landing::cell(first)
        );
        let mut body = format!(
            "Automatic recovery for {}/{}#{} ({}, landing {}).\n\nmerge: `{}`\n\ncheck | observed\n--- | ---\n",
            row.owner, row.repo, row.pr, row.ticket, row.id, row.merge_sha
        );
        for (name, outcome) in failed {
            body.push_str(&format!(
                "{} | {}\n",
                crate::landing::cell(name),
                crate::landing::cell(&outcome.observed)
            ));
        }
        self.github.open(row, &branch, &base, &title, &body).await
    }
}

/// These commands touch only the dedicated recovery worktree, never another ticket's checkout.
/// Timeouts/cancellation kill the process group, including a stalled SSH child.
async fn git(path: &Path, args: &[&str]) -> Result<String, RevertError> {
    let mut command = tokio::process::Command::new("git");
    command
        .current_dir(path)
        .args([
            "-c",
            "gc.auto=0",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=Rhapsody",
            "-c",
            "user.email=rhapsody@localhost",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        command.env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes");
    }
    let child = command
        .spawn()
        .map_err(|e| RevertError::Other(e.to_string()))?;
    let _group = GitGroup(child.id());
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .map_err(|_| RevertError::Other("git recovery command timed out".into()))?
        .map_err(|e| RevertError::Other(e.to_string()))?;
    if !output.status.success() {
        let detail: String = String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(2048)
            .collect();
        return Err(RevertError::Other(format!(
            "git {}: {}: {detail}",
            args.first().copied().unwrap_or(""),
            output.status
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}
struct GitGroup(Option<u32>);
impl Drop for GitGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0.and_then(|p| i32::try_from(p).ok()) {
            // SAFETY: this is the process group just created for this command.
            if unsafe { libc::kill(-pid, libc::SIGKILL) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    tracing::warn!(pid, %error, "revert git process group cleanup failed");
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ghsummons::*;
    use crate::landing::tests::{RepoRig, git};
    use std::sync::Mutex;

    pub(crate) struct Github {
        pub opened: Mutex<Vec<(String, String, String)>>,
        pub existing: Mutex<Option<i64>>,
        pub state: Mutex<(PrStatus, String, String)>,
    }
    impl Github {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self {
                opened: Mutex::new(Vec::new()),
                existing: Mutex::new(None),
                state: Mutex::new((PrStatus::Open, "abcdef1234567".into(), String::new())),
            })
        }
    }
    #[async_trait::async_trait]
    impl RevertGithub for Github {
        async fn original(&self, _: &LandingRow) -> Result<(String, String), RevertError> {
            Ok(("ship health".into(), "main".into()))
        }
        async fn find(&self, _: &LandingRow, _: &str) -> Result<Option<i64>, RevertError> {
            Ok(*self.existing.lock().unwrap())
        }
        async fn open(
            &self,
            _: &LandingRow,
            branch: &str,
            _: &str,
            title: &str,
            body: &str,
        ) -> Result<i64, RevertError> {
            self.opened
                .lock()
                .unwrap()
                .push((branch.into(), title.into(), body.into()));
            *self.existing.lock().unwrap() = Some(2);
            Ok(2)
        }
        async fn snapshot(&self, _: &LandingRow, _: i64) -> Result<RevertSnapshot, RevertError> {
            let (status, head, merge_sha) = self.state.lock().unwrap().clone();
            Ok(RevertSnapshot {
                status,
                head,
                merge_sha,
                labelled: true,
            })
        }
    }
    pub(crate) fn reverter(rig: &RepoRig, github: Arc<dyn RevertGithub>) -> Arc<Reverter> {
        let gh = Arc::new(GH::new(
            "",
            Some(Box::new(|_| Err("unexpected merge".into()))),
        ));
        let holds = Arc::new(crate::dispatch::HumanHoldLedger::default());
        holds.begin_pass(true);
        Arc::new(Reverter {
            manager: crate::testsupport::mk_workspace(&rig._dir.child("reverts")),
            repo_url: rig.origin.clone(),
            github,
            auto_merge: false,
            control: None,
            reviewers: Vec::new(),
            holds,
            merges: Arc::new(crate::runautomerge::AutoMergeDeps {
                prs: gh.clone(),
                mergestate: gh.clone(),
                policy: gh.clone(),
                updater: gh.clone(),
                checks: gh.clone(),
                merger: gh,
                allow: HeadAllowlist::none(),
                approvals: None,
                ledger: Arc::new(crate::runautomerge::AutoMergeLedger::default()),
            }),
        })
    }
    #[tokio::test]
    async fn failure_with_auto_revert_opens_labelled_revert_pr_with_evidence() {
        let rig = RepoRig::new(
            None,
            Some("version: 1\nlive: {command: [git, rev-parse, HEAD]}\nchecks: []\n"),
        );
        let gh = Github::new();
        let revert = reverter(&rig, gh.clone());
        let failed = vec![(
            "health".into(),
            CheckOutcome {
                pass: false,
                observed: "status=503".into(),
                duration: Duration::ZERO,
            },
        )];
        assert_eq!(revert.open_revert(&rig.row(), &failed).await.unwrap(), 2);
        let opened = gh.opened.lock().unwrap();
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].1, "revert: ship health (landing failed: health)");
        assert!(opened[0].2.contains("status=503"));
        let branch = opened[0].0.clone();
        drop(opened);
        assert_eq!(
            git(
                &rig.origin,
                &[
                    "diff",
                    "--stat",
                    &format!("{}^1", rig.row().merge_sha),
                    &branch
                ]
            ),
            ""
        );
    }
    #[tokio::test]
    async fn revert_conflict_holds_and_escalates_without_opening_pr() {
        let mut rig = RepoRig::new(
            None,
            Some("version: 1\nlive: {command: [git, rev-parse, HEAD]}\nchecks: []\n"),
        );
        std::fs::write(
            Path::new(&rig.origin).join(".rhapsody/landing.yaml"),
            "changed again\n",
        )
        .unwrap();
        git(&rig.origin, &["add", "-A"]);
        git(
            &rig.origin,
            &["commit", "-m", "human merge conflicts with rollback"],
        );
        let gh = Github::new();
        rig.runtime.reverter = Some(reverter(&rig, gh.clone()));
        rig.runtime.notifications = Some(Arc::new(crate::breaker::NotificationsState::default()));
        let mut row = rig.row();
        row.phase = rhapsody_store::LandingPhase::Reverting;
        rig.runtime.store.save_landing(row).unwrap();
        rig.tick(1).await;
        assert_eq!(rig.row().phase, rhapsody_store::LandingPhase::Held);
        assert!(rig.row().hold_reason.unwrap().contains("conflict"));
        assert!(gh.opened.lock().unwrap().is_empty());
        let comments = rig.runtime.tracker.list_comments("ID-1").await.unwrap();
        assert!(comments[0].body.contains("conflict"));
        assert_eq!(
            rig.runtime.notifications.as_ref().unwrap().pending().len(),
            1
        );
    }
    #[tokio::test]
    async fn existing_revert_is_reconciled_without_repeating_git_or_opening_a_pr() {
        let rig = RepoRig::new(None, None);
        let gh = Github::new();
        *gh.existing.lock().unwrap() = Some(2);
        let revert = reverter(&rig, gh.clone());
        assert_eq!(revert.open_revert(&rig.row(), &[]).await.unwrap(), 2);
        assert!(gh.opened.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn github_creation_applies_the_revert_label_and_reads_it_back() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let gh = GH::new(
            "",
            Some(Box::new(move |args| {
                recorded
                    .lock()
                    .unwrap()
                    .push(args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
                let result = match &args[..2] {
                    ["label", "list"] => "[]",
                    ["label", "create"] => "",
                    ["pr", "create"] => "https://github.com/o/r/pull/2\n",
                    ["pr", "view"] => {
                        r#"{"state":"OPEN","headRefOid":"abcdef1234567","labels":[{"name":"rhapsody:revert"}]}"#
                    }
                    _ => return Err("unexpected gh call".into()),
                };
                Ok(result.as_bytes().to_vec())
            })),
        );
        let row = LandingRow {
            owner: "o".into(),
            repo: "r".into(),
            ..Default::default()
        };
        assert_eq!(
            gh.open(
                &row,
                "symphony/landing-revert-1",
                "main",
                "revert: title (landing failed: health)",
                "health: status=503"
            )
            .await
            .unwrap(),
            2
        );
        assert!(gh.snapshot(&row, 2).await.unwrap().labelled);
        let calls = calls.lock().unwrap();
        let create = calls
            .iter()
            .find(|a| a[0] == "pr" && a[1] == "create")
            .unwrap();
        assert!(
            create
                .windows(2)
                .any(|w| w == ["--label", "rhapsody:revert"])
        );
        assert!(
            create
                .windows(2)
                .any(|w| w == ["--body", "health: status=503"])
        );
        assert_eq!(
            calls
                .iter()
                .filter(|a| a[0] == "pr" && a[1] == "create")
                .count(),
            1
        );
    }
}
