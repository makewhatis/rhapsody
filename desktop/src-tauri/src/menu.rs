//! Maps the supervisor's status into the menu-bar (tray) item's rendered model. Parity port of
//! `$REF/desktop/internal/tray/menu.go`: the mapping is pure and unit-tested; the actual tray
//! rendering lives in the app glue ([`crate::app`] + the bin's `tray` module), which calls
//! [`menu_from_status`] and applies the result to the tray item and menu.
//!
//! Brand note: the Go reference renders the upstream vendor's "Symphony —" status prefixes + tooltip;
//! this Rust port is REBRANDED to "Rhapsody" (matching D1's window title / header / footer and the
//! P7-D5 brand guard, which purges the old vendor tokens). The behavioral contract `menu_test.go`
//! pins — which state word / agent summary shows and which actions are live — is preserved exactly.

use crate::supervisor::{State, Status};

/// The tray item's rendered state derived from the supervisor status: a status line + tooltip and
/// which actions are enabled. The glue uses `can_start`/`can_stop`/`can_open` to enable/disable the
/// corresponding menu items. Mirrors Go `tray.MenuModel`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MenuModel {
    pub status_text: String,
    pub tooltip: String,
    pub can_start: bool,
    pub can_stop: bool,
    pub can_restart: bool,
    pub can_open: bool,
}

/// Derives the tray model from the daemon status, the active agent count (from the daemon's
/// `/api/v1/state`), and whether a WORKFLOW.md is configured. It is the single source of truth for
/// what the menu-bar item shows and which controls are live in each state. Start is only offered when
/// configured, matching [`crate::app::App::start_daemon`] (which rejects an unconfigured start) and
/// the web UI — so the tray never enables an action that cannot succeed. Mirrors Go `MenuFromStatus`.
pub fn menu_from_status(st: &Status, agent_count: i64, configured: bool) -> MenuModel {
    let mut m = MenuModel {
        tooltip: st.last_err.clone(),
        ..MenuModel::default()
    };
    match st.state {
        State::Stopped => {
            m.status_text = "Rhapsody — Stopped".to_string();
            // Start is only offered when configured — the tray must not enable an action that
            // start_daemon would reject (no WORKFLOW.md to run).
            m.can_start = configured;
        }
        State::Starting => {
            m.status_text = "Rhapsody — Starting…".to_string();
            m.can_stop = true;
            m.can_restart = true;
        }
        State::Running => {
            m.status_text = format!("Rhapsody — Running ({})", agent_summary(agent_count));
            m.can_stop = true;
            m.can_restart = true;
            m.can_open = true;
        }
    }
    m
}

/// Renders the active agent count with correct pluralization, or "idle" at zero. Mirrors Go
/// `agentSummary`.
fn agent_summary(n: i64) -> String {
    match n {
        n if n <= 0 => "idle".to_string(),
        1 => "1 agent".to_string(),
        n => format!("{n} agents"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A Status with the given state and last_err; the other fields are their zero values (mirrors the
    // Go tests' `supervisor.Status{State: ...}` literals).
    fn status(state: State, last_err: &str) -> Status {
        Status {
            state,
            pid: 0,
            restarts: 0,
            last_err: last_err.to_string(),
        }
    }

    // When the daemon is stopped, only Start is actionable and the dashboard cannot be opened.
    // Mirrors Go `TestMenuStopped`.
    #[test]
    fn stopped_offers_start_only() {
        let m = menu_from_status(&status(State::Stopped, ""), 0, true);
        assert!(
            m.can_start && !m.can_stop && !m.can_open,
            "stopped menu actions = {m:?}; want Start only"
        );
        assert!(
            m.status_text.to_lowercase().contains("stopped"),
            "status_text = {:?}; want it to mention stopped",
            m.status_text
        );
    }

    // When stopped but no WORKFLOW.md exists, Start is NOT actionable — the tray must not offer an
    // action that start_daemon would reject. Mirrors Go `TestMenuStoppedUnconfigured`.
    #[test]
    fn stopped_unconfigured_disables_start() {
        let m = menu_from_status(&status(State::Stopped, ""), 0, false);
        assert!(
            !m.can_start,
            "unconfigured stopped menu can_start = true; want false (no WORKFLOW.md to run)"
        );
    }

    // While starting, Stop/Restart are available but the dashboard is not yet openable and Start is
    // disabled. Mirrors Go `TestMenuStarting`.
    #[test]
    fn starting_enables_stop_disables_open_and_start() {
        let m = menu_from_status(&status(State::Starting, ""), 0, true);
        assert!(
            !m.can_start && m.can_stop && !m.can_open,
            "starting menu actions = {m:?}; want Stop enabled, Open/Start disabled"
        );
    }

    // Running with no active agents reads as idle and the dashboard opens. Mirrors Go
    // `TestMenuRunningIdle`.
    #[test]
    fn running_idle_enables_stop_and_open() {
        let m = menu_from_status(&status(State::Running, ""), 0, true);
        assert!(
            !m.can_start && m.can_stop && m.can_open,
            "running menu actions = {m:?}; want Stop+Open enabled, Start disabled"
        );
        assert!(
            m.status_text.to_lowercase().contains("idle"),
            "status_text = {:?}; want it to read idle when 0 agents",
            m.status_text
        );
    }

    // The active agent count surfaces in the status text with correct pluralization. Mirrors Go
    // `TestMenuRunningWithAgents`.
    #[test]
    fn running_agent_count_pluralizes() {
        let one = menu_from_status(&status(State::Running, ""), 1, true);
        assert!(
            one.status_text.contains("1 agent") && !one.status_text.contains("1 agents"),
            "status_text = {:?}; want singular '1 agent'",
            one.status_text
        );
        let three = menu_from_status(&status(State::Running, ""), 3, true);
        assert!(
            three.status_text.contains("3 agents"),
            "status_text = {:?}; want '3 agents'",
            three.status_text
        );
    }

    // A recorded error (e.g. a crash reason) is surfaced in the tooltip so the menu-bar item explains
    // a degraded state. Mirrors Go `TestMenuTooltipSurfacesError`.
    #[test]
    fn tooltip_surfaces_error() {
        let m = menu_from_status(
            &status(State::Stopped, "daemon exited: signal: killed"),
            0,
            true,
        );
        assert!(
            m.tooltip.contains("daemon exited"),
            "tooltip = {:?}; want it to surface the last error",
            m.tooltip
        );
    }
}
