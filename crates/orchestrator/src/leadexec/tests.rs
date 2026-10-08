use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rhapsody_agent::{
    Harness, TurnResult,
    fake::{Fake, TurnScript},
};
use rhapsody_core::Issue;
use rhapsody_store::{LeadTrigger, Sqlite, Store, StorePath};

use super::*;
use crate::leaddecision::LEAD_DECISION_TAG;

#[derive(Default)]
struct RecordingHost {
    subject: Mutex<LeadSubject>,
    calls: Mutex<Vec<String>>,
    trail: Mutex<Vec<LeadTrail>>,
    findings: Mutex<Option<String>>,
    read_err: bool,
    action_err: bool,
}

impl RecordingHost {
    fn record(&self, text: String) {
        self.calls.lock().expect("calls").push(text);
    }
}

#[async_trait]
impl LeadHost for RecordingHost {
    async fn subject(&self) -> Result<LeadSubject, String> {
        if self.read_err {
            return Err("source unavailable".into());
        }
        Ok(self.subject.lock().expect("subject").clone())
    }
    async fn prepend(&self, ticket: &Issue, text: &str) -> Result<(), String> {
        self.record(format!("prepend:{}:{text}", ticket.identifier));
        if self.action_err {
            return Err("uncertain description write".into());
        }
        Ok(())
    }
    async fn todo(&self, ticket: &Issue) -> Result<(), String> {
        self.record(format!("todo:{}", ticket.identifier));
        self.subject.lock().expect("subject").ticket.state = "Todo".into();
        Ok(())
    }
    async fn clear_review(&self, pr: &str) -> Result<(), String> {
        self.record(format!("clear:{pr}"));
        Ok(())
    }
    async fn reassign(&self, ticket: &Issue, identity: &str) -> Result<(), String> {
        self.record(format!("reassign:{}:{identity}", ticket.identifier));
        self.subject.lock().expect("subject").ticket.labels =
            Some(vec![format!("rhapsody:@{identity}")]);
        Ok(())
    }
    async fn commission(
        &self,
        _ticket: &Issue,
        question: &str,
        hypothesis: &str,
    ) -> Result<String, String> {
        self.record(format!("commission:{question}:{hypothesis}"));
        Ok("TEST-200".into())
    }
    async fn paper_trail(&self, trail: &LeadTrail) -> Result<(), String> {
        self.trail.lock().expect("trail").push(trail.clone());
        self.record(format!("ticket_line:{}", trail.text));
        self.record(format!("room:{}", trail.text));
        self.record(format!("retain:by: lead; context:{}", trail.text));
        Ok(())
    }
    async fn findings(&self, _ticket: &str, _after: &str) -> Result<Option<String>, String> {
        Ok(self.findings.lock().expect("findings").clone())
    }
}

fn setup(question: &str) -> (Arc<Sqlite>, RecordingHost, LeadCase) {
    let store = Arc::new(Sqlite::open(StorePath::InMemory).expect("store"));
    let id = store
        .enqueue_lead_item(
            &LeadTrigger::BlockedHandoff {
                ticket: "TEST-100".into(),
                question: question.into(),
            },
            "2026-10-07T12:00:00Z",
        )
        .expect("enqueue");
    let subject = LeadSubject {
        ticket: Issue {
            id: "uuid-100".into(),
            identifier: "TEST-100".into(),
            team_id: "team".into(),
            state: "In Review".into(),
            labels: Some(vec!["rhapsody:@jerry".into()]),
            ..Issue::default()
        },
        pr: Some("o/r#290".into()),
        head: "head-a".into(),
        open: true,
    };
    let case = LeadCase {
        item: store.load_lead_items().expect("items").remove(0),
        subject: subject.clone(),
        identities: vec!["jerry".into(), "alice".into()],
        evidence: "host case packet".into(),
    };
    assert_eq!(case.item.id, id);
    let host = RecordingHost {
        subject: Mutex::new(subject),
        ..Default::default()
    };
    (store, host, case)
}

