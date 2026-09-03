import * as React from "react";
import {
  Button,
  Card,
  Mono,
  Note,
  NowStats,
  NowStrip,
  Pill,
  Seg,
  Stat,
  TicketChip,
} from "@/components/console";
import { useDismissReview, useReviews, useRerunReview } from "@/hooks/useReviews";
import { errText } from "@/lib/teams-model";
import {
  retiredCount,
  reviewRows,
  reviewStats,
  type ReviewFilter,
  type ReviewRow,
} from "@/lib/reviews-model";
import type { ReviewJob } from "@/lib/api";
import "@/theme/console-reviews.css";

// Reviews — the ticketless review console (STUDIO-722, slice 8 of the design record
// `~/.rhapsody/docs/STUDIO-703-ticketless-pr-review.md`, §7, §15-e).
//
// Every pull request the daemon is watching, one row per (PR, reviewer), plus the two operator
// controls. It is a child of Teams in the rail (§2.3's `NAV_PARENT` shape, like `manage`), reached
// from the Teams console, because a review is a thing the TEAM does.
//
// # Why the controls are here and not in the room
//
// §14.1's fatal F-SEC finding: a review checks a pull request's head out and reads its diff under
// `bypassPermissions`, so whoever names the pull request decides what code the daemon runs — and a
// room post's `from: operator` is forgeable by any local process. §15-e moved operator control to
// the authenticated console, and this view is it. The daemon re-validates every coordinate it is
// sent (including the watched-repo allowlist) and can only ever re-arm a row that already exists,
// so nothing here can introduce a pull request into the watch set.
//
// It reads exactly three routes — `GET /api/v1/reviews` and the two `POST /api/v1/reviews/*`
// controls — through `hooks/useReviews`, and adds no model: `lib/reviews-model.ts` owns everything
// derived, so the rules are assertable without a DOM.

const FILTERS: readonly { value: ReviewFilter; label: string }[] = [
  { value: "active", label: "Active" },
  { value: "all", label: "All" },
];

export interface ReviewsViewProps {
  /** Route back — the breadcrumb returns to the Teams console, as Manage team's does (§7). */
  onNavigate: (route: "teams") => void;
  /** Poll cadence for the watch set, matched to the daemon's own interval when known. */
  pollMs?: number;
}

export function ReviewsView({ onNavigate, pollMs }: ReviewsViewProps) {
  // Mounted only from a Teams-ON route (`useConsoleRoute` sends `reviews` to Jobs otherwise), so
  // the query is safe to fire — the §16 gate's Teams half is already satisfied by being here.
  const reviews = useReviews(true, pollMs);
  const rerun = useRerunReview();
  const dismiss = useDismissReview();
  const [filter, setFilter] = React.useState<ReviewFilter>("active");

  const enabled = reviews.data?.enabled === true;
  const jobs = React.useMemo(() => reviews.data?.reviews ?? [], [reviews.data]);
  const rows = React.useMemo(() => reviewRows(jobs, filter), [jobs, filter]);
  const stats = React.useMemo(() => reviewStats(jobs), [jobs]);
  const retired = retiredCount(jobs);

  // The OTHER half of the §16 gate. Teams is on (or this view would not be mounted) but the review
  // mode is not `ticketless`, so the subsystem is dormant: say so plainly instead of rendering an
  // empty table, which would read as "no pull requests are being reviewed".
  if (reviews.data !== undefined && !enabled) {
    return (
      <Page onNavigate={onNavigate}>
        <Note>
          Ticketless review is off on this daemon. Set <code>teams.review.mode</code> to{" "}
          <code>ticketless</code> in <code>teams.yaml</code> to have hand-offs open review jobs on
          their own pull requests.
        </Note>
      </Page>
    );
  }

  // A failed write is reported above the table and left there: the row it concerns is still on
  // screen, and clearing the message on the next 5s poll would take the explanation away before it
  // had been read. Any later click replaces it.
  const writeError = rerun.error ?? dismiss.error;

  return (
    <Page onNavigate={onNavigate}>
      <p className="lead">
        Every pull request the team is reviewing, one row per reviewer. Re-run asks for another
        round of the current head; dismiss takes the pull request out of the watch set. Neither is
        available from the team room — an operator control over what gets checked out belongs to
        this console.
      </p>

      <NowStrip>
        <NowStats>
          <Stat value={stats.pullRequests} label="pull requests" />
          <Stat
            value={stats.inFlight}
            label="reviewing"
            tone={stats.inFlight > 0 ? "acc" : undefined}
          />
          <Stat value={stats.awaiting} label="awaiting a round" />
        </NowStats>
      </NowStrip>

      {retired > 0 ? (
        <div className="rfilters">
          <Seg
            accent
            aria-label="Which reviews to show"
            options={FILTERS.map((f) => ({ value: f.value, label: f.label }))}
            value={filter}
            onChange={(v) => setFilter(v as ReviewFilter)}
          />
          <span className="hint">{retired} retired</span>
        </div>
      ) : null}

      {writeError ? (
        <div role="alert">
          <Note variant="warn">The daemon refused that: {errText(writeError)}</Note>
        </div>
      ) : null}

      <Card>
        <table className="jtbl rtbl">
          <thead>
            <tr>
              <th>Pull request</th>
              <th>Reviewer</th>
              <th>Status</th>
              <th>Reviewed at</th>
              <th>
                <span className="sr">Controls</span>
              </th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => (
              <ReviewsRow
                key={row.key}
                row={row}
                busy={rerun.isPending || dismiss.isPending}
                onRerun={(job) => rerun.mutate(job)}
                onDismiss={(job) => dismiss.mutate(job)}
              />
            ))}
          </tbody>
        </table>
        {rows.length === 0 ? (
          <div className="empty">{emptyMessage(jobs.length, reviews.isPending)}</div>
        ) : null}
      </Card>
    </Page>
  );
}

