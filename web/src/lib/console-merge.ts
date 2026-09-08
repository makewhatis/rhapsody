// The console merge action's mergeability wording (STUDIO-784).
//
// WHY THIS EXISTS. Under `--squash --auto` an applied merge ARMS GitHub's own auto-merge; it does
// not land the pull request. Today the operator gets one room line, one `.actok` and then silence,
// with no way to learn the merge is parked — which is how a BEHIND branch sat "queued for merge"
// forever. The daemon's receipt now carries GitHub's own `mergeStateStatus`, and this turns it into
// the one sentence that says what the pull request is actually waiting on.
//
// It is DISPLAY only. Nothing here decides anything: the refusals are the daemon's, made before it
// ever calls `gh pr merge` (see `runmerge::resolve_and_merge`), and a console that re-derived them
// would be guessing at state it does not hold.

/**
 * GitHub's `mergeStateStatus` as an operator reads it, or "" when there is nothing to say.
 *
 * An unrecognised value is passed through rather than swallowed. GitHub's vocabulary is its own and
 * has grown before, and a state this console has never heard of is exactly the one worth showing.
 *
 * `undefined` is accepted even though the receipt types the field as a `string`, because the
 * receipt crosses a process boundary and `api.ts` casts the daemon's JSON rather than validating
 * it. A display helper that throws takes the whole run-detail header down with it; saying nothing
 * is the right failure.
 *
 * `armed` says which side of the click the note is being read on, and it changes exactly one
 * answer. Before the click the note describes what the operator is about to act on, and `CLEAN`
 * means "nothing is in the way". AFTER it, `gh pr merge --auto` has already landed a `CLEAN` pull
 * request — so "GitHub reports it ready to merge" beside "queued … for merge" would read as though
 * it were still waiting. There is nothing left for it to wait on, so the note says nothing. The
 * states that DO still hold it up — BLOCKED, BEHIND, DIRTY — are the ones worth reading there, and
 * they are unchanged.
 */
export function mergeStateNote(state: string | undefined, armed = false): string {
  const value = (state ?? "").trim().toUpperCase();
  switch (value) {
    case "":
    case "UNKNOWN":
      // GitHub computes mergeability lazily and says nothing while it does. Silence beats a guess.
      return "";
    case "CLEAN":
      return armed ? "" : "GitHub reports it ready to merge.";
    case "BLOCKED":
      // The ORDINARY state at arming time: the required contexts have not passed yet, which is
      // precisely what `--auto` exists to wait for.
      return "GitHub is holding it until its required checks pass.";
    case "UNSTABLE":
      return "GitHub reports a non-required check failing; the required ones still decide.";
    case "BEHIND":
      return "The branch is behind its base — it cannot land until someone pushes.";
    case "DIRTY":
      return "The branch conflicts with its base — it cannot land until someone resolves that.";
    case "DRAFT":
      return "The pull request is still a draft, so GitHub will not merge it.";
    case "HAS_HOOKS":
      return "GitHub reports it mergeable, with repository hooks still to run.";
    default:
      return `GitHub reports its merge state as ${value}.`;
  }
}
