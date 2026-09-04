import type { HTMLAttributes, ReactNode } from "react";
import { cn } from "@/lib/utils";

// Layout primitives — STUDIO-681 §1.4. The two-column `.grid` (Teams console, Job detail)
// and the `.now` summary strip (Jobs, Teams), plus the small mono spans §1.2 asks for.

/** Shared shape for the plain wrapper primitives below — a div with children. */
export interface DivProps extends HTMLAttributes<HTMLDivElement> {
  children?: ReactNode;
}

/** `.grid` — 1fr + 336px, collapsing to a single column below 1000px. */
export function Grid({ className, children, ...rest }: DivProps) {
  return (
    <div className={cn("grid", className)} {...rest}>
      {children}
    </div>
  );
}

/** `.side` — the 336px column's vertical stack of cards. */
export function GridSide({ className, children, ...rest }: DivProps) {
  return (
    <div className={cn("side", className)} {...rest}>
      {children}
    </div>
  );
}

/** `.now` — the summary strip: teammate states on the left, stat pills on the right. */
export function NowStrip({ className, children, ...rest }: DivProps) {
  return (
    <div className={cn("now", className)} {...rest}>
      {children}
    </div>
  );
}

/** `.now .who` — the teammate-state group. */
export function NowMates({ className, children, ...rest }: DivProps) {
  return (
    <div className={cn("who", className)} {...rest}>
      {children}
    </div>
  );
}

/** `.stats` — the stat-pill group. */
export function NowStats({ className, children, ...rest }: DivProps) {
  return (
    <div className={cn("stats", className)} {...rest}>
      {children}
    </div>
  );
}

export interface MateProps {
  name: string;
  /** What they are doing right now — a ticket key when running, "idle" otherwise. */
  task?: ReactNode;
  running?: boolean;
  className?: string;
}

/** `.mate` — one teammate's live state in the now strip. */
export function Mate({ name, task, running = false, className }: MateProps) {
  return (
    <span className={cn("mate", running && "run", className)}>
      <span className="st" aria-hidden="true" />
      <b>{name}</b>
      {task === undefined ? null : <span className="task">{task}</span>}
    </span>
  );
}

export interface StatProps {
  value: ReactNode;
  label: ReactNode;
  /** `acc` tints the number amber (in review), `bad` red (blocked, quorum failures), `op` the
      operator teal (work waiting on the human — STUDIO-743's "Needs you"). */
  tone?: "acc" | "bad" | "op";
  className?: string;
}

/** `.stat` — one counted stat pill. Numbers are mono + tabular so columns line up (§1.2). */
export function Stat({ value, label, tone, className }: StatProps) {
  return (
    <div className={cn("stat", tone, className)}>
      <div className="n">{value}</div>
      <div className="l">{label}</div>
    </div>
  );
}

export interface TeammateAvatarProps {
  /** A CSS color, normally `teammateColor(roster, name)` from theme/teammates.ts (§1.5). */
  color: string;
  size?: number;
  /**
   * The teammate this stands for. Supplying one turns the disc into the prototype's `.av` — a
   * filled circle carrying their INITIAL (STUDIO-763) — which is what the run-detail header shows
   * beside the full name. Omit it for the plain disc a table row or a summary cell uses.
   */
  name?: string;
  className?: string;
}

/** The small color disc that identifies a teammate in a table row or summary cell. */
export function TeammateAvatar({ color, size = 8, name, className }: TeammateAvatarProps) {
  // A roster name arrives from `teams.yaml` and from a route event's text, so it can carry
  // whitespace; a name that is nothing but whitespace has no initial and stays the plain disc
  // rather than rendering an empty lettered circle.
  const initial = (name ?? "").trim().charAt(0).toUpperCase();
  return (
    <span
      // `.ini` is what the stylesheet keys the lettered variant's grid centring and type off.
      // NOT `.mate`, which the Now strip's teammate CHIP already owns (`console.css`) — sharing it
      // would wrap the avatar in that chip's pill border and padding.
      className={cn("av", initial !== "" && "ini", className)}
      // Decorative either way: the lettered variant abbreviates a name that is rendered in full
      // beside it, so announcing the letter would only repeat it.
      aria-hidden="true"
      // `grid` for the lettered variant so the initial centres; the plain disc stays inline-block,
      // which is what every table row and summary cell already lays out against.
      style={{ display: initial === "" ? "inline-block" : "grid", flex: "none", width: size, height: size, borderRadius: "50%", background: color }}
    >
      {initial === "" ? null : initial}
    </span>
  );
}

/** `.mono` — ids, keys, SHAs and code that must render in IBM Plex Mono (§1.2). */
export function Mono({ className, children, ...rest }: HTMLAttributes<HTMLSpanElement>) {
  return (
    <span className={cn("mono", className)} {...rest}>
      {children}
    </span>
  );
}

/** `.at` — a timestamp. Mono and muted, per §1.2. */
export function Timestamp({ className, children, ...rest }: HTMLAttributes<HTMLSpanElement>) {
  return (
    <span className={cn("at", className)} {...rest}>
      {children}
    </span>
  );
}
