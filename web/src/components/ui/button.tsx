import * as React from "react";
import { Slot } from "@radix-ui/react-slot";
import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/utils";

// Button — the Podium button set (P10-D1). The package variants (primary / ghost / subtle /
// danger / link) and sizes (sm / md) sit alongside the pre-existing shadcn variants
// (default / outline / secondary, sizes default / lg / icon) that the legacy dashboard still
// uses, so nothing is forked.
//
// The CVA default stays variant="default"/size="default" (NOT the package's subtle/md
// default) so the legacy dashboard's bare <Button> call sites keep their primary look; new UI
// passes explicit variants.
//
// NOTE on key order: `size` is declared before `variant` so that variant classes are
// emitted last and win twMerge conflicts — this lets `link` collapse the height/padding
// (`h-auto p-0`) that a size would otherwise impose.
const buttonVariants = cva(
  [
    "inline-flex items-center justify-center gap-[7px] whitespace-nowrap select-none",
    "rounded-[var(--r-ctrl)] border border-transparent font-medium leading-none tracking-[-0.01em]",
    "transition-all duration-150 ease-[cubic-bezier(.2,.7,.2,1)] cursor-pointer",
    "focus-visible:outline-none focus-visible:ring-[3px] focus-visible:ring-[var(--focus-ring)]",
    "disabled:cursor-default disabled:opacity-45 disabled:[filter:saturate(.4)] disabled:pointer-events-none",
    "[&_svg]:shrink-0",
  ],
  {
    variants: {
      size: {
        default: "h-9 px-4 py-2 text-sm",
        sm: "h-[30px] px-[11px] text-[12.5px]",
        md: "h-9 px-[15px] text-[13.5px]",
        lg: "h-10 px-6 text-sm",
        icon: "h-9 w-9",
      },
      variant: {
        // Podium variants (rust accent)
        primary:
          "bg-[var(--rust)] text-[var(--on-rust)] font-semibold hover:bg-[var(--rust-hover)]",
        subtle:
          "bg-[rgba(255,255,255,.03)] text-[var(--btn-label)] border-[var(--hair-control)] hover:bg-[rgba(255,255,255,.06)] hover:border-[var(--hair-strong)] hover:text-[var(--ink)]",
        ghost:
          "bg-transparent text-[var(--btn-label)] border-transparent hover:bg-[rgba(255,255,255,.04)] hover:text-[var(--ink)]",
        danger:
          "bg-transparent text-[var(--red)] border-[var(--border-danger)] hover:bg-[var(--tint-red)]",
        link: "bg-transparent text-[var(--rust-text)] h-auto p-0 font-medium hover:text-[var(--rust-hover)]",
        // legacy shadcn variants (still used by the Runs dashboard components)
        default: "bg-[var(--primary)] text-[var(--primary-foreground)] hover:opacity-90",
        outline:
          "border-[var(--border)] bg-transparent hover:bg-[var(--accent)] hover:text-[var(--accent-foreground)]",
        secondary: "bg-[var(--secondary)] text-[var(--secondary-foreground)] hover:opacity-90",
      },
    },
    defaultVariants: { variant: "default", size: "default" },
  },
);

type ButtonIcon = React.ComponentType<{ size?: number | string; className?: string }>;

export interface ButtonProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement>,
    VariantProps<typeof buttonVariants> {
  asChild?: boolean;
  /** Optional leading icon component (sized to the button). */
  icon?: ButtonIcon;
  /** Render a muted "Soon" badge to mark not-yet-available actions. */
  comingSoon?: boolean;
}

const Button = React.forwardRef<HTMLButtonElement, ButtonProps>(
  ({ className, variant, size, asChild = false, icon: Icon, comingSoon, children, ...props }, ref) => {
    const Comp = asChild ? Slot : "button";
    const iconSize = size === "sm" ? 14 : 15;
    return (
      <Comp className={cn(buttonVariants({ variant, size }), className)} ref={ref} {...props}>
        {asChild ? (
          children
        ) : (
          <>
            {Icon ? <Icon size={iconSize} /> : null}
            {children}
            {comingSoon ? (
              <span
                className="ml-0.5 rounded-[5px] px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-[.04em] text-[var(--tx-3)]"
                style={{ background: "rgba(255,255,255,.05)" }}
              >
                Soon
              </span>
            ) : null}
          </>
        )}
      </Comp>
    );
  },
);
Button.displayName = "Button";

export { Button, buttonVariants };
