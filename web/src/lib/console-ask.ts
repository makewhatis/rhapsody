import type { TeamsRoomMessage } from "@/lib/api";

// console-ask — the model behind the ANSWER half of the "Ask about this run" dock (design record
// `~/.rhapsody/docs/answering-manager-design.md` §9.5 slice 5, "console parity for team-scoped
// answers"; the dock itself is STUDIO-745, slice 4 of `console-run-detail-design.md`).
//
// The dock already posts the operator's question into the team room, and the answering manager
// already replies to it there (STUDIO-729→732). This module is the READ that closes the loop: it
// finds that reply and hands it back so the console can render it beside the question instead of
// making the operator go looking for it in the room feed.
//
// It is a read and nothing else. There is no second answer engine here, no second gather, and no
// new fact source: what the console renders is byte-for-byte the room post `@manager` wrote, and
// the answer keeps living in the room where every teammate reads it. The console is a window onto
// that one record, not a copy of it.

/**
 * The identity the daemon host-stamps on the manager's own room posts —
 * `MANAGER_IDENTITY` (`crates/orchestrator/src/triage.rs`).
 *
 * Matching on it is not decoration. `already_answered`
 * (`crates/orchestrator/src/teamsears.rs`) consults `@manager`'s posts and ONLY `@manager`'s for
 * exactly the same reason this does: `refs` is caller-supplied on every other post, so a teammate
 * — or a forged line appended to the room's JSONL — can name an operator post's id and would
 * otherwise be rendered to the operator as the manager's answer. `from` is the one field a poster
 * cannot supply (design §0.11.4), so it is the one field worth trusting here.
 */
export const MANAGER_IDENTITY = "@manager";

/** A question this dock posted, as the daemon echoed it back. */
export interface AskedQuestion {
  /**
   * `file:seq` — the id `POST /api/v1/teams/room` returned. The same id a later room read serves
   * for that message, which is what makes this lookup possible at all.
   */
  id: string;
  /**
   * What the operator actually sent. Kept from the POST rather than re-read from the input box, so
   * the card names the question that LANDED and can never drift onto the one being typed next.
   *
   * It is what was SENT, which is very nearly but not exactly what the room stored: the wire
   * accepts `MAX_RETAIN_BODY` (64 KiB) but `RoomLog::append` truncates the body it writes to
   * `MAX_POST_BODY_BYTES` (4000), and the daemon's echo carries no body back to check against. So
   * a question between those two sizes is quoted whole here while the manager only ever saw its
   * first 4000 bytes. The answer half carries the room's own `…`; this half cannot, because it
   * never went through the room.
   */
  body: string;
}

/**
 * What the room read says about a question the dock posted.
 *
 * Three outcomes rather than two, because "no reply found" means two different things and only one
 * of them is a statement the console is entitled to make.
 */
export type AskOutcome =
  /** `@manager` replied, and this is the reply — the room's own post, not a re-computed answer. */
  | { kind: "answered"; reply: TeamsRoomMessage }
  /** The question is inside the read and carries no reply yet: a real, provable "not yet". */
  | { kind: "waiting" }
  /** The read no longer reaches the question, so its silence proves nothing. */
  | { kind: "past-window" }
  /** No read has come back since the question landed, so nothing has looked for it yet. */
  | { kind: "unread" };

