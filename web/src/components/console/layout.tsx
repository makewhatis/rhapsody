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
  /** `acc` tints the number amber (in review), `bad` red (blocked, quorum failures). */
  tone?: "acc" | "bad";
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
  className?: string;
}

/** The small color disc that identifies a teammate in a table row or summary cell. */
export function TeammateAvatar({ color, size = 8, className }: TeammateAvatarProps) {
  return (
    <span
      className={cn("av", className)}
      aria-hidden="true"
      style={{ display: "inline-block", flex: "none", width: size, height: size, borderRadius: "50%", background: color }}
    />
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
