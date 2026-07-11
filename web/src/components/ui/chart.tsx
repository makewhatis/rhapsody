import * as React from "react";
import * as RechartsPrimitive from "recharts";
import { cn } from "@/lib/utils";

export type ChartConfig = Record<
  string,
  { label?: React.ReactNode; color?: string }
>;

const ChartContext = React.createContext<{ config: ChartConfig } | null>(null);

function useChart() {
  const ctx = React.useContext(ChartContext);
  if (!ctx) throw new Error("useChart must be used within <ChartContainer>");
  return ctx;
}

const ChartContainer = React.forwardRef<
  HTMLDivElement,
  React.HTMLAttributes<HTMLDivElement> & {
    config: ChartConfig;
    children: React.ComponentProps<
      typeof RechartsPrimitive.ResponsiveContainer
    >["children"];
  }
>(({ id, className, children, config, ...props }, ref) => {
  const uniqueId = React.useId();
  const chartId = `chart-${id || uniqueId.replace(/:/g, "")}`;
  const colorVars = Object.entries(config)
    .filter(([, v]) => v.color)
    .map(([k, v]) => `--color-${k}: ${v.color};`)
    .join(" ");
  return (
    <ChartContext.Provider value={{ config }}>
      <div
        ref={ref}
        data-chart={chartId}
        className={cn("flex aspect-video justify-center text-xs", className)}
        {...props}
      >
        <style
          dangerouslySetInnerHTML={{
            __html: `[data-chart=${chartId}] { ${colorVars} }`,
          }}
        />
        <RechartsPrimitive.ResponsiveContainer>
          {children}
        </RechartsPrimitive.ResponsiveContainer>
      </div>
    </ChartContext.Provider>
  );
});
ChartContainer.displayName = "ChartContainer";

const ChartTooltip = RechartsPrimitive.Tooltip;

function ChartTooltipContent({
  active,
  payload,
  label,
}: {
  active?: boolean;
  payload?: Array<{ name?: string; value?: number | string; color?: string }>;
  label?: React.ReactNode;
}) {
  const { config } = useChart();
  if (!active || !payload?.length) return null;
  return (
    <div className="rounded-md border border-[var(--border)] bg-[var(--popover)] px-2.5 py-1.5 text-xs shadow-md">
      {label != null && <div className="mb-1 font-medium">{label}</div>}
      {payload.map((item, i) => {
        const key = item.name ?? `series-${i}`;
        const labelText = config[key]?.label ?? key;
        return (
          <div key={i} className="flex items-center gap-2">
            <span
              className="inline-block h-2 w-2 rounded-full"
              style={{ backgroundColor: item.color }}
            />
            <span className="text-[var(--muted-foreground)]">{labelText}</span>
            <span className="ml-auto font-mono">{item.value}</span>
          </div>
        );
      })}
    </div>
  );
}

export { ChartContainer, ChartTooltip, ChartTooltipContent, useChart };
