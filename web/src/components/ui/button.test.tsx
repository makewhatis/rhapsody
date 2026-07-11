// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent } from "@testing-library/react";
import { Button } from "@/components/ui/button";
import { Check } from "@/components/ui/icons";

afterEach(cleanup);

describe("Button", () => {
  it("renders its label and is clickable", () => {
    const onClick = vi.fn();
    render(<Button onClick={onClick}>Save</Button>);
    const btn = screen.getByRole("button", { name: "Save" });
    fireEvent.click(btn);
    expect(onClick).toHaveBeenCalledOnce();
  });

  it("applies the emerald primary variant", () => {
    render(<Button variant="primary">Go</Button>);
    const btn = screen.getByRole("button", { name: "Go" });
    expect(btn.className).toContain("bg-[var(--em-bright)]");
    expect(btn.className).toContain("text-[var(--on-em)]");
  });

  it("applies the danger variant", () => {
    render(<Button variant="danger">Stop</Button>);
    expect(screen.getByRole("button", { name: "Stop" }).className).toContain("text-[var(--red)]");
  });

  it("collapses height/padding for the link variant", () => {
    render(
      <Button variant="link" size="md">
        more
      </Button>,
    );
    expect(screen.getByRole("button", { name: "more" }).className).toContain("p-0");
  });

  it("honors sm vs md sizing", () => {
    const { rerender } = render(<Button size="sm">x</Button>);
    expect(screen.getByRole("button").className).toContain("h-[30px]");
    rerender(<Button size="md">x</Button>);
    expect(screen.getByRole("button").className).toContain("px-[15px]");
  });

  it("disables and dims when disabled", () => {
    render(<Button disabled>nope</Button>);
    const btn = screen.getByRole("button", { name: "nope" }) as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
    expect(btn.className).toContain("disabled:opacity-45");
  });

  it('renders a "Soon" badge when comingSoon', () => {
    render(<Button comingSoon>Later</Button>);
    expect(screen.getByText("Soon")).toBeTruthy();
  });

  it("renders a leading icon", () => {
    const { container } = render(<Button icon={Check}>With icon</Button>);
    expect(container.querySelector("svg")).toBeTruthy();
  });

  it("sizes the leading icon 14 (sm) vs 15 (md)", () => {
    const sm = render(
      <Button size="sm" icon={Check}>
        x
      </Button>,
    );
    expect(sm.container.querySelector("svg")?.getAttribute("width")).toBe("14");
    sm.unmount();
    const md = render(
      <Button size="md" icon={Check}>
        x
      </Button>,
    );
    expect(md.container.querySelector("svg")?.getAttribute("width")).toBe("15");
  });

  it("applies the subtle and ghost variant surfaces", () => {
    const { rerender } = render(<Button variant="subtle">s</Button>);
    expect(screen.getByRole("button").className).toContain("bg-[var(--bg-raised)]");
    rerender(<Button variant="ghost">g</Button>);
    expect(screen.getByRole("button").className).toContain("border-[var(--line-strong)]");
  });
});
