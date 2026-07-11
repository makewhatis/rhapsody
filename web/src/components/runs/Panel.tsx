import * as React from "react";

// Panel — the design package's bare `Card` (ui.jsx) as a thin inline-token surface. The repo's
// exported `Card` is the shadcn variant (Tailwind `rounded-xl p-6`, no `pad`/inline-padding
// hook), so the Runs re-skin uses this local surface to match `runs.jsx`'s `Card{style:{…}}`
// call sites (paddingless lists, custom-padded tiles/meta grids) detail-for-detail. Callers set
// their own padding/layout via `style`.
export function Panel({ style, children, ...rest }: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      style={{
        background: "var(--bg-card)",
        border: "1px solid var(--line)",
        borderRadius: "var(--r-card)",
        boxShadow: "var(--shadow-card)",
        ...style,
      }}
      {...rest}
    >
      {children}
    </div>
  );
}
