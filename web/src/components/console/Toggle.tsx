import { cn } from "@/lib/utils";

export interface ToggleProps {
  pressed: boolean;
  onChange: (next: boolean) => void;
  /** The 34x19 variant used in dense setting rows (`.toggle.sm`). */
  small?: boolean;
  /** Required: the switch renders no text of its own, so it needs an accessible name. */
  label: string;
  disabled?: boolean;
  className?: string;
}

// Toggle — pill switch (STUDIO-681 §1.3). `aria-pressed` rather than a checkbox, matching
// the prototype's markup and the Chip/Seg convention used across the console.
export function Toggle({ pressed, onChange, small, label, disabled, className }: ToggleProps) {
  return (
    <button
      type="button"
      aria-pressed={pressed}
      aria-label={label}
      disabled={disabled}
      className={cn("toggle", small && "sm", className)}
      onClick={() => onChange(!pressed)}
    />
  );
}
