import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

/** The coarse step, used once a value has reached four digits. */
export const STEPPER_LARGE_STEP = 1000;

/** A non-finite value (a cleared input, a bad API payload) reads as the minimum. */
function normalize(value: number, min: number): number {
  return Number.isFinite(value) ? value : min;
}

/**
 * One step up: coarse at and above 1000, fine below.
 * Mirrors the prototype's `n + (n >= 1000 ? 1000 : 1)`.
 */
export function stepperIncrement(value: number, min = 0): number {
  const n = normalize(value, min);
  return n + (n >= STEPPER_LARGE_STEP ? STEPPER_LARGE_STEP : 1);
}

/**
 * One step down, clamped at `min`. Mirrors the prototype's
 * `Math.max(0, n - (n > 1000 ? 1000 : 1))` — note `>` rather than `>=`, so 1000 is the
 * floor of the coarse range and steps down to 999 rather than to zero.
 */
export function stepperDecrement(value: number, min = 0): number {
  const n = normalize(value, min);
  return Math.max(min, n - (n > STEPPER_LARGE_STEP ? STEPPER_LARGE_STEP : 1));
}

export interface StepperProps {
  value: number;
  onChange: (next: number) => void;
  /** Floor for both the buttons and typed input. `0` matches the prototype. */
  min?: number;
  /** Trailing unit label ("ms", "reviewers"). */
  unit?: ReactNode;
  /** Required: the control has no visible label, so it needs an accessible name. */
  label: string;
  className?: string;
}

// Stepper — numeric −/value/+ (STUDIO-681 §1.3/§10 box 1.5). `role="spinbutton"` on a text
// input rather than `type="number"`: the prototype styles a text field, and the coarse
// 1000-step is ours, not the browser's.
export function Stepper({ value, onChange, min = 0, unit, label, className }: StepperProps) {
  return (
    <>
      <span className={cn("stepper", className)}>
        <button type="button" aria-label={`Decrease ${label}`} onClick={() => onChange(stepperDecrement(value, min))}>
          –
        </button>
        <input
          type="text"
          inputMode="numeric"
          role="spinbutton"
          aria-label={label}
          aria-valuenow={Number.isFinite(value) ? value : undefined}
          aria-valuemin={min}
          value={Number.isFinite(value) ? value : min}
          onChange={(event) => {
            const digits = event.target.value.replace(/[^0-9]/g, "");
            const parsed = Number.parseInt(digits, 10);
            onChange(Number.isNaN(parsed) ? min : Math.max(min, parsed));
          }}
        />
        <button type="button" aria-label={`Increase ${label}`} onClick={() => onChange(stepperIncrement(value, min))}>
          +
        </button>
      </span>
      {unit === undefined ? null : <span className="unit">{unit}</span>}
    </>
  );
}