async fn replay(
    store: &(dyn Store + Sync),
    host: &dyn LeadHost,
    case: &LeadCase,
    actions: serde_json::Value,
) -> LeadResult {
    let text = format!(
        "```{LEAD_DECISION_TAG}\n{}\n```",
        serde_json::json!({"actions": actions})
    );
    let mut fake = Fake::new();
    fake.turns = vec![TurnScript {
        result: TurnResult {
            result_text: text,
            ..Default::default()
        },
        ..Default::default()
    }];
    let session = fake
        .start_manager_session(
            rhapsody_agent::manager::ManagerSessionStart {
                model: "openai/gpt-test".into(),
                effort: "high".into(),
                cwd: String::new(),
                config_dir: String::new(),
                run_timeout_ms: 1000,
                model_credential: None,
            },
            case.subject.ticket.clone(),
            None,
        )
        .expect("fake manager");
    let prompt = lead_live_prompt(&case.evidence);
    let (result, error) = session.run_turn(&prompt, None, None, &|_| {}).await;
    assert!(error.is_none(), "scripted turn: {error:?}");
    assert!(fake.last_prompt().contains(LEAD_DECISION_TAG));
    assert!(!fake.last_prompt().contains("rhapsody-manager-decision"));
    execute(
        store,
        host,
        case,
        &result.result_text,
        "opencode",
        "openai/gpt-test",
    )
    .await
    .expect("execute")
}

#[tokio::test]
async fn replay_290_requeue() {
    let (store, host, case) = setup("three 401s and zero review verdicts");
    let result = replay(
        store.as_ref(),
        &host,
        &case,
        serde_json::json!([
            {"action":"requeue","ticket":"TEST-100"}, {"action":"clear_review","pr":"o/r#290"}
        ]),
    )
    .await;
    assert_eq!(result.state, "done");
    assert_eq!(
        &host.calls.lock().expect("calls")[..2],
        &["clear:o/r#290", "todo:TEST-100"]
    );
}

#[tokio::test]
async fn replay_294_commission() {
    let (store, host, case) = setup("canary UnknownError needs network/login diagnosis");
    let result = replay(store.as_ref(), &host, &case, serde_json::json!([
        {"action":"commission","kind":"ticket","question":"why does the canary fail?","hypothesis":"cold model cache"}
    ])).await;
    assert_eq!(result.state, "parked");
    assert_eq!(result.commission_ticket.as_deref(), Some("TEST-200"));
    assert!(host.calls.lock().expect("calls")[0].starts_with("commission:"));
}

#[tokio::test]
async fn replay_297_route_back_with_credential_rule() {
    let (store, host, case) = setup("Event shape and permitted credential source?");
    let result = replay(store.as_ref(), &host, &case, serde_json::json!([
        {"action":"route_back","ticket":"TEST-100","answer":"Use additive LimitObs; use a refresh-blank copy."},
        {"action":"authorize_credential","ticket":"TEST-100","rule":"openai-refresh-blank-copy"}
    ])).await;
    assert_eq!(result.state, "done");
    let calls = host.calls.lock().expect("calls");
    assert!(
        calls[0].contains("refresh: \"\""),
        "credential instructions must land before the ticket can dispatch"
    );
    assert!(calls.iter().any(|s| s.contains("LimitObs")));
    assert!(calls.iter().any(|s| s.contains("refresh: \"\"")));
    assert!(calls.iter().any(|s| s == "todo:TEST-100"));
    assert!(calls.iter().any(|s| s == "clear:o/r#290"));
}

#[tokio::test]
async fn replay_mh3_route_back() {
    let (store, host, case) = setup("permission tail exception?");
    replay(store.as_ref(), &host, &case, serde_json::json!([
        {"action":"route_back","ticket":"TEST-100","answer":"Allow only the private tool-output directory; retain the canary."}
    ])).await;
    assert!(host.calls.lock().expect("calls")[0].contains("private tool-output"));
}

