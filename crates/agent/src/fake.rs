//! A scriptable [`Runner`](crate::Runner)/[`Session`](crate::Session) for tests — the parity port
//! of Go `internal/agent/fake`. P5's in-process agent test double.
//!
//! Programmable inputs (`thread_id_value`, `turns`, `start_err`) are plain fields set directly by
//! tests, exactly as Go's `fake.Runner` exposes them. State the methods *mutate* (the start-call
//! counter, the last prompt/transcript) lives behind a single [`Mutex`] shared with the sessions
//! (Go's `session.runner` back-pointer writes `LastPrompt`), because the trait methods take `&self`.
//! That recorded state is read back through the accessor methods (`start_calls()`, `last_prompt()`,
//! …) rather than Go's public fields.

use crate::{AgentError, Event, Runner, Session, TURN_SUCCEEDED, Transcript, TurnResult};
use async_trait::async_trait;
use rhapsody_core::Issue;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::sync::mpsc;

/// One turn's emitted events and outcome. The mirror of Go's `fake.TurnScript`: `run_turn` emits
/// `events` in order via `on_event`, then returns `(result, err)`.
#[derive(Debug, Clone, Default)]
pub struct TurnScript {
    pub events: Vec<Event>,
    pub result: TurnResult,
    pub err: Option<AgentError>,
}

/// A scriptable fake agent backend.
pub struct Fake {
    /// The stable thread id every started session reports; defaults to `"thread-fake"`.
    pub thread_id_value: String,
    /// The per-turn scripts, consumed in order by each session's `run_turn`.
    pub turns: Vec<TurnScript>,
    /// When set, returned by `start_session` (the call is still recorded).
    pub start_err: Option<AgentError>,
    /// Recorded state mutated by the `&self` methods, shared with started sessions.
    recorded: Arc<Mutex<Recorded>>,
}

/// Recorded state guarded by [`Fake::recorded`] and shared with each [`FakeSession`].
#[derive(Default)]
struct Recorded {
    start_calls: i64,
    /// The prompt text of the most recent `run_turn` (turn 1 is the rendered task prompt), so tests
    /// can assert which prompt source/template the worker fed the agent.
    last_prompt: String,
    /// The transcript passed to the most recent `start_session` (Go's `LastTranscript`). Held so
    /// tests can observe whether the worker wired transcript capture; the writers are opaque.
    last_transcript: Option<Transcript>,
    /// The run id the most recent session was given via [`Session::set_run_id`], or `None` when the
    /// caller never set one. Recorded so a test can assert the worker threads the store run row's
    /// id onto the session — the env the agent's `teams_post`/`teams_retain` resolve from
    /// (STUDIO-675).
    last_run_id: Option<i64>,
    /// The review head SHA the most recent session was given via [`Session::set_review_head`], or
    /// `None` when the caller never set one (every non-review run). Recorded so a test can assert
    /// the worker pins the dispatched head onto the session exactly once (STUDIO-715).
    last_review_head: Option<String>,
}

impl Fake {
    /// Returns an empty fake Runner (mirror of Go `fake.New()`).
    pub fn new() -> Self {
        Self {
            thread_id_value: "thread-fake".to_string(),
            turns: Vec::new(),
            start_err: None,
            recorded: Arc::new(Mutex::new(Recorded::default())),
        }
    }

    /// Number of `start_session` calls.
    pub fn start_calls(&self) -> i64 {
        self.lock().start_calls
    }

    /// The prompt of the most recent `run_turn`.
    pub fn last_prompt(&self) -> String {
        self.lock().last_prompt.clone()
    }

    /// Whether the most recent `start_session` received a transcript (Go's `LastTranscript != nil`).
    pub fn last_transcript_present(&self) -> bool {
        self.lock().last_transcript.is_some()
    }

    /// The run id the most recent session was given via [`Session::set_run_id`]; `None` when the
    /// caller never called it (STUDIO-675).
    pub fn last_run_id(&self) -> Option<i64> {
        self.lock().last_run_id
    }

    /// The review head SHA the most recent session was given via [`Session::set_review_head`];
    /// `None` when the caller never called it (STUDIO-715).
    pub fn last_review_head(&self) -> Option<String> {
        self.lock().last_review_head.clone()
    }

