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
 * Every value reaching here comes off a `MergeReceipt`, so it has passed every refusal
 * `runmerge::resolve_pull_request` makes. That is load-bearing for BEHIND below, whose reading
 * depends on the gate the daemon already applied to it. A value from anywhere else — the Diff
 * tab's `rundiff`, which gates nothing — belongs in [`ungatedMergeStateNote`] instead.
 *
 * `undefined` is accepted even though the receipt types the field as a `string`, because the
 * receipt crosses a process boundary and `api.ts` casts the daemon's JSON rather than validating
 * it. A display helper that throws takes the whole run-detail header down with it; saying nothing
 * is the right failure.
 *
 * `armed` says which side of the merge the note is being read on, and it changes exactly one
 * answer. Unarmed — in the confirm dialog, describing the pull request the operator is about to
 * merge — `CLEAN` means "nothing is in the way". ARMED, `gh pr merge --auto` has already landed a
 * `CLEAN` pull request, so "GitHub reports it ready to merge" beside "queued … for merge" would
 * read as though it were still waiting. There is nothing left for it to wait on, so the note says
 * nothing. The states that DO still hold it up — BLOCKED, DIRTY — are the ones worth reading
 * there, and they are unchanged.
 *
 * Unarmed is the DIALOG's reading and not the header's. The header renders no note before a
 * merge: its pre-click channel is the daemon's own verdict on the control itself (STUDIO-790),
 * and a second sentence beside it, derived here from a receipt the daemon did NOT refuse, is one
 * that can contradict it — which it did, on DIRTY.
 */
export function mergeStateNote(state: string | undefined, armed = false): string {
  return stateNote(state, armed, true);
}

/**
 * The same reading, for a `mergeStateStatus` NO daemon gate has filtered (STUDIO-749).
 *
 * `rundiff` deliberately refuses nothing, so the console's Diff tab is the first caller holding a
 * raw `mergeStateStatus` — one that has passed none of `runmerge::resolve_pull_request`'s
 * refusals. Exactly one arm above depends on having passed them, and it is BEHIND: its promise
 * that GitHub updates the branch itself is true only because a BEHIND branch on a repository that
 * will NOT update one was already refused. Ungated, that promise sends the operator to wait for
 * something that never happens — while Merge on the same run tells them to push the branch.
 *
 * So this says only what is observable without the policy read: the branch is behind its base.
 * Every other state is a fact of GitHub's own, reads the same either way, and shares this switch
 * rather than being written twice — one sentence per GitHub state, as before.
 *
 * There is no `armed` here: arming is a property of a merge this console applied, and a value that
 * reached no merge has none.
 */
export function ungatedMergeStateNote(state: string | undefined): string {
  return stateNote(state, false, false);
}

/**
 * The one switch both readings share. `gated` says whether the value passed the daemon's refusals.
 */
function stateNote(state: string | undefined, armed: boolean, gated: boolean): string {
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
      // NOT "push the branch". A receipt carrying BEHIND has already passed
      // `runmerge::resolve_pull_request`'s branch-update gate, which refuses a behind branch on a
      // repository that will not update one (STUDIO-784 gap 1) — so the only BEHIND that reaches
      // this console THROUGH A RECEIPT is one GitHub brings up to date itself. Ungated, that gate
      // has not run, so the note stops at what GitHub itself reported (STUDIO-749).
      return gated
        ? "The branch is behind its base; GitHub will bring it up to date itself before merging."
        : "The branch is behind its base.";
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
