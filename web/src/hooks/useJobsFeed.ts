import { useEffect, useRef } from "react";
import { useQueryClient } from "@tanstack/react-query";
import type { StateResponse } from "@/lib/api";
import { HISTORY_ISSUES_QUERY_KEY, useIssueRuns } from "@/hooks/useHistory";
import { LIVE_POLL_MS, useStateQuery } from "@/hooks/useStateQuery";

/**
 * The two reads the Jobs worklist is built from, held to ONE freshness (STUDIO-791).
 *
 * The Jobs surface draws its header counts and its rows from the same merged array, but that array
 * is fed by two endpoints: `/api/v1/state` for what is live right now and `/api/v1/history/issues`
 * for one row per ticket. Only the first was polling — the second was called with no options, and
 * `useIssueRuns` defaults `refetchInterval` to `false`, so it fetched once on mount and never again.
 * The list was not slow; it was static, and it looked intermittently alive only because the live
 * half of the merge kept moving under it.
 *
 * Which half then shows the wrong answer depends on the transition, and it is worth being exact
 * about since "the header ticks while the rows don't" is the obvious guess and only half right. The
 * counts are computed from the SAME merged array the table renders, so a stale stored row pins the
 * count exactly as it pins the row: a run FINISHING froze both at "running". A run STARTING is the
 * asymmetric one — the live snapshot carries it into the strip and into a live row immediately,
 * while every stored column beside it (outcome, lifecycle, updated-at) stays at mount's answer.
 *
 * Two things are needed to close that, and equal intervals is only the first. Two independent 2s
 * timers are still phase-shifted by up to a full tick, which is long enough to see. So the live
 * snapshot — the fresher and cheaper of the pair — also PULLS the listing forward: when the set of
 * work it reports changes shape, the listing is invalidated immediately rather than waiting for its
 * own tick. Between the two, the header can no longer report a transition the rows have not seen.
 *
 * On the ticket's "event-driven" alternative: the daemon does own an SSE seam
 * (`GET /api/v1/logs/stream`), but it is not one this can borrow. There is no run/state event stream
 * to subscribe to, so the push route means a new `/api/v1` endpoint — an additive divergence from
 * the Go reference — plus orchestrator-side broadcast plumbing, plus a SECOND host-side IPC bridge:
 * the packaged app serves the dashboard over wry's fully-buffered custom protocol, which is why the
 * log tail already needs `desktop/src-tauri/src/logbridge.rs` to reach the webview at all (TRA-252).
 * That is a slice of its own, not a line in this one. The floor below is what landed.
 */
export function useJobsFeed() {
  const qc = useQueryClient();
  const state = useStateQuery();
  const issueRuns = useIssueRuns({}, { refetchInterval: LIVE_POLL_MS });

  // `null` until the first snapshot lands, so seeding the comparison is not mistaken for a change:
  // the listing has already fetched on mount and does not need a second identical request.
  const signature = state.data === undefined ? null : liveJobsSignature(state.data);
  const seen = useRef<string | null>(null);
  useEffect(() => {
    if (signature === null) return;
    if (seen.current !== null && seen.current !== signature) {
      void qc.invalidateQueries({ queryKey: HISTORY_ISSUES_QUERY_KEY });
    }
    seen.current = signature;
  }, [signature, qc]);

  return { state, issueRuns };
}

/**
 * A stable fingerprint of the WORK the live snapshot reports — the set of tickets in flight, queued
 * for retry or held, and the identity of each — and deliberately nothing else.
 *
 * What is excluded matters more than what is included. A running agent's `turn_count` and token
 * totals move on every single poll; folding either in would make the signature change 30 times a
 * minute per live run, and the fix for a list that never refreshed would become a list that fires a
 * second request on every tick. What is in scope is membership and identity: a run appearing or
 * disappearing, a ticket's workflow state moving under it, a retry becoming a later attempt, a
 * blocker's state clearing — the transitions that actually change what a row should say.
 *
 * Sorted, because the daemon promises no order and a reordered list is not a state change.
 * Exported for unit testing, in the shape `runDetailPollInterval` set.
 */
export function liveJobsSignature(state: StateResponse | undefined): string {
  const parts: string[] = [];
  for (const r of state?.running ?? []) parts.push(`r:${r.run_id}:${r.issue_identifier}:${r.state}`);
  for (const r of state?.retrying ?? []) parts.push(`q:${r.issue_identifier}:${r.attempt}`);
  for (const b of state?.blocked ?? []) parts.push(`b:${b.issue_identifier}:${b.blocker_identifier}:${b.blocker_state}`);
  return parts.sort().join("|");
}