/**
 * The manager's reply to one question, or why the console cannot see one.
 *
 * The `waiting` branch is a stronger claim than [`roomEmptyNote`](./console-watch)'s, and it is
 * allowed to be, because of a property of the read rather than of this function. `GET
 * /api/v1/teams/room` is served by `RoomLog::read_since`, whose contract is "at most `limit`
 * messages, the NEWEST ones when more are available, oldest first" (`crates/config/src/room.rs`),
 * and a reply is always appended AFTER the post it answers. So a read containing the question also
 * contains every message written since it — the manager's reply among them, if one exists. Absence
 * is then evidence, and "not answered yet" is true rather than merely unrefuted.
 *
 * That premise is the whole warrant for the claim, so it is worth being precise about what it does
 * NOT rest on. It does not need the read to be complete, and the day-partitioned
 * `MAX_ROOM_FILE_SCAN` bound under the message window cannot weaken it: that bound drops the
 * OLDEST day files, which can only ever remove messages written BEFORE the question.
 *
 * Lose that premise and the claim goes with it: once the question itself has fallen out of the
 * window, the read says nothing at all about what came after, so the outcome is `past-window` and
 * the dock stops claiming. That distinction is the whole point — a dock that reported both cases as
 * "waiting" would sit there telling the operator an answer had not arrived long after it had.
 *
 * Which leaves the absence of the question meaning THREE things rather than two, and that is what
 * `readSettledSinceAsking` decides between. "The window has moved past it" is a conclusion about a
 * read that COULD have seen the question; a read that came back before the question landed never
 * could, and its silence is not evidence of anything — it has not caught up yet. That case is not
 * exotic: the Room tab holds this very query open on essentially every run detail, so the newest
 * data on the key at the moment a question lands is routinely a window from before it.
 *
 * The caller supplies the difference; it is not a clock comparison, because two reads can settle
 * inside one millisecond. It is that a read has SETTLED since the question landed — react-query's
 * `isFetchedAfterMount`, an update count against the one this exchange mounted on. An absence the
 * gate has not vouched for is `unread`, which claims nothing at all.
 *
 * Note what the gate does NOT touch: `answered` and `waiting` are positive findings, impossible to
 * produce from a read that never saw the message, so they stand on their own either way.
 */
export function managerReply(
  messages: readonly TeamsRoomMessage[],
  asked: AskedQuestion,
  readSettledSinceAsking: boolean,
): AskOutcome {
  const reply = messages.find(
    (m) => m.from === MANAGER_IDENTITY && (m.refs ?? []).includes(asked.id),
  );
  if (reply !== undefined) return { kind: "answered", reply };
  if (messages.some((m) => m.id === asked.id)) return { kind: "waiting" };
  return readSettledSinceAsking ? { kind: "past-window" } : { kind: "unread" };
}

/**
 * What the dock may say while the reply has not arrived.
 *
 * "Has not replied yet" and not "is thinking": nothing tells the console that the manager has even
 * READ the question. It answers on its own cycle, at most `MAX_POSTS_PER_TICK` posts per pass and
 * behind its own action budget (`crates/orchestrator/src/teamsears.rs`), so a wait of several
 * cycles is ordinary and a progress claim would be invented.
 */
export const ASK_WAITING_NOTE = `Posted to the room. ${MANAGER_IDENTITY} has not replied to it yet.`;

/**
 * What the dock may say once the read has moved past the question.
 *
 * It reports the READ, never the room. Two separate bounds can put a question out of reach — the
 * message window, and the day-partitioned `MAX_ROOM_FILE_SCAN` scan under it — and
 * `TeamsRoomResponse` distinguishes neither, so the sentence names no number and no cause. What it
 * must not do is degrade into either lie available here: that no answer came, or that one is still
 * coming.
 */
export const ASK_PAST_WINDOW_NOTE =
  `Posted to the room. This dock's room read no longer reaches that question, so it cannot tell ` +
  `whether ${MANAGER_IDENTITY} replied.`;

/**
 * What the dock may say before any read has come back since the question landed.
 *
 * The room read polls on its own 5s cycle and the post invalidates it, so this is the state
 * between the question landing and the first read that could possibly contain it — ordinarily one
 * round trip. It reports the CONSOLE, which is the only thing that has happened yet: saying
 * `ASK_WAITING_NOTE` here would report a silence nothing has listened for, and
 * `ASK_PAST_WINDOW_NOTE` would claim the read had moved past a question it has not yet reached.
 */
export const ASK_READING_NOTE = "Posted to the room — reading it back…";

/** The sentence for an outcome with no reply in it. `answered` has a card, not a note. */
export function askNote(outcome: Exclude<AskOutcome, { kind: "answered" }>): string {
  switch (outcome.kind) {
    case "waiting":
      return ASK_WAITING_NOTE;
    case "past-window":
      return ASK_PAST_WINDOW_NOTE;
    case "unread":
      return ASK_READING_NOTE;
  }
}
