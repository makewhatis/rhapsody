import type { ReactNode } from "react";
import { cn } from "@/lib/utils";
import { InfoIcon, WarnIcon } from "./icons";

export type NoteVariant = "warn" | "info";

export interface NoteProps {
  variant?: NoteVariant;
  /** Override the leading glyph; defaults to the variant's own icon. */
  icon?: ReactNode;
  className?: string;
  children?: ReactNode;
}

// Note — inline callout (STUDIO-681 §1.3). `warn` carries the amber starvation/danger
// treatment (e.g. the sub-15000ms turn-timeout warning of §7); `info` is the quiet one.
export function Note({ variant = "info", icon, className, children }: NoteProps) {
  return (
    <div className={cn("note", variant, className)} role="note">
      {icon ?? (variant === "warn" ? <WarnIcon /> : <InfoIcon />)}
      <div>{children}</div>
    </div>
  );
}
