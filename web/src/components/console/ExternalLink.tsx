import type { AnchorHTMLAttributes } from "react";
import { openExternal } from "@/lib/bindings";

/**
 * `href` is required and never empty: a call site with nothing to link to renders its own
 * dependency-named placeholder rather than a link that goes nowhere.
 */
export interface ExternalLinkProps
  extends Omit<AnchorHTMLAttributes<HTMLAnchorElement>, "href" | "target" | "rel" | "onClick"> {
  href: string;
}

// ExternalLink — the console's one seam for leaving the app (STUDIO-765).
//
// In the packaged desktop app `<a target="_blank">` is a NO-OP: wry never hands the URL to the
// OS, so "Open ticket" and "View PR" had the right href — you could copy it — and clicking did
// nothing. The `openExternal` binding is what the shell already uses (Onboarding, RunDetail,
// AppShell), and it falls back to `window.open` with no Tauri bridge, so the daemon's
// plain-browser dashboard is unchanged.
//
// The anchor keeps its `href`: copy-link, right-click and keyboard focus are anchor behaviour,
// and only the click is redirected. `onClick`, `target` and `rel` are NOT accepted — a call
// site that set its own `onClick` would silently replace the seam, which is the regression this
// component exists to make impossible.
export function ExternalLink({ href, children, ...rest }: ExternalLinkProps) {
  return (
    <a
      href={href}
      target="_blank"
      rel="noreferrer noopener"
      onClick={(e) => {
        // The host command refuses any scheme but http/https, so a `mailto:` (which the room's
        // markdown can produce) is left to the browser rather than sent to a rejected invoke.
        if (!isWebUrl(href)) return;
        e.preventDefault();
        openExternal(href);
      }}
      {...rest}
    >
      {children}
    </a>
  );
}

/** Mirrors `windowserver::open_external`'s scheme guard on the host side. */
function isWebUrl(href: string): boolean {
  return /^https?:\/\//i.test(href);
}
