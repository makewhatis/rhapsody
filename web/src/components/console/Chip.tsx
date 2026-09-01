import type { ButtonHTMLAttributes, ReactNode } from "react";
import { cn } from "@/lib/utils";

export interface ChipProps extends Omit<ButtonHTMLAttributes<HTMLButtonElement>, "aria-pressed"> {
  /** Filter chips are toggles, so the pressed state is `aria-pressed`, not a class. */
  pressed?: boolean;
  /** Optional trailing count (`.k`), e.g. the room's "Quorum 3". */
  count?: ReactNode;
}

// Chip — toggle chip used by the room and jobs filter bars (STUDIO-681 §1.3).
export function Chip({ pressed = false, count, className, children, type = "button", ...rest }: ChipProps) {
  return (
    <button type={type} aria-pressed={pressed} className={cn("chip", className)} {...rest}>
      {children}
      {count === undefined || count === null ? null : <span className="k">{count}</span>}
    </button>
  );
}