#[tokio::test]
async fn replay_l1_requeue() {
    let (store, host, mut case) = setup("In Review without a PR");
    case.subject.pr = None;
    host.subject.lock().expect("subject").pr = None;
    replay(
        store.as_ref(),
        &host,
        &case,
        serde_json::json!([{"action":"requeue","ticket":"TEST-100"}]),
    )
    .await;
    assert_eq!(host.calls.lock().expect("calls")[0], "todo:TEST-100");
}

#[tokio::test]
async fn replay_flux87_escalate() {
    let (store, host, case) = setup("needs a B2 console key for flux#87");
    let result = replay(store.as_ref(), &host, &case, serde_json::json!([
        {"action":"escalate","need":"Operator must mint the B2 console key; cluster deploy needs operator approval."}
    ])).await;
    assert_eq!(result.state, "done");
    assert!(result.escalation.as_deref().expect("need").contains("B2"));
    assert!(
        host.calls
            .lock()
            .expect("calls")
            .iter()
            .all(|s| !s.starts_with("todo:") && !s.starts_with("prepend:"))
    );
}

#[tokio::test]
async fn never_merges_or_pushes() {
    for action in ["merge", "push", "commit", "spend_credits", "shell"] {
        let (store, host, case) = setup("malicious action");
        let result = replay(
            store.as_ref(),
            &host,
            &case,
            serde_json::json!([{"action":action}]),
        )
        .await;
        assert!(result.escalation.is_some(), "{action}");
        assert!(
            host.calls
                .lock()
                .expect("calls")
                .iter()
                .all(|s| s.starts_with("ticket_line:")
                    || s.starts_with("room:")
                    || s.starts_with("retain:"))
        );
    }
}

#[tokio::test]
async fn second_route_back_refused() {
    let (store, host, mut case) = setup("which representation?");
    let actions =
        serde_json::json!([{"action":"route_back","ticket":"TEST-100","answer":"Use LimitObs."}]);
    replay(store.as_ref(), &host, &case, actions.clone()).await;
    case.subject = host.subject.lock().expect("subject").clone();
    case.item = store.load_lead_items().expect("items").remove(0);
    host.calls.lock().expect("calls").clear();
    let result = replay(store.as_ref(), &host, &case, actions).await;
    assert!(
        result
            .escalation
            .as_deref()
            .expect("refusal")
            .contains("commission or escalate")
    );
    assert!(
        !host
            .calls
            .lock()
            .expect("calls")
            .iter()
            .any(|s| s.starts_with("prepend:") || s.starts_with("todo:"))
    );
    assert_eq!(
        store.load_lead_items().expect("items")[0].attempts_on_question,
        1
    );
}

#[tokio::test]
async fn stale_subject_rechecked_before_action() {
    for change in ["moved", "closed", "relabeled", "head"] {
        let (store, host, case) = setup("stale question");
        {
            let mut subject = host.subject.lock().expect("subject");
            match change {
                "moved" => subject.ticket.state = "In Progress".into(),
                "closed" => subject.open = false,
                "relabeled" => subject.ticket.labels = Some(vec!["rhapsody:@alice".into()]),
                _ => subject.head = "head-b".into(),
            }
        }
        let result = replay(
            store.as_ref(),
            &host,
            &case,
            serde_json::json!([{"action":"requeue","ticket":"TEST-100"}]),
        )
        .await;
        assert_eq!(result.state, "queued", "{change}");
        assert!(
            !host
                .calls
                .lock()
                .expect("calls")
                .iter()
                .any(|s| s.starts_with("todo:")),
            "{change}"
        );
    }
}

