// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

// STUDIO-765 — the console's one seam for leaving the app.
//
// In the packaged desktop app a plain `<a target="_blank">` is a NO-OP: wry never hands the
// URL to the OS, so the click does nothing while the href still copies. These pin the two
// halves of the fix — the click reaches `openExternal`, and the anchor keeps everything that
// makes it an anchor (copy, right-click, keyboard) — plus the one href shape that must NOT be
// routed, because the host command rejects it.

const h = vi.hoisted(() => ({ openExternal: vi.fn() }));

vi.mock("@/lib/bindings", async (orig) => {
  const actual = await orig<typeof import("@/lib/bindings")>();
  return { ...actual, ...h };
});

const { ExternalLink } = await import("./ExternalLink");

afterEach(() => {
  cleanup();
  h.openExternal.mockClear();
});

describe("ExternalLink", () => {
  it("keeps the href, so copy, right-click and keyboard still work", () => {
    render(
      <ExternalLink href="https://linear.app/studio49/issue/STUDIO-765">Open ticket</ExternalLink>,
    );
    const link = screen.getByRole("link", { name: "Open ticket" });
    expect(link.getAttribute("href")).toBe("https://linear.app/studio49/issue/STUDIO-765");
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.getAttribute("rel")).toContain("noreferrer");
    expect(link.getAttribute("rel")).toContain("noopener");
  });

  it("hands a click to openExternal instead of letting the webview swallow it", () => {
    render(<ExternalLink href="https://github.com/o/r/pull/1">View PR</ExternalLink>);
    const click = new MouseEvent("click", { bubbles: true, cancelable: true });
    const delivered = fireEvent(screen.getByRole("link", { name: "View PR" }), click);
    expect(h.openExternal).toHaveBeenCalledWith("https://github.com/o/r/pull/1");
    // Prevented, so the webview never tries the navigation it would drop on the floor.
    expect(delivered).toBe(false);
    expect(click.defaultPrevented).toBe(true);
  });

  it("leaves a mailto link to the browser, because the host command refuses that scheme", () => {
    // `windowserver::open_external` rejects any scheme but http/https, so routing a mailto
    // there would swap a dead click for a rejected promise. It stays a plain anchor.
    render(<ExternalLink href="mailto:a@b.c">mail</ExternalLink>);
    const click = new MouseEvent("click", { bubbles: true, cancelable: true });
    fireEvent(screen.getByRole("link", { name: "mail" }), click);
    expect(h.openExternal).not.toHaveBeenCalled();
    expect(click.defaultPrevented).toBe(false);
  });

  // The props type omits `onClick`/`target`/`rel`, so this shape is a compile error at a real call
  // site — the cast is what lets the test assert the RUNTIME half of that guarantee as well.
  it("cannot have its seam displaced by a call site that smuggles its own onClick past the types", () => {
    const theirs = vi.fn();
    const props = { onClick: theirs, target: "_self" } as unknown as { className: string };
    render(
      <ExternalLink {...props} href="https://example.com/pr/2">
        smuggled
      </ExternalLink>,
    );
    const link = screen.getByRole("link", { name: "smuggled" });
    expect(link.getAttribute("target")).toBe("_blank");
    const ev = new MouseEvent("click", { bubbles: true, cancelable: true });
    fireEvent(link, ev);
    expect(theirs).not.toHaveBeenCalled();
    expect(h.openExternal).toHaveBeenCalledWith("https://example.com/pr/2");
    expect(ev.defaultPrevented).toBe(true);
  });

  it("passes the caller's class through, so a link can still be a button or a chip", () => {
    render(
      <ExternalLink className="btn sec" href="https://example.com">
        x
      </ExternalLink>,
    );
    expect(screen.getByRole("link", { name: "x" }).className).toBe("btn sec");
  });
});
