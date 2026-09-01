import type { HTMLAttributes, ReactNode } from "react";
import { cn } from "@/lib/utils";

/** `plain` is a ticket key, `pr` a pull request, `sha` a commit or run id. */
export type TicketChipVariant = "plain" | "pr" | "sha";

export interface TicketChipProps extends HTMLAttributes<HTMLSpanElement> {
  variant?: TicketChipVariant;
  children?: ReactNode;
}

// TicketChip (`.tk`) — the mono key chip (STUDIO-681 §1.3). Mono is not decorative here:
// §1.2 requires ids, ticket keys and SHAs to render in IBM Plex Mono so they stay
// scannable in a column.
export function TicketChip({ variant = "plain", className, children, ...rest }: TicketChipProps) {
  return (
    <span className={cn("tk", variant !== "plain" && variant, className)} {...rest}>
      {children}
    </span>
  );
}
