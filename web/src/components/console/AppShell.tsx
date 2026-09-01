import { Fragment, type ReactNode } from "react";
import { cn } from "@/lib/utils";
import { NavItem } from "./NavItem";

export interface NavItemSpec {
  id: string;
  label: string;
  icon: ReactNode;
  count?: number;
  href?: string;
  /**
   * §2.2's capability gate. `false` means the daemon cannot do this at all (teams off),
   * and the item is NOT RENDERED — absent from the DOM, never greyed out, because a
   * disabled row still advertises a feature the operator cannot reach.
   */
  enabled?: boolean;
  /** Draw the rail's hairline separator immediately above this item. */
  separatorBefore?: boolean;
}

export interface AppShellProps {
  items: readonly NavItemSpec[];
  /** Id of the item to highlight. A child route highlights its parent (§2.3). */
  active: string;
  onNavigate?: (id: string) => void;
  /** Wordmark beside the logo mark. */
  brand?: ReactNode;
  /** Single glyph inside the logo mark. */
  mark?: ReactNode;
  /** Rail foot — daemon status, port, version, capability flags. */
  foot?: ReactNode;
  className?: string;
  children?: ReactNode;
}

// AppShell — the 214px nav rail plus the main column (STUDIO-681 §1.3/§1.7). It carries the
// `.rh-console` theme scope, so every token in theme/tokens.css resolves for everything
// rendered inside it. Below 860px the rail collapses to a 52px icon rail, in CSS.
export function AppShell({
  items,
  active,
  onNavigate,
  brand = "rhapsodyd",
  mark = "R",
  foot,
  className,
  children,
}: AppShellProps) {
  const visible = items.filter((item) => item.enabled !== false);
  return (
    <div className={cn("app", "rh-console", className)}>
      <aside className="rail">
        <div className="logo">
          <span className="mk" aria-hidden="true">
            {mark}
          </span>
          <b>{brand}</b>
        </div>
        <nav className="nav" aria-label="Primary">
          {visible.map((item) => (
            <Fragment key={item.id}>
              {item.separatorBefore ? <div className="sep" /> : null}
              <NavItem
                id={item.id}
                label={item.label}
                icon={item.icon}
                count={item.count}
                href={item.href}
                active={item.id === active}
                onSelect={onNavigate}
              />
            </Fragment>
          ))}
        </nav>
        {foot === undefined ? null : <div className="foot">{foot}</div>}
      </aside>
      <main className="main">{children}</main>
    </div>
  );
}