#[tokio::test]
async fn unknown_action_or_rule_rejected() {
    for actions in [
        serde_json::json!([{"action":"approve"}]),
        serde_json::json!([
            {"action":"route_back","ticket":"TEST-100","answer":"an answer"},
            {"action":"authorize_credential","ticket":"TEST-100","rule":"copy-refresh-capable"}
        ]),
    ] {
        let (store, host, case) = setup("invalid block");
        let result = replay(store.as_ref(), &host, &case, actions).await;
        assert!(result.escalation.is_some());
        assert!(
            !host
                .calls
                .lock()
                .expect("calls")
                .iter()
                .any(|s| s.starts_with("prepend:") || s.starts_with("todo:"))
        );
    }
}

#[tokio::test]
async fn paper_trail_complete() {
    let (store, host, case) = setup("answer needed");
    let token = rhapsody_core::SUMMON_TOKEN_SYMPHONY;
    replay(
        store.as_ref(),
        &host,
        &case,
        serde_json::json!([
            {"action":"route_back","ticket":"TEST-100","answer":format!("Answer {token}")}
        ]),
    )
    .await;
    let rows = store.load_lead_decisions().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].item, case.item.id);
    assert_eq!(rows[0].harness, "opencode");
    assert_eq!(rows[0].model, "openai/gpt-test");
    assert!(rows[0].evidence.contains("host case packet"));
    assert!(rows[0].actions.contains("route_back"));
    assert_eq!(
        store
            .lead_digest_entries("2026-01-01")
            .expect("digest")
            .len(),
        1
    );
    let calls = host.calls.lock().expect("calls");
    for prefix in ["ticket_line:", "room:", "retain:by: lead; context:"] {
        assert!(calls.iter().any(|s| s.starts_with(prefix)), "{prefix}");
    }
    assert!(
        calls
            .iter()
            .all(|s| !s.contains(token) && !s.contains(rhapsody_core::SUMMON_TOKEN_RHAPSODY))
    );
}

#[tokio::test]
async fn commission_parks_and_resumes_on_findings() {
    let (store, host, case) = setup("diagnose the cold model cache");
    replay(store.as_ref(), &host, &case, serde_json::json!([
        {"action":"commission","kind":"ticket","question":"diagnose only","hypothesis":"cold cache"}
    ])).await;
    assert_eq!(store.load_lead_items().expect("items")[0].state, "parked");
    assert!(
        !resume_on_findings(store.as_ref(), &host, case.item.id)
            .await
            .expect("no findings")
    );
    *host.findings.lock().expect("findings") =
        Some("Cold cache reproduces UnknownError; warm catalogue fixes it.".into());
    assert!(
        resume_on_findings(store.as_ref(), &host, case.item.id)
            .await
            .expect("resume")
    );
    assert_eq!(store.load_lead_items().expect("items")[0].state, "queued");
    assert!(
        store
            .lead_execution(case.item.id)
            .expect("execution")
            .expect("row")
            .findings
            .contains("warm catalogue")
    );
}

#[test]
fn lead_block_is_closed_and_duplicate_fields_never_grant_authority() {
    for json in [
        r#"{"actions":[{"action":"requeue","ticket":"TEST-100","ticket":"OTHER-1"}]}"#,
        r#"{"actions":[{"action":"merge","action":"requeue","ticket":"TEST-100"}]}"#,
        r#"{"actions":[{"action":"requeue","ticket":"TEST-100","answer":null}]}"#,
        r#"{"actions":[{"action":"route_back","ticket":"TEST-100","answer":""}]}"#,
        r#"{"actions":[{"action":"route_back","ticket":"TEST-100","answer":"sk-ant-api03-abcdefghijklmno"}]}"#,
        r#"{"actions":[],"actions":[{"action":"requeue","ticket":"TEST-100"}]}"#,
    ] {
        let text = format!("```{LEAD_DECISION_TAG}\n{json}\n```");
        assert!(
            crate::leaddecision::parse_lead_decision(&text).is_err(),
            "{json}"
        );
    }
}

