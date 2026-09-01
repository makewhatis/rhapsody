import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

export interface SegOption {
  value: string;
  label?: ReactNode;
  disabled?: boolean;
}

export interface SegProps {
  options: readonly (SegOption | string)[];
  value: string;
  onChange: (value: string) => void;
  /** The accent treatment (`.seg.acc`) — amber-on-soft rather than raised grey. */
  accent?: boolean;
  className?: string;
  "aria-label"?: string;
}

// Seg — segmented button group (STUDIO-681 §1.3). Selection is `aria-pressed`, matching
// the prototype, so a filter's state is readable by assistive tech and by a test.
export function Seg({ options, value, onChange, accent, className, ...rest }: SegProps) {
  return (
    <div className={cn("seg", accent && "acc", className)} role="group" {...rest}>
      {options.map((option) => {
        const opt = typeof option === "string" ? { value: option, label: option, disabled: false } : option;
        return (
          <button
            key={opt.value}
            type="button"
            disabled={opt.disabled}
            aria-pressed={opt.value === value}
            onClick={() => onChange(opt.value)}
          >
            {opt.label ?? opt.value}
          </button>
        );
      })}
    </div>
  );
}
