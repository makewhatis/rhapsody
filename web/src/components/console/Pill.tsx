import type { HTMLAttributes, ReactNode } from "react";
import { cn } from "@/lib/utils";

export type PillVariant = "run" | "review" | "queued" | "done" | "blocked";

/**
 * The color each variant paints, as declared in `theme/console.css`. Exported so a view
 * can tint a matching dot or sparkline off the same source instead of guessing, and so
 * the variant→color contract (§10 box 1.4) is assertable without a browser.
 */
export const PILL_COLORS: Record<PillVariant, string> = {
  run: "var(--ok)",
  review: "var(--accent)",
  queued: "var(--ink-3)",
  done: "var(--info)",
  blocked: "var(--bad)",
};

export interface PillProps extends HTMLAttributes<HTMLSpanElement> {
  variant: PillVariant;
  children?: ReactNode;
}

// Pill — status pill with a leading color dot (STUDIO-681 §1.3).
export function Pill({ variant, className, children, ...rest }: PillProps) {
  return (
    <span className={cn("pill", variant, className)} {...rest}>
      <span className="d" aria-hidden="true" />
      {children}
    </span>
  );
}