#[tokio::test]
async fn failed_reads_and_uncertain_writes_leave_a_durable_escalation() {
    for read_err in [false, true] {
        let (store, mut host, case) = setup("failure question");
        host.read_err = read_err;
        host.action_err = !read_err;
        let result = replay(
            store.as_ref(),
            &host,
            &case,
            serde_json::json!([
                {"action":"route_back","ticket":"TEST-100","answer":"an answer"}
            ]),
        )
        .await;
        assert!(result.escalation.is_some());
        assert_eq!(store.load_lead_items().expect("items")[0].state, "done");
        assert!(
            store.load_lead_decisions().expect("rows")[0]
                .decision
                .starts_with("escalate:")
        );
        assert!(
            !host
                .calls
                .lock()
                .expect("calls")
                .iter()
                .any(|s| s.starts_with("todo:"))
        );
        let trigger = case.item.trigger.clone();
        store
            .enqueue_lead_item(&trigger, "2026-10-08T00:00:00Z")
            .expect("dedupe");
        assert_eq!(
            store.load_lead_items().expect("items")[0].state,
            "done",
            "an escalation must not be recreated by another poll"
        );
    }
}

#[tokio::test]
async fn interrupted_effects_never_replay_and_other_subjects_are_refused() {
    let (store, host, case) = setup("interrupted commission");
    store
        .save_lead_decision(&rhapsody_store::LeadDecisionRow {
            item: case.item.id,
            decision: "applying".into(),
            ..Default::default()
        })
        .expect("interrupted record");
    let result = replay(
        store.as_ref(),
        &host,
        &case,
        serde_json::json!([
            {"action":"commission","kind":"ticket","question":"diagnose","hypothesis":"cold cache"}
        ]),
    )
    .await;
    assert!(
        result
            .escalation
            .as_deref()
            .expect("need")
            .contains("uncertain")
    );
    assert!(
        !host
            .calls
            .lock()
            .expect("calls")
            .iter()
            .any(|s| s.starts_with("commission:"))
    );
    let (store, host, case) = setup("cross installation");
    for actions in [
        serde_json::json!([{"action":"route_back","ticket":"OTHER-1","answer":"answer"}]),
        serde_json::json!([{"action":"clear_review","pr":"other/repo#1"}]),
        serde_json::json!([{"action":"reassign","ticket":"TEST-100","identity":"stranger"}]),
    ] {
        assert!(
            replay(store.as_ref(), &host, &case, actions)
                .await
                .escalation
                .is_some()
        );
    }
}

#[tokio::test]
async fn commission_author_does_not_consume_a_second_route_back() {
    let (store, host, mut case) = setup("same question again");
    replay(
        store.as_ref(),
        &host,
        &case,
        serde_json::json!([{"action":"route_back","ticket":"TEST-100","answer":"answer"}]),
    )
    .await;
    case.subject = host.subject.lock().expect("subject").clone();
    let result = replay(
        store.as_ref(),
        &host,
        &case,
        serde_json::json!([
            {"action":"commission","kind":"author","question":"diagnose","hypothesis":"cold cache"}
        ]),
    )
    .await;
    assert_eq!(result.state, "parked");
    assert_eq!(result.commission_ticket.as_deref(), Some("TEST-100"));
    assert_eq!(
        store.load_lead_items().expect("items")[0].attempts_on_question,
        1
    );
    assert!(
        host.calls
            .lock()
            .expect("calls")
            .iter()
            .any(|s| s.contains("Diagnose, do not fix") && s.contains("TEST-100-findings.md"))
    );
}

#[tokio::test]
async fn reassignment_and_fireworks_authorization_have_only_the_named_effects() {
    let (store, host, case) = setup("healthy engine needed");
    let result = replay(
        store.as_ref(),
        &host,
        &case,
        serde_json::json!([
            {"action":"reassign","ticket":"TEST-100","identity":"alice"},
            {"action":"authorize_credential","ticket":"TEST-100","rule":"fireworks-key-measurement"}
        ]),
    )
    .await;
    assert!(result.escalation.is_none());
    let calls = host.calls.lock().expect("calls");
    assert_eq!(calls[0], "reassign:TEST-100:alice");
    assert!(calls[1].contains("within configured USD caps"));
    assert!(
        !calls
            .iter()
            .any(|s| s.starts_with("todo:") || s.starts_with("clear:"))
    );
}

