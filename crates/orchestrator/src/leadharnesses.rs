//! Live operator edits of the one manager/lead harness list (STUDIO-1157).

use super::*;
use std::{path::PathBuf, sync::Arc};

pub struct LeadHarnesses {
    pub state: Arc<ManagerSelfTestState>,
    pub factory: Arc<dyn CanaryRunnerFactory>,
    pub path: PathBuf,
    pub room: Option<Arc<rhapsody_config::room::LocalRoom>>,
}

impl LeadHarnesses {
    pub fn snapshot(&self) -> HarnessesView {
        view(&self.state)
    }

    pub async fn update(
        &self,
        entries: Vec<ManagerHarnessEntry>,
    ) -> Result<HarnessesView, UpdateError> {
        validate(&entries).map_err(UpdateError::Invalid)?;
        // Serializes edits with boot/version canaries, but never blocks selection or control.
        let _guard = self.state.test_gate.lock().await;
        let candidate = ManagerSelfTestState::new(entries.clone());
        candidate.set_credential_probe(self.state.credential_probe());
        {
            let live = self.state.inner.lock().unwrap_or_else(|e| e.into_inner());
            let mut next = candidate.inner.lock().unwrap_or_else(|e| e.into_inner());
            for entry in &mut next.entries {
                if let Some(old) = live.entries.iter().find(|old| old.entry == entry.entry) {
                    *entry = old.clone();
                }
            }
        }
        // The same isolation evaluation as boot; no hermetic/skip path exists here.
        run_entry_self_tests(self.factory.as_ref(), &candidate).await;
        let tested = view(&candidate);
        if !tested.harnesses.iter().any(|entry| entry.state == "passed") {
            return Err(UpdateError::AllFailed(tested));
        }
        // Save before publishing: a failed write leaves the old list live.
        rhapsody_config::teams::Teams::save_manager_harnesses(&self.path, &entries)
            .map_err(|e| UpdateError::Persist(e.to_string()))?;
        {
            let mut live = self.state.inner.lock().unwrap_or_else(|e| e.into_inner());
            let mut next = candidate.inner.lock().unwrap_or_else(|e| e.into_inner());
            // Runs and selection continued during the canaries. Preserve a newly observed auth
            // rejection/notice for an unchanged tuple rather than publishing a stale clone.
            for entry in &mut next.entries {
                if let Some(current) = live.entries.iter().find(|old| old.entry == entry.entry) {
                    entry.auth_blocked = current.auth_blocked.clone();
                    entry.warned_at_ms = current.warned_at_ms;
                    entry.credential_notice = current.credential_notice.clone();
                }
            }
            live.entries = std::mem::take(&mut next.entries);
            live.generation = live.generation.saturating_add(1);
            live.record = live.entries.first().and_then(|e| e.record.clone());
            live.installed_version = live
                .entries
                .first()
                .and_then(|e| e.installed_version.clone());
        }
        let body = format!(
            "Lead now runs on {} (changed by operator)",
            entries
                .iter()
                .map(|e| format!("{} {} {}", e.harness, e.model, e.effort))
                .collect::<Vec<_>>()
                .join(" → ")
        );
        tracing::info!("{body}");
        if let Some(room) = &self.room
            && let Err(error) = room.append(&rhapsody_config::room::Message::room(
                "manager",
                chrono::Utc::now(),
                body,
            ))
        {
            tracing::warn!(%error, "lead harness change room post failed");
        }
        Ok(self.snapshot())
    }
}

