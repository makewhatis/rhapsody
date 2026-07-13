// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { StatusDot } from "@/components/ui/status-dot";
import { StatusChip, STATUS_META } from "@/components/ui/status-chip";
import { Pill } from "@/components/ui/pill";
import { Skeleton, SkeletonCard } from "@/components/ui/skeleton";
import { SectionCard } from "@/components/ui/section-card";
import { Divider } from "@/components/ui/divider";
import { Sliders } from "@/components/ui/icons";

afterEach(cleanup);

describe("StatusDot", () => {
  it("animates only when pulse is set", () => {
    const { container, rerender } = render(<StatusDot pulse />);
    const dot = container.firstChild as HTMLElement;
    expect(dot.getAttribute("data-pulse")).toBe("true");
    expect(dot.style.animation).toContain("pulse");
    rerender(<StatusDot />);
    expect((container.firstChild as HTMLElement).style.animation).toBe("none");
  });

  it("paints the requested color", () => {
    const { container } = render(<StatusDot color="var(--red)" />);
    expect((container.firstChild as HTMLElement).style.background).toBe("var(--red)");
  });
});

describe("StatusChip + STATUS_META", () => {
  it("covers every documented run/agent state", () => {
    expect(Object.keys(STATUS_META).sort()).toEqual(
      [
        "completed",
        "continued",
        "failed",
        "idle",
        "interrupted",
        "paused",
        "queued",
        "review",
        "running",
        "stopped",
        "waiting",
      ].sort(),
    );
    expect(STATUS_META.review.label).toBe("in review");
    expect(STATUS_META.running.pulse).toBe(true);
    // display-label mapping: the enum key stays "running", the chip reads "playing".
    expect(STATUS_META.running.label).toBe("playing");
    // stopped uses the amber warn palette (an operator-attention state, not an error).
    expect(STATUS_META.stopped.color).toBe("var(--amber)");
    // waiting (held dependent, INF-320) uses the benign slate palette — not error red, not pulsing.
    expect(STATUS_META.waiting.color).toBe("var(--slate)");
    expect(STATUS_META.waiting.pulse).toBeUndefined();
  });

  it("renders the meta label by default", () => {
    render(<StatusChip status="paused" />);
    expect(screen.getByText("paused")).toBeTruthy();
  });

  it("formats a count using the display label (running → playing)", () => {
    render(<StatusChip status="running" count={3} />);
    expect(screen.getByText("3 playing")).toBeTruthy();
  });

  it("falls back to idle for an unknown status", () => {
    render(<StatusChip status="???" />);
    expect(screen.getByText("idle")).toBeTruthy();
  });
});

describe("Pill", () => {
  it("applies tonal colors", () => {
    const { container } = render(<Pill tone="emerald">Healthy</Pill>);
    const pill = container.firstChild as HTMLElement;
    // legacy "emerald" tone re-points onto the Podium success/sage color.
    expect(pill.style.color).toBe("var(--sage)");
    expect(screen.getByText("Healthy")).toBeTruthy();
  });
});

describe("Skeleton", () => {
  it("shimmers", () => {
    const { container } = render(<Skeleton />);
    expect((container.firstChild as HTMLElement).style.animation).toContain("shimmer");
  });

  it("SkeletonCard renders several shimmer bars", () => {
    render(<SkeletonCard />);
    expect(screen.getAllByTestId("skeleton").length).toBeGreaterThan(3);
  });
});

describe("SectionCard", () => {
  it("renders title, description, icon and action", () => {
    render(
      <SectionCard title="General" desc="Global defaults" icon={Sliders} action={<button>Edit</button>}>
        <div>body</div>
      </SectionCard>,
    );
    expect(screen.getByText("General")).toBeTruthy();
    expect(screen.getByText("Global defaults")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Edit" })).toBeTruthy();
    expect(screen.getByText("body")).toBeTruthy();
  });
});

describe("Divider", () => {
  it("renders a hairline", () => {
    const { container } = render(<Divider />);
    expect((container.firstChild as HTMLElement).style.height).toBe("1px");
  });
});

describe("display primitives — additional state coverage", () => {
  it("StatusDot uses the Podium opacity pulse (2.4s) when live", () => {
    const a = render(<StatusDot pulse />);
    expect((a.container.firstChild as HTMLElement).style.animation).toContain("pulse 2.4s");
    // a static dot carries no animation at all
    const b = render(<StatusDot color="var(--amber)" />);
    expect((b.container.firstChild as HTMLElement).style.animation).toBe("none");
  });

  it("StatusChip honors an explicit label override", () => {
    render(<StatusChip status="running" label="custom" />);
    expect(screen.getByText("custom")).toBeTruthy();
  });

  it("Pill maps every tone to its accent color", () => {
    const tones = [
      ["neutral", "var(--neutral)"],
      ["emerald", "var(--sage)"],
      ["amber", "var(--amber)"],
      ["sky", "var(--slate)"],
      ["rust", "var(--rust-text)"],
      ["sage", "var(--sage)"],
      ["slate", "var(--slate)"],
    ] as const;
    for (const [tone, color] of tones) {
      const { container, unmount } = render(<Pill tone={tone}>{tone}</Pill>);
      expect((container.firstChild as HTMLElement).style.color).toBe(color);
      unmount();
    }
  });

  it("SectionCard omits the icon chip and description when not provided", () => {
    const { container } = render(<SectionCard title="Bare">body</SectionCard>);
    expect(container.querySelector("svg")).toBeNull();
    expect(screen.getByText("body")).toBeTruthy();
  });
});
