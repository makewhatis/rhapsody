import type { ReactNode, SelectHTMLAttributes } from "react";
import { cn } from "@/lib/utils";

export interface SelectOption {
  value: string;
  label?: ReactNode;
}

export interface SelectProps extends SelectHTMLAttributes<HTMLSelectElement> {
  /** A bare string is its own value and label ("alice"). */
  options: readonly (SelectOption | string)[];
  /** Class for the `.selwrap` positioning span, which draws the caret. */
  wrapperClassName?: string;
}

// Select — styled native `<select>` (STUDIO-681 §1.3). Native on purpose: the room's
// teammate filter and the memory page's ticket filter must scale to N entries, which is
// what a native listbox does well and a chip row does not (§5, §6).
export function Select({ options, className, wrapperClassName, ...rest }: SelectProps) {
  return (
    <span className={cn("selwrap", wrapperClassName)}>
      <select className={cn("sel", className)} {...rest}>
        {options.map((option) => {
          const { value, label } = typeof option === "string" ? { value: option, label: option } : option;
          return (
            <option key={value} value={value}>
              {label ?? value}
            </option>
          );
        })}
      </select>
    </span>
  );
}