function emptyMessage(total: number, loading: boolean): string {
  if (loading) return "Loading reviews…";
  if (total === 0) return "No pull requests are being watched.";
  return "No active reviews. Switch to All to see retired ones.";
}

/**
 * One watch-set row.
 *
 * Unlike a Jobs row this is NOT itself an activation target: it holds two buttons, and a row that
 * also navigated would make every click ambiguous. There is no review-detail route to navigate to
 * in this slice either — the pull request itself is where a review is read, and that is the link.
 */
function ReviewsRow({
  row,
  busy,
  onRerun,
  onDismiss,
}: {
  row: ReviewRow;
  busy: boolean;
  onRerun: (job: ReviewJob) => void;
  onDismiss: (job: ReviewJob) => void;
}) {
  return (
    <tr>
      <td>
        <a className="ti" href={row.url} target="_blank" rel="noreferrer">
          <TicketChip variant="pr">{row.pr}</TicketChip>
        </a>
        <div className="pj">{row.job.introduced_by === "" ? "" : row.job.introduced_by}</div>
      </td>
      <td>{row.job.reviewer === "" ? "—" : row.job.reviewer}</td>
      <td>
        <Pill variant={row.variant}>{row.label}</Pill>
      </td>
      {/* The SHA the last completed round actually READ — pinned at checkout, never re-queried at
          completion, so it is the commit that was reviewed rather than whatever the head is now. */}
      <td>{row.reviewedShort === "" ? "—" : <Mono>{row.reviewedShort}</Mono>}</td>
      <td className="rctl">
        {row.live ? (
          <>
            <Button
              variant="sec"
              disabled={busy}
              onClick={() => onRerun(row.job)}
              aria-label={`Re-run the review of ${row.pr}`}
            >
              Re-run
            </Button>
            <Button
              variant="link"
              disabled={busy}
              onClick={() => onDismiss(row.job)}
              aria-label={`Dismiss ${row.pr} from the watch set`}
            >
              Dismiss
            </Button>
          </>
        ) : (
          // A retired row has nothing to steer: re-running it would put a merged, closed or
          // already-dismissed pull request back into the dispatch path, which the daemon refuses
          // anyway. Offering a button that can only fail is worse than offering none.
          <span className="hint">retired</span>
        )}
      </td>
    </tr>
  );
}

function Page({
  onNavigate,
  children,
}: {
  onNavigate: (r: "teams") => void;
  children: React.ReactNode;
}) {
  return (
    // `.rh-console` is normally inherited from AppShell, which carries the theme scope; it is
    // repeated here so the view is also correct rendered on its own (a test, a gallery route).
    <section className="rh-console">
      <div className="crumbs">
        {/* A button, not a link: it performs an action (routing), not a document jump. */}
        <button type="button" className="link" onClick={() => onNavigate("teams")}>
          Teams
        </button>{" "}
        · Reviews
      </div>
      <div className="head">
        <h1>Reviews</h1>
      </div>
      {children}
    </section>
  );
}
