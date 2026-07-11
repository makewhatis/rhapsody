// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { STATUS_META, StatusChip } from "@/components/ui/status-chip";

afterEach(cleanup);

describe("status-chip waiting status (INF-320)", () => {
  it("defines a distinct 'waiting' palette entry (NOT the idle fallback)", () => {
    expect(STATUS_META.waiting).toBeDefined();
    // It must be its own entry, not silently resolving to the idle fallback.
    expect(STATUS_META.waiting).not.toBe(STATUS_META.idle);
    expect(STATUS_META.waiting.label).toBe("waiting");
  });

  it("renders the waiting label on the chip", () => {
    render(<StatusChip status="waiting" />);
    expect(screen.getByText("waiting")).toBeTruthy();
  });

  it("uses a non-error (sky, not red) treatment so it reads as held-by-design", () => {
    // A held-on-predecessor ticket is benign — it must not borrow the failed/error red palette.
    expect(STATUS_META.waiting.color).not.toBe(STATUS_META.failed.color);
    expect(STATUS_META.waiting.color).toBe("var(--sky)");
  });
});
