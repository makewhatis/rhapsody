import * as React from "react";
import { useStateQuery } from "@/hooks/useStateQuery";
import { hasBridge, notifyNative } from "@/lib/bindings";
import type { NotificationEntry } from "@/lib/api";

// NOTIFICATIONS_SEEN_KEY holds the fingerprints of the desktop notifications already shown. It is in
// localStorage rather than a ref/Set so the de-dupe survives a desktop app RELOAD: the daemon's
// pending-notification queue lives in its own process, so a restarted app polling the same daemon
// would otherwise re-show every notification still queued — the repeat the ticket rules out.
export const NOTIFICATIONS_SEEN_KEY = "rhapsody.notifications.seen";

// MAX_SEEN bounds the fingerprint list so a long-lived install cannot grow it without limit. Oldest
// entries are dropped; the daemon's own queue is bounded the same way.
const MAX_SEEN = 200;

// notificationFingerprint identifies one queued notification. `id` is monotonic per daemon PROCESS
// and resets on restart, so it is paired with `at` (the crossing time) — stable across a poll,
// distinct across a restart — and the ticket to keep two crossings for one ticket apart.
export function notificationFingerprint(n: NotificationEntry): string {
  return `${n.at}|${n.id}|${n.ticket}`;
}

function loadSeen(): Set<string> {
  try {
    if (typeof localStorage === "undefined") return new Set();
    const raw = localStorage.getItem(NOTIFICATIONS_SEEN_KEY);
    if (raw === null) return new Set();
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return new Set();
    return new Set(parsed.filter((v): v is string => typeof v === "string"));
  } catch {
    return new Set();
  }
}

function saveSeen(seen: Set<string>): void {
  try {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(NOTIFICATIONS_SEEN_KEY, JSON.stringify([...seen].slice(-MAX_SEEN)));
  } catch {
    // Storage disabled/full is not worth surfacing: the worst case is a repeated notification.
  }
}

/**
 * useNotifications shows each pending desktop notification from the live snapshot natively, once.
 *
 * The daemon's runaway-loop breaker holds a ticket when its review rounds or per-ticket spend cross a
 * configured limit and queues a notification under `/api/v1/state`'s `notifications` key (STUDIO-1026)
 * — the "new daemon event on the existing state stream". This hook polls that key with the shared
 * live query and hands each entry the daemon has not already shown to `notifyNative`.
 *
 * Desktop-only and inert elsewhere: with no Tauri bridge it does nothing, so the daemon-served
 * dashboard and `vite dev` are unaffected. Idempotent per entry, so the 2s poll never re-shows one,
 * and a reload does not re-show what a previous session already did.
 */
export function useNotifications(): void {
  const { data } = useStateQuery();
  const notifications = data?.notifications;
  React.useEffect(() => {
    if (!hasBridge() || notifications === undefined || notifications.length === 0) return;
    const seen = loadSeen();
    let changed = false;
    for (const n of notifications) {
      const fp = notificationFingerprint(n);
      if (seen.has(fp)) continue;
      seen.add(fp);
      changed = true;
      void notifyNative(n.title, n.body);
    }
    if (changed) saveSeen(seen);
  }, [notifications]);
}
