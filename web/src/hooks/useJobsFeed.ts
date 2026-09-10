import { useEffect, useRef } from "react";
import { useQueryClient } from "@tanstack/react-query";
import type { HistoryFilter, StateResponse } from "@/lib/api";
import {
  HISTORY_ISSUES_QUERY_KEY,
  HISTORY_ISSUE_COUNTS_QUERY_KEY,
  TRACKER_POLL_MS,
  useIssueCounts,
  useIssueRuns,
} from "@/hooks/useHistory";
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
 *
 * THE WINDOW, AND WHY ONLY THE DEFAULT ONE IS POLLED (STUDIO-792). The worklist can now widen its
 * page past the newest 50, and that window arrives here as `filter`. It is deliberately NOT put on
 * the cadence above. Measured against the operator's own daemon (389 issues), a full-width request
 * took 0.63s and 0.88s warm and 3.63s on a cold lifecycle memo, against 1.6ms at the default width.
 * A request of that size every 2s, for as long as the page stays open, is a large and permanent
 * share of the daemon's history path spent on the rows least likely to have moved: the listing is
 * ordered newest-first, so everything a widened window adds is the OLD tail.
 *
 * It costs nothing for the transition this hook exists to catch, and the daemon's own ordering is
 * why rather than luck. `on_worker_exit` takes the run out of the live map and writes
 * `store.end_run` synchronously within a single control-loop event (`orchestrator/src/retry.rs` ->
 * `persist.rs`), and the loop republishes the snapshot `/api/v1/state` serves only AFTER that
 * handler has returned (`orchestrator/src/loop.rs`). So the first snapshot in which a run has left
 * the live set is already backed by a store that has recorded its end — the pull-forward below
 * fires on exactly that snapshot, and the refetch it triggers reads the finished row. A run
 * starting, a retry reaching a new attempt and a blocker clearing move the signature the same way.
 *
 * What the interval uniquely buys is a change the live snapshot cannot see at all: a ticket's
 * tracker state moving with no run in flight, which `JobsView`'s "moves a stored row and its count
 * when only the issue listing changed" pins. That is worth 2s at the default width. It is not worth
 * a 0.6s request every 2s across a window the operator widened, so a widened window came off that
 * cadence entirely — and STUDIO-828 has since put it back on a much slower one (see below), which
 * is the same trade settled at a price the measurements actually support rather than at "never".
 * Capping how far the chip may reach was the other way out and was rejected: it puts STUDIO-792's
 * silent truncation back at a different number.
 *
 * One consequence, stated rather than left to be discovered: while the window is widened the rail's
 * badge query (`useIssueRuns()`, the `{}` key) has no poller of its own, so it too falls back to the
 * pull-forward. That covers the half of that number which actually moves — the live set. The other
 * half, the page rows it also folds in, is the counting defect already noted on `useIssueRuns`, and
 * no cadence was ever going to fix that one.
 *
 * THE THIRD READ, AND THE CASE NONE OF THE ABOVE CATCHES (STUDIO-828). The strip's numbers are now a
 * daemon-computed tally over the whole store rather than a fold over these rows, so this hook holds
 * three queries and not two. All three are pulled forward by the same live signature, which is what
 * keeps the strip, the rows and the badge from reporting a run's start or end a tick apart.
 *
 * The signature cannot see a ticket whose TRACKER state moved with no run involved, and that case
 * stopped being exotic: `review.done_state` (STUDIO-712) now moves a ticket to Done when its pull
 * request merges, from a code path that is not a run — verified in production on 2026-09-10, a merge
 * at 17:14:22 and `auto-done: … state=Done` 108 seconds later with no run in flight. A human editing
 * Linear directly does the same thing. Only an interval catches it, so all three reads keep one —
 * and the BOUND on how late it can be is the daemon's, not the client's: `LIFECYCLE_TTL` is 60s, so
 * no console sees such a move sooner than that however fast it polls. Within two minutes, about one
 * typically, is what this surface promises for a ticket that moved with nothing running.
 *
 * A WIDENED listing is the one read that cannot ride the live cadence, and it now falls back to
 * TRACKER_POLL_MS in place of `false`. STUDIO-792
 * took the widened window off the 2s cadence with real numbers — 0.63s warm for a full-width page,
 * so a request every 2s is a third of the daemon's history path spent on the rows least likely to
 * have moved — and none of those numbers argues for never. At 60s the same page is well under one
 * percent of that path, and it buys the widened window the one refresh the pull-forward cannot give
 * it: a stale row under a strip that has already moved on is the disagreement this ticket is about.
 */
export function useJobsFeed(filter: HistoryFilter = {}) {
  const qc = useQueryClient();
  const state = useStateQuery();
  // An explicit `limit` is the signal, rather than "any filter at all": `JobsView` sends none until
  // the operator has actually widened past the daemon's own default page, so its presence IS the
  // widening. A filter that NARROWS instead — a project, an outcome — makes the request smaller, not
  // larger, and has no reason to lose the cadence.
  const widened = filter.limit !== undefined;
  const issueRuns = useIssueRuns(filter, {
    refetchInterval: widened ? TRACKER_POLL_MS : LIVE_POLL_MS,
  });
  // The tally rides the LIVE cadence with the rest of the surface — it is unfiltered and unwidened
  // by construction, O(1) on the wire, and ~1ms of SQL, so nothing argues for holding it back. A
  // slower strip than table is the disagreement this hook exists to prevent, only spelled the other
  // way round.
  const issueCounts = useIssueCounts({ refetchInterval: LIVE_POLL_MS });

  // `null` until the first snapshot lands, so seeding the comparison is not mistaken for a change:
  // the listing has already fetched on mount and does not need a second identical request.
  const signature = state.data === undefined ? null : liveJobsSignature(state.data);
  const seen = useRef<string | null>(null);
  useEffect(() => {
    if (signature === null) return;
    if (seen.current !== null && seen.current !== signature) {
      void qc.invalidateQueries({ queryKey: HISTORY_ISSUES_QUERY_KEY });
      void qc.invalidateQueries({ queryKey: HISTORY_ISSUE_COUNTS_QUERY_KEY });
    }
    seen.current = signature;
  }, [signature, qc]);

  return { state, issueRuns, issueCounts };
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
