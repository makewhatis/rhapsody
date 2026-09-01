import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

export interface NavItemProps {
  /** Route id — also the default hash target, so the rail works before JS routing does. */
  id: string;
  label: string;
  icon: ReactNode;
  /** Optional trailing count badge (`.ct`), e.g. Jobs' open-ticket count. */
  count?: number;
  active?: boolean;
  /** Overrides the default `#{id}` target. */
  href?: string;
  onSelect?: (id: string) => void;
}

// NavItem — one row of the rail (STUDIO-681 §1.3). A real `<a href>` rather than a div with
// a click handler: the console routes by hash (§2.3), so the anchor is both the correct
// semantics and keyboard-operable for free.
export function NavItem({ id, label, icon, count, active = false, href, onSelect }: NavItemProps) {
  return (
    <a
      href={href ?? `#${id}`}
      data-nav={id}
      className={cn(active && "active")}
      aria-current={active ? "page" : undefined}
      onClick={() => onSelect?.(id)}
    >
      <span className="ic">{icon}</span>
      <span>{label}</span>
      {count === undefined ? null : <span className="ct">{count}</span>}
    </a>
  );
}