struct ValidLogin;
impl crate::managerselftest::EntryCredentialProbe for ValidLogin {
    fn status(
        &self,
        _: &rhapsody_config::teams::ManagerHarnessEntry,
        _: i64,
    ) -> crate::managerselftest::CredentialStatus {
        crate::managerselftest::CredentialStatus::Valid {
            expires_in_ms: 100_000_000,
        }
    }
    fn fingerprint(&self, _: &rhapsody_config::teams::ManagerHarnessEntry) -> Option<String> {
        Some("fake-login".into())
    }
}

#[test]
fn lead_launch_reuses_selftest_ordered_fallback_and_generation_budget() {
    use crate::managerselftest::{SelfTestRecord, SelfTestVerdict};
    use crate::testsupport::{empty_effective, empty_resolved_project};
    use rhapsody_config::teams::{
        Identity, ManagerHarnessEntry, ReviewAuthority, ReviewMode, Teams,
    };
    let (store, _, case) = setup("which event shape?");
    store
        .save_lead_execution(&rhapsody_store::LeadExecution {
            item: case.item.id,
            ..Default::default()
        })
        .expect("execution");
    let tracker = Arc::new(rhapsody_tracker::fake::Fake::new());
    let mut eff = empty_effective(tracker.clone());
    let mut project = empty_resolved_project("proj", tracker);
    project.repo = "https://github.com/o/r.git".into();
    project.mcfg.claude.command = "/bin/echo 9.9.9".into();
    project.mcfg.opencode.command = "/bin/echo 9.9.9".into();
    eff.projects.push(project);
    eff.max_concurrent = 10;
    let mut o = crate::Orchestrator::new("WORKFLOW.md");
    o.eff = Some(eff);
    o.set_store(store.clone());
    let mut teams = Teams::disabled();
    teams.enabled = true;
    teams.manager.lead.enabled = true;
    teams.manager.review_authority = ReviewAuthority::Act;
    teams.review.mode = ReviewMode::Ticketless;
    teams.manager.harnesses = vec![
        ManagerHarnessEntry {
            harness: "opencode".into(),
            model: "openai/gpt-test".into(),
            effort: "high".into(),
        },
        ManagerHarnessEntry {
            harness: "claude".into(),
            model: "claude-test".into(),
            effort: "high".into(),
        },
    ];
    teams.roster.push(Identity {
        name: "jerry".into(),
        profile: "swe".into(),
        ..Default::default()
    });
    o.manager_selftest
        .configure(teams.manager.effective_harnesses());
    o.manager_selftest
        .set_credential_probe(Arc::new(ValidLogin));
    o.teams = Some(teams);
    let dispatched = Arc::new(Mutex::new(Vec::new()));
    let sink = dispatched.clone();
    o.spawn = Some(Box::new(move |_, _, re| {
        sink.lock().expect("spawn").push(re.clone());
    }));
    let run = crate::managerrun::ManagerRun {
        lead_item: Some(case.item.id),
        owner: "o".into(),
        repo: "r".into(),
        number: 290,
        repo_url: "https://github.com/o/r.git".into(),
        case_packet: case.evidence.clone(),
        ..Default::default()
    };
    assert!(matches!(
        o.dispatch_manager(run.clone()),
        crate::managerrun::ManagerDispatchOutcome::SelfTestFailed(_)
    ));
    assert_eq!(
        store
            .lead_execution(case.item.id)
            .expect("execution")
            .expect("row")
            .run_attempts,
        0
    );
    let version = crate::managerselftest::probe_cli_version("/bin/echo 9.9.9").expect("version");
    for index in 0..2 {
        o.manager_selftest.record_entry(
            index,
            SelfTestRecord {
                cli_version: version.clone(),
                verdict: SelfTestVerdict::Passed,
            },
        );
    }
    o.handle_lead_prepared(case.item.id, Ok(Some((case.clone(), run.clone()))));
    assert_eq!(dispatched.lock().expect("spawn")[0].harness, "opencode");
    assert!(store.load_review_watch().expect("watch").is_empty());
    o.pump_manager_interventions();
    assert!(
        o.manager_attempts.contains_key(&run.key()),
        "a manager sweep must preserve the active lead's cursor"
    );
    let re = o.running.get(&run.key()).expect("running").clone();
    o.on_worker_exit(crate::retry::EvWorkerExit {
        issue_id: run.key(),
        started_at: re.started_at,
        failed: true,
        auth_needed: true,
        err_msg: "turn_failed".into(),
        last_state: String::new(),
        declared_handoff: false,
        review_verdict: None,
        manager_text: None,
        refused: false,
    });
    let spawned = dispatched.lock().expect("spawn");
    assert_eq!(spawned.len(), 2);
    assert_eq!(spawned[1].harness, "claude");
    assert_eq!(spawned[1].model_override.model, "claude-test");
    drop(spawned);
    assert_eq!(
        store
            .manager_budget("o/r#290")
            .expect("budget")
            .expect("row")
            .runs_used,
        2
    );
    assert_eq!(
        store
            .lead_execution(case.item.id)
            .expect("execution")
            .expect("row")
            .run_attempts,
        2
    );
    assert!(o.lead_cases.contains_key(&run.key()));
}