    fn lock(&self) -> MutexGuard<'_, Recorded> {
        // Recover a poisoned lock rather than panic: the fake is library (non-test-cfg) code.
        self.recorded.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for Fake {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Runner for Fake {
    async fn start_session(
        &self,
        _workspace_path: &str,
        _issue: Issue,
        transcript: Option<Transcript>,
    ) -> Result<Box<dyn Session>, AgentError> {
        {
            let mut rec = self.lock();
            rec.start_calls += 1;
            rec.last_transcript = transcript;
        }
        if let Some(e) = &self.start_err {
            return Err(e.clone());
        }
        Ok(Box::new(FakeSession {
            thread_id: self.thread_id_value.clone(),
            turns: self.turns.clone(),
            turn_n: AtomicI64::new(0),
            recorded: Arc::clone(&self.recorded),
        }))
    }
}

/// A live fake session. `turn_n` is interior-mutable (the trait's `run_turn`/`id` take `&self`); it
/// starts at 0 and `run_turn` advances it, so `id()` reports `<thread>-<n>` — e.g. `<thread>-1`
/// after one turn (the mirror of Go's `s.turnN++`).
struct FakeSession {
    thread_id: String,
    turns: Vec<TurnScript>,
    turn_n: AtomicI64,
    recorded: Arc<Mutex<Recorded>>,
}

#[async_trait]
impl Session for FakeSession {
    fn id(&self) -> String {
        format!("{}-{}", self.thread_id, self.turn_n.load(Ordering::SeqCst))
    }

    fn thread_id(&self) -> String {
        self.thread_id.clone()
    }

    fn set_run_id(&self, id: i64) {
        self.recorded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last_run_id = Some(id);
    }

    fn set_review_head(&self, sha: &str) {
        self.recorded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last_review_head = Some(sha.to_string());
    }

    async fn run_turn(
        &self,
        prompt: &str,
        _attempt: Option<i64>,
        _messages: Option<&mut mpsc::Receiver<String>>,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> (TurnResult, Option<AgentError>) {
        self.recorded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last_prompt = prompt.to_string();

        let idx = self.turn_n.fetch_add(1, Ordering::SeqCst) as usize;
        if idx >= self.turns.len() {
            // No script for this turn: succeed with no events.
            return (
                TurnResult {
                    status: TURN_SUCCEEDED.to_string(),
                    ..Default::default()
                },
                None,
            );
        }
        let ts = &self.turns[idx];
        for e in &ts.events {
            on_event(e.clone());
        }
        (ts.result.clone(), ts.err.clone())
    }

    async fn stop(&self) -> Result<(), AgentError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EVENT_NOTIFICATION, EVENT_SESSION_STARTED, Usage};
    use std::sync::Mutex as StdMutex;

    // Mirrors Go `fake.TestFakeImplementsRunner`: compile-time interface check.
    #[test]
    fn fake_implements_runner() {
        let _: &dyn Runner = &Fake::new();
    }

    // Mirrors Go `fake.TestFakeSessionEmitsScriptedEventsAndResult`.
    #[tokio::test]
    async fn fake_session_emits_scripted_events_and_result() {
        let mut f = Fake::new();
        f.thread_id_value = "thread-1".to_string();
        f.turns = vec![TurnScript {
            events: vec![
                Event {
                    event_type: EVENT_SESSION_STARTED.to_string(),
                    ..Default::default()
                },
                Event {
                    event_type: EVENT_NOTIFICATION.to_string(),
                    message: "working".to_string(),
                    ..Default::default()
                },
            ],
            result: TurnResult {
                status: TURN_SUCCEEDED.to_string(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                    ..Default::default()
                },
                ..Default::default()
            },
            err: None,
        }];

        let sess = f
            .start_session(
                "/ws/MT-1",
                Issue {
                    id: "1".into(),
                    identifier: "MT-1".into(),
                    ..Default::default()
                },
                None,
            )
            .await
            .expect("start session");

        // The callback captures its events through a Mutex (the trait's `on_event` is `Fn`, and the
        // future must be `Send`), the mirror of Go's `got = append(got, e.Type)` closure.
        let got = StdMutex::new(Vec::<String>::new());
        let (res, err) = sess
            .run_turn("do it", None, None, &|e: Event| {
                got.lock().expect("lock got").push(e.event_type);
            })
            .await;

        assert!(err.is_none(), "unexpected error: {err:?}");
        assert_eq!(res.status, TURN_SUCCEEDED, "result = {res:?}");
        assert_eq!(res.usage.total_tokens, 15, "result = {res:?}");

        let got = got.into_inner().expect("into inner");
        assert_eq!(got.len(), 2, "events = {got:?}");
        assert_eq!(got[0], EVENT_SESSION_STARTED, "events = {got:?}");

        assert_eq!(sess.thread_id(), "thread-1");
        assert_eq!(sess.id(), "thread-1-1");
    }

    // Mirrors Go `fake.TestFakeTurnFailureReturnsError`.
    #[tokio::test]
    async fn fake_turn_failure_returns_error() {
        let mut f = Fake::new();
        f.turns = vec![TurnScript {
            result: TurnResult {
                status: crate::TURN_FAILED.to_string(),
                ..Default::default()
            },
            err: Some(AgentError::Other("boom".to_string())),
            ..Default::default()
        }];
        let sess = f
            .start_session("/ws", Issue::default(), None)
            .await
            .expect("start session");
        let (_, err) = sess.run_turn("p", None, None, &|_e: Event| {}).await;
        assert!(err.is_some(), "expected turn error");
    }

    // Mirrors Go `fake.TestFakeStartSessionError`.
    #[tokio::test]
    async fn fake_start_session_error() {
        let mut f = Fake::new();
        f.start_err = Some(AgentError::Other("nope".to_string()));
        let res = f.start_session("/ws", Issue::default(), None).await;
        assert!(res.is_err(), "expected start error");
    }
}
