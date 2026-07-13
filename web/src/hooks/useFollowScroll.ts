import * as React from "react";
import { isAtBottom } from "@/lib/follow-scroll";

export interface FollowScroll {
  /** True while the view is stuck to the bottom, auto-following new content. */
  following: boolean;
  /** Attach to the scroll container's `onScroll` — flips following off on an upward scroll and back
   *  on when the user reaches the bottom again. */
  onScroll: React.UIEventHandler<HTMLElement>;
  /** Scroll to the bottom and resume following (the "jump to latest ↓" action). */
  jumpToLatest: () => void;
}

// useFollowScroll keeps a scroll container pinned to its bottom as new content streams in, and
// releases follow-mode the moment the user scrolls up (so a "jump to latest ↓" affordance can show),
// resuming once they scroll back to the bottom or click jump-to-latest. The `dep` should change
// whenever content is appended (e.g. the entry/line count) so the pin re-fires; `active` gates the
// auto-pin so a finished/static view opens at its natural top instead of snapping to the end. The
// returned shape is deliberately generic — the transcript (D4) and the logs panel (D6) share it.
export function useFollowScroll(
  ref: React.RefObject<HTMLElement | null>,
  dep: unknown,
  active = true,
): FollowScroll {
  const [following, setFollowing] = React.useState(true);
  // Read the latest `following` inside the layout effect without making it a dependency (we only
  // want to re-pin when new content arrives, not when follow-state toggles).
  const followingRef = React.useRef(following);
  followingRef.current = following;

  React.useLayoutEffect(() => {
    const el = ref.current;
    if (active && followingRef.current && el) {
      el.scrollTop = el.scrollHeight;
    }
  }, [ref, dep, active]);

  const onScroll = React.useCallback<React.UIEventHandler<HTMLElement>>((e) => {
    const el = e.currentTarget;
    setFollowing(isAtBottom({ scrollTop: el.scrollTop, scrollHeight: el.scrollHeight, clientHeight: el.clientHeight }));
  }, []);

  const jumpToLatest = React.useCallback(() => {
    const el = ref.current;
    if (el) {
      el.scrollTop = el.scrollHeight;
    }
    setFollowing(true);
  }, [ref]);

  return { following, onScroll, jumpToLatest };
}
