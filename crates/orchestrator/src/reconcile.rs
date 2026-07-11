//! reconcile — parity port of Go `internal/orchestrator/reconcile.go` (the reconcile DECISION).
//!
//! [`reconcile_actions`] maps refreshed running-issue states to [`ReconcileAction`]s (upstream
//! §8.5 Part B). The apply side — running-issue grouping, per-project refresh, stall detection, and
//! workspace cleanup — lives in [`crate::reconcile_run`]. State changes express intent about the
//! TICKET; only terminal states express intent about the RUN (INF-266): a non-terminal move (e.g. a
//! GitHub integration parking the issue in "In Review" mid-run) must not stop the worker.

use std::collections::HashSet;

use rhapsody_core::{Issue, normalize_state};

/// A reconciliation outcome for one running issue (upstream §8.5 Part B). Mirrors Go `ActionKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// Terminal state → stop the worker + clean the workspace.
    TerminateCleanup,
    /// Any non-terminal state → keep running, refresh the in-memory snapshot.
    UpdateState,
}

/// One decision produced by [`reconcile_actions`]. Mirrors Go `ReconcileAction`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconcileAction {
    pub issue_id: String,
    pub kind: ActionKind,
    /// The refreshed ticket state. Populated for BOTH kinds: [`ActionKind::UpdateState`] refreshes the
    /// in-memory snapshot from it; [`ActionKind::TerminateCleanup`] uses it to classify the terminal
    /// move as Done-type (completed) vs cancel-type (stopped). (INF-272)
    pub new_state: String,
}

/// Maps refreshed running-issue states to actions. State changes express intent about the TICKET;
/// only terminal states express intent about the RUN (INF-266) — a non-terminal move (e.g. a GitHub
/// integration parking the issue in "In Review" mid-run) must not stop the worker. `running_ids` not
/// present in `refreshed` are omitted (partial refresh → keep running). Mirrors Go `ReconcileActions`.
pub fn reconcile_actions(
    running_ids: &[String],
    refreshed: &[Issue],
    terminal: &HashSet<String>,
) -> Vec<ReconcileAction> {
    let is_running: HashSet<&str> = running_ids.iter().map(String::as_str).collect();
    let mut acts = Vec::new();
    for iss in refreshed {
        if !is_running.contains(iss.id.as_str()) {
            continue;
        }
        if terminal.contains(&normalize_state(&iss.state)) {
            acts.push(ReconcileAction {
                issue_id: iss.id.clone(),
                kind: ActionKind::TerminateCleanup,
                new_state: iss.state.clone(),
            });
            continue;
        }
        acts.push(ReconcileAction {
            issue_id: iss.id.clone(),
            kind: ActionKind::UpdateState,
            new_state: iss.state.clone(),
        });
    }
    acts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{issue, terminal_set};
    use std::collections::HashMap;

    // Mirrors Go `TestReconcileActions`: terminal → cleanup (with NewState), active/non-active
    // non-terminal → update snapshot, an absent running id → no action.
    #[test]
    fn reconcile_actions_classifies_running_issues() {
        let running = [
            "1".to_string(),
            "2".to_string(),
            "3".to_string(),
            "4".to_string(),
        ];
        let refreshed = vec![
            issue("1", "MT-1", "Done"),        // terminal → cleanup
            issue("2", "MT-2", "In Progress"), // active → update snapshot
            issue("3", "MT-3", "In Review"), // non-active non-terminal → update snapshot (INF-266)
                                             // "4" absent from refresh → no action
        ];
        let acts = reconcile_actions(&running, &refreshed, &terminal_set());

        let by_id: HashMap<&str, &ReconcileAction> =
            acts.iter().map(|a| (a.issue_id.as_str(), a)).collect();

        // NewState is populated for TerminateCleanup too, so reconcile_group can classify the
        // terminal move as Done-type (completed) vs cancel-type (stopped). (INF-272)
        assert_eq!(by_id["1"].kind, ActionKind::TerminateCleanup);
        assert_eq!(by_id["1"].new_state, "Done");
        assert_eq!(by_id["2"].kind, ActionKind::UpdateState);
        assert_eq!(by_id["2"].new_state, "In Progress");
        // A non-active, non-terminal state (e.g. a GitHub integration parking the issue in
        // "In Review" mid-run) must NOT stop the worker: it only refreshes the snapshot.
        assert_eq!(by_id["3"].kind, ActionKind::UpdateState);
        assert_eq!(by_id["3"].new_state, "In Review");
        assert!(
            !by_id.contains_key("4"),
            "absent running id must produce no action"
        );
        assert_eq!(acts.len(), 3);
    }
}