#[test]
fn lead_route_reservation_and_paper_trail_survive_restart() {
    let dir = crate::testsupport::TempDir::new();
    let path = dir.child("lead.db");
    let id;
    {
        let store = Sqlite::open(StorePath::Disk(path.clone().into())).expect("store");
        id = store
            .enqueue_lead_item(
                &LeadTrigger::BlockedHandoff {
                    ticket: "TEST-100".into(),
                    question: "same question".into(),
                },
                "2026-10-07",
            )
            .expect("enqueue");
        assert!(store.reserve_lead_route_back(id).expect("first"));
        store
            .save_lead_decision(&rhapsody_store::LeadDecisionRow {
                item: id,
                decision: "done: route_back".into(),
                at: "2026-10-07".into(),
                ..Default::default()
            })
            .expect("row");
        store.set_lead_item_state(id, "done").expect("done");
    }
    let store = Sqlite::open(StorePath::Disk(path.into())).expect("reopen");
    assert!(!store.reserve_lead_route_back(id).expect("second"));
    assert_eq!(
        store
            .lead_digest_entries("2026-10-06")
            .expect("digest")
            .len(),
        1
    );
    assert!(
        store
            .lead_digest_entries("2026-10-08")
            .expect("digest")
            .is_empty()
    );
    let id2 = store
        .enqueue_lead_item(
            &LeadTrigger::BlockedHandoff {
                ticket: "TEST-100".into(),
                question: "same   question".into(),
            },
            "2026-10-08",
        )
        .expect("repeat");
    assert_eq!(id2, id);
    assert_eq!(store.load_lead_items().expect("items")[0].state, "queued");
    assert_eq!(
        store.load_lead_items().expect("items")[0].attempts_on_question,
        1
    );
}

