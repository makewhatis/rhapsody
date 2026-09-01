import type { HTMLAttributes, ReactNode } from "react";
import { cn } from "@/lib/utils";

export interface CardProps extends Omit<HTMLAttributes<HTMLElement>, "title"> {
  title?: ReactNode;
  /** Secondary text beside the title ("newest first · 3 dispatches"). */
  sub?: ReactNode;
  /** Right-hand slot of the header — a link, a chip, an action. */
  right?: ReactNode;
  children?: ReactNode;
}

// Card — bordered panel with an optional `.hd` header (STUDIO-681 §1.3). The header is
// omitted entirely when none of its three slots is given, so a bare Card is just the panel.
export function Card({ title, sub, right, className, children, ...rest }: CardProps) {
  const hasHeader = title !== undefined || sub !== undefined || right !== undefined;
  return (
    <section className={cn("card", className)} {...rest}>
      {hasHeader ? (
        <div className="hd">
          {title === undefined ? null : <h2>{title}</h2>}
          {sub === undefined ? null : <span className="sub">{sub}</span>}
          {right === undefined ? null : <span className="rt">{right}</span>}
        </div>
      ) : null}
      {children}
    </section>
  );
}