#[derive(Debug)]
pub enum UpdateError {
    Invalid(String),
    AllFailed(HarnessesView),
    Persist(String),
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HarnessEntryView {
    #[serde(flatten)]
    pub entry: ManagerHarnessEntry,
    pub state: String,
    pub tested_at: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HarnessesView {
    pub harnesses: Vec<HarnessEntryView>,
    pub last_used: Option<ManagerHarnessEntry>,
}

fn view(state: &ManagerSelfTestState) -> HarnessesView {
    let inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    HarnessesView {
        harnesses: inner
            .entries
            .iter()
            .map(|entry| HarnessEntryView {
                entry: entry.entry.clone(),
                tested_at: entry.tested_at.clone(),
                state: if entry.testing {
                    "testing".into()
                } else {
                    match &entry.record {
                        Some(SelfTestRecord {
                            verdict: SelfTestVerdict::Failed(reason),
                            ..
                        }) => format!("failed: {}", reason.detail),
                        Some(record)
                            if entry.installed_version.as_ref() == Some(&record.cli_version) =>
                        {
                            match &record.verdict {
                                SelfTestVerdict::Passed => "passed".into(),
                                SelfTestVerdict::Failed(reason) => {
                                    format!("failed: {}", reason.detail)
                                }
                            }
                        }
                        Some(_) | None => "testing".into(),
                    }
                },
            })
            .collect(),
        last_used: inner.last_used.clone(),
    }
}

pub fn validate(entries: &[ManagerHarnessEntry]) -> Result<(), String> {
    if !(1..=4).contains(&entries.len()) {
        return Err("harnesses must contain 1–4 entries".into());
    }
    let mut seen = std::collections::HashSet::new();
    for (index, entry) in entries.iter().enumerate() {
        let field = format!("harnesses[{index}]");
        let efforts: &[&str] = match entry.harness.as_str() {
            "claude" => &["", "low", "medium", "high", "max"],
            "opencode" => &["", "none", "minimal", "low", "medium", "high", "xhigh"],
            _ => return Err(format!("{field}.harness must be claude or opencode")),
        };
        if entry.model.trim().is_empty()
            || entry.model.len() > 256
            || entry.model.chars().any(char::is_control)
        {
            return Err(format!(
                "{field}.model must be non-empty, at most 256 bytes, with no control characters"
            ));
        }
        if !efforts.contains(&entry.effort.as_str()) {
            return Err(format!(
                "{field}.effort is not supported by {}",
                entry.harness
            ));
        }
        if !seen.insert((&entry.harness, &entry.model)) {
            return Err(format!(
                "{field}.model duplicates an earlier (harness, model)"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Factory(Arc<AtomicUsize>);
    struct Canary;
    struct FailedCanary;
    #[async_trait::async_trait]
    impl CanaryRunner for FailedCanary {
        async fn run_canary(&self, _: &str) -> Vec<CanaryObservation> {
            vec![CanaryObservation {
                attempt: CanaryAttempt::InitContract,
                refused: false,
                detail: "built-in shell exposed".into(),
            }]
        }
    }
    #[async_trait::async_trait]
    impl CanaryRunner for Canary {
        async fn run_canary(&self, _: &str) -> Vec<CanaryObservation> {
            REQUIRED_ATTEMPTS
                .iter()
                .map(|attempt| CanaryObservation {
                    attempt: *attempt,
                    refused: true,
                    detail: "refused by fake harness".into(),
                })
                .collect()
        }
    }
    impl CanaryRunnerFactory for Factory {
        fn probe_version(&self, _: &ManagerHarnessEntry) -> Result<String, String> {
            Ok("1".into())
        }
        fn runner(&self, entry: &ManagerHarnessEntry) -> Option<Box<dyn CanaryRunner>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            if entry.model == "bad" {
                Some(Box::new(FailedCanary))
            } else {
                Some(Box::new(Canary))
            }
        }
    }
    fn entry(model: &str) -> ManagerHarnessEntry {
        ManagerHarnessEntry {
            harness: "claude".into(),
            model: model.into(),
            effort: "high".into(),
        }
    }
    #[tokio::test]
    async fn new_entry_runs_self_test_before_first_use() {
        let dir = crate::testsupport::TempDir::new();
        let path = PathBuf::from(dir.child("teams.yaml"));
        rhapsody_config::teams::Teams::save(&path, &rhapsody_config::teams::Teams::disabled())
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let service = LeadHarnesses {
            state: Arc::new(ManagerSelfTestState::new(vec![entry("old")])),
            factory: Arc::new(Factory(calls.clone())),
            path,
            room: None,
        };
        service.update(vec![entry("new")]).await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the PUT update must exercise the canary"
        );
        assert_eq!(
            service
                .state
                .select(0, &NativeCredentialProbe)
                .unwrap()
                .entry
                .model,
            "new"
        );
    }

    fn service(dir: &crate::testsupport::TempDir) -> LeadHarnesses {
        let path = PathBuf::from(dir.child("teams.yaml"));
        std::fs::write(&path, "enabled: false\nmanager:\n  model: old\n  max_concurrent: 2\nreview:\n  reviewers: 3\nfuture_key: keep-me\n").unwrap();
        let state = Arc::new(ManagerSelfTestState::new(vec![entry("old")]));
        state.record_entry(
            0,
            SelfTestRecord {
                cli_version: "1".into(),
                verdict: SelfTestVerdict::Passed,
            },
        );
        LeadHarnesses {
            state,
            factory: Arc::new(Factory(Arc::new(AtomicUsize::new(0)))),
            path,
            room: Some(Arc::new(rhapsody_config::room::LocalRoom::new(
                dir.child("room"),
            ))),
        }
    }

    #[tokio::test]
    async fn failed_entry_marked_unavailable_others_used() {
        let dir = crate::testsupport::TempDir::new();
        let service = service(&dir);
        let view = service
            .update(vec![entry("bad"), entry("good")])
            .await
            .unwrap();
        assert!(view.harnesses[0].state.contains("failed:"));
        assert!(view.harnesses[0].state.contains("built-in shell exposed"));
        assert!(view.harnesses.iter().all(|entry| entry.tested_at.is_some()));
        assert_eq!(
            service
                .state
                .select(0, &NativeCredentialProbe)
                .unwrap()
                .entry
                .model,
            "good"
        );
    }

    #[tokio::test]
    async fn all_entries_fail_rejected_old_list_kept() {
        let dir = crate::testsupport::TempDir::new();
        let service = service(&dir);
        let old = std::fs::read_to_string(&service.path).unwrap();
        assert!(matches!(
            service.update(vec![entry("bad")]).await,
            Err(UpdateError::AllFailed(_))
        ));
        assert_eq!(
            service
                .state
                .select(0, &NativeCredentialProbe)
                .unwrap()
                .entry
                .model,
            "old"
        );
        assert_eq!(std::fs::read_to_string(&service.path).unwrap(), old);
    }

    #[tokio::test]
    async fn persisted_to_teams_yaml_other_keys_untouched() {
        let dir = crate::testsupport::TempDir::new();
        let service = service(&dir);
        let mut before: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&std::fs::read_to_string(&service.path).unwrap()).unwrap();
        service
            .update(vec![entry("new"), entry("fallback")])
            .await
            .unwrap();
        before["manager"]["harnesses"] =
            serde_yaml_ng::to_value(vec![entry("new"), entry("fallback")]).unwrap();
        let after: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&std::fs::read_to_string(&service.path).unwrap()).unwrap();
        assert_eq!(before, after);
        let reloaded = rhapsody_config::teams::Teams::try_load(&service.path).unwrap();
        let restarted = ManagerSelfTestState::new(reloaded.manager.effective_harnesses());
        assert_eq!(restarted.entries(), service.state.entries());
        assert!(
            restarted.select(0, &NativeCredentialProbe).is_err(),
            "restart still needs its boot canary"
        );
    }

    #[tokio::test]
    async fn reorder_reuses_verdict_and_old_auth_failure_does_not_poison_new_entry() {
        let dir = crate::testsupport::TempDir::new();
        let service = service(&dir);
        service
            .update(vec![entry("old"), entry("new")])
            .await
            .unwrap();
        let old_selection = service.state.select(0, &NativeCredentialProbe).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let service = LeadHarnesses {
            factory: Arc::new(Factory(calls.clone())),
            ..service
        };
        service
            .update(vec![entry("new"), entry("old")])
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "reorder is not a changed entry"
        );
        service
            .state
            .mark_selected_auth_blocked(&old_selection, None);
        assert_eq!(
            service
                .state
                .select(0, &NativeCredentialProbe)
                .unwrap()
                .entry
                .model,
            "new"
        );
    }

    #[tokio::test]
    async fn room_post_and_log_on_change() {
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await;
        let dir = crate::testsupport::TempDir::new();
        let service = service(&dir);
        let (events, subscriber) = crate::testsupport::recording_subscriber();
        let _guard = tracing::subscriber::set_default(subscriber);
        service.update(vec![entry("warm")]).await.unwrap();
        tracing::callsite::rebuild_interest_cache();
        events.lock().unwrap().clear();
        service
            .update(vec![entry("new"), entry("fallback")])
            .await
            .unwrap();
        assert!(events.lock().unwrap().iter().any(|event| event.level == "INFO" && event.message.contains("Lead now runs on claude new high → claude fallback high (changed by operator)")));
        let text = std::fs::read_to_string(
            PathBuf::from(dir.child("room"))
                .join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d"))),
        )
        .unwrap();
        let post: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(post["from"], "manager");
        assert!(
            post["body"]
                .as_str()
                .unwrap()
                .contains("changed by operator")
        );
    }

    struct BlockingFactory {
        started: Arc<tokio::sync::Notify>,
        finish: Arc<tokio::sync::Notify>,
    }
    struct BlockingCanary {
        started: Arc<tokio::sync::Notify>,
        finish: Arc<tokio::sync::Notify>,
    }
    #[async_trait::async_trait]
    impl CanaryRunner for BlockingCanary {
        async fn run_canary(&self, version: &str) -> Vec<CanaryObservation> {
            self.started.notify_one();
            self.finish.notified().await;
            Canary.run_canary(version).await
        }
    }
    impl CanaryRunnerFactory for BlockingFactory {
        fn probe_version(&self, _: &ManagerHarnessEntry) -> Result<String, String> {
            Ok("1".into())
        }
        fn runner(&self, _: &ManagerHarnessEntry) -> Option<Box<dyn CanaryRunner>> {
            Some(Box::new(BlockingCanary {
                started: self.started.clone(),
                finish: self.finish.clone(),
            }))
        }
    }
    #[tokio::test]
    async fn live_list_remains_usable_while_new_entry_is_testing() {
        let dir = crate::testsupport::TempDir::new();
        let started = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let service = Arc::new(LeadHarnesses {
            factory: Arc::new(BlockingFactory {
                started: started.clone(),
                finish: finish.clone(),
            }),
            ..service(&dir)
        });
        let edit = {
            let service = service.clone();
            tokio::spawn(async move { service.update(vec![entry("new")]).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        assert_eq!(
            service
                .state
                .select(0, &NativeCredentialProbe)
                .unwrap()
                .entry
                .model,
            "old"
        );
        assert_eq!(service.snapshot().harnesses[0].entry.model, "old");
        finish.notify_one();
        edit.await.unwrap().unwrap();
        assert_eq!(
            service
                .state
                .select(0, &NativeCredentialProbe)
                .unwrap()
                .entry
                .model,
            "new"
        );
    }

    #[tokio::test]
    async fn unchanged_entry_keeps_an_auth_rejection_observed_during_testing() {
        let dir = crate::testsupport::TempDir::new();
        let started = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let service = Arc::new(LeadHarnesses {
            factory: Arc::new(BlockingFactory {
                started: started.clone(),
                finish: finish.clone(),
            }),
            ..service(&dir)
        });
        let edit = {
            let service = service.clone();
            tokio::spawn(async move { service.update(vec![entry("old"), entry("new")]).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        service.state.mark_auth_blocked(0, None);
        finish.notify_one();
        edit.await.unwrap().unwrap();
        assert_eq!(
            service
                .state
                .select(0, &NativeCredentialProbe)
                .unwrap()
                .entry
                .model,
            "new"
        );
    }

    #[tokio::test]
    async fn persistence_failure_keeps_the_old_live_list() {
        let dir = crate::testsupport::TempDir::new();
        let service = service(&dir);
        std::fs::remove_file(&service.path).unwrap();
        std::fs::create_dir(&service.path).unwrap();
        assert!(matches!(
            service.update(vec![entry("new")]).await,
            Err(UpdateError::Persist(_))
        ));
        assert_eq!(
            service
                .state
                .select(0, &NativeCredentialProbe)
                .unwrap()
                .entry
                .model,
            "old"
        );
    }
}
