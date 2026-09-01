import type { ButtonHTMLAttributes } from "react";
import { cn } from "@/lib/utils";

/** `.btn` on its own is the accent (outlined-amber) button the prototype uses most. */
export type ButtonVariant = "accent" | "pri" | "sec" | "link";

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant;
}

// Button — STUDIO-681 §1.3. Defaults to `type="button"`: these sit inside the manage-team
// form, where a stray submit would post a half-filled roster.
export function Button({ variant = "accent", className, type = "button", ...rest }: ButtonProps) {
  return <button type={type} className={cn("btn", variant !== "accent" && variant, className)} {...rest} />;
}