#[tokio::test]
async fn real_tracker_writes_and_findings_use_only_the_scoped_subject() {
    use rhapsody_config::{
        memory::LocalBank,
        room::{Cursor, LocalRoom},
    };
    use rhapsody_tracker::Tracker;
    let dir = crate::testsupport::TempDir::new();
    let path = dir.child("issues.json");
    std::fs::write(&path, r#"{"issues":[{"id":"uuid-100","identifier":"TEST-100","team_id":"team","state":"In Review","description":"Original acceptance.","labels":["rhapsody:@jerry"]},{"id":"uuid-101","identifier":"TEST-101","state":"Todo","description":"Do not touch."}]}"#).expect("fixture");
    let tracker = Arc::new(rhapsody_tracker::file::new(
        rhapsody_tracker::file::Config {
            source: path.clone(),
            ..Default::default()
        },
    ));
    let (store, _, _) = setup("real tracker question");
    let mut o = crate::Orchestrator::new("WORKFLOW.md");
    o.set_store(store.clone());
    let mut control = o.control();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    control.events = tx;
    let task = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let crate::Event::LeadAllowed { reply, .. } = event {
                let _ = reply.send(true);
            }
        }
    });
    let room = Arc::new(LocalRoom::new(dir.child("room")));
    let bank = Arc::new(LocalBank::new(dir.child("banks"), "agent-"));
    let runtime = LeadRuntime {
        control,
        store,
        projects: Vec::new(),
        teams: rhapsody_config::teams::Teams::disabled(),
        prs: Arc::new(crate::ghsummons::GH::new("", None)),
        room: Some(room.clone()),
        memory: Some(bank.clone()),
        findings_dir: Some(dir.path.clone().into()),
    };
    let project = LeadProject {
        tracker: tracker.clone(),
        repo_url: "https://github.com/o/r.git".into(),
        terminal_states: ["done".into()].into_iter().collect(),
        summon_token: "@custom-bot".into(),
    };
    let host = RuntimeHost {
        runtime: &runtime,
        project: &project,
        ticket: "TEST-100".into(),
        pr: None,
    };
    let snapshot = host.subject().await.expect("fresh snapshot");
    host.prepend(&snapshot.ticket, "Answer @custom-bot")
        .await
        .expect("prepend");
    host.reassign(&snapshot.ticket, "alice")
        .await
        .expect("reassign");
    host.todo(&snapshot.ticket).await.expect("todo");
    let issue = tracker
        .fetch_issue_by_identifier("TEST-100")
        .await
        .expect("read")
        .expect("issue");
    assert_eq!(issue.state, "Todo");
    assert_eq!(issue.labels, Some(vec!["rhapsody:@alice".into()]));
    assert_eq!(
        issue.description.as_deref(),
        Some("## Lead answer\nAnswer \n\nOriginal acceptance.")
    );
    assert_eq!(
        tracker
            .fetch_issue_by_identifier("TEST-101")
            .await
            .expect("read")
            .expect("issue")
            .description
            .as_deref(),
        Some("Do not touch.")
    );
    let result = host
        .paper_trail(&LeadTrail {
            text: "Decided @custom-bot".into(),
            decision_id: 1,
            ticket: issue,
        })
        .await;
    assert!(
        result
            .expect_err("file tracker has no comment writes")
            .contains("ticket line")
    );
    assert_eq!(
        room.read_since("jerry", &Cursor::default(), 10)
            .expect("room")
            .messages[0]
            .body,
        "Decided "
    );
    assert!(std::path::Path::new(&dir.child("banks/operator-decisions")).is_dir());
    let old = "2000-01-01T00:00:00Z";
    assert!(
        host.findings("TEST-200", old)
            .await
            .expect("absent")
            .is_none()
    );
    let file = dir.child("TEST-200-findings.md");
    std::fs::write(&file, "verified diagnosis").expect("findings");
    assert_eq!(
        host.findings("TEST-200", old)
            .await
            .expect("findings")
            .as_deref(),
        Some("verified diagnosis")
    );
    assert!(
        host.findings("TEST-200", "2999-01-01T00:00:00Z")
            .await
            .expect("old file")
            .is_none()
    );
    assert!(host.findings("../outside", old).await.is_err());
    std::fs::remove_file(&file).expect("remove");
    std::os::unix::fs::symlink(&path, &file).expect("symlink");
    assert!(host.findings("TEST-200", old).await.is_err());
    task.abort();
}
