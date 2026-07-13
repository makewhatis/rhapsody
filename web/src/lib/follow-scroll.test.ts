import { describe, expect, it } from "vitest";
import { distanceFromBottom, isAtBottom, FOLLOW_THRESHOLD_PX } from "@/lib/follow-scroll";

describe("distanceFromBottom", () => {
  it("is 0 when pinned to the bottom (scrollTop == scrollHeight - clientHeight)", () => {
    expect(distanceFromBottom({ scrollTop: 700, scrollHeight: 1000, clientHeight: 300 })).toBe(0);
  });

  it("grows as the user scrolls up", () => {
    expect(distanceFromBottom({ scrollTop: 0, scrollHeight: 1000, clientHeight: 300 })).toBe(700);
    expect(distanceFromBottom({ scrollTop: 400, scrollHeight: 1000, clientHeight: 300 })).toBe(300);
  });

  it("clamps a rubber-band over-scroll (or a zero-height container) to 0", () => {
    expect(distanceFromBottom({ scrollTop: 999, scrollHeight: 1000, clientHeight: 300 })).toBe(0);
    expect(distanceFromBottom({ scrollTop: 0, scrollHeight: 0, clientHeight: 0 })).toBe(0);
  });
});

describe("isAtBottom", () => {
  it("is true exactly at the bottom", () => {
    expect(isAtBottom({ scrollTop: 700, scrollHeight: 1000, clientHeight: 300 })).toBe(true);
  });

  it("tolerates a small slack within the threshold", () => {
    // 12px up from the bottom — still following (<= 24px slack).
    expect(isAtBottom({ scrollTop: 700 - 12, scrollHeight: 1000, clientHeight: 300 })).toBe(true);
    // exactly at the threshold is inclusive.
    expect(isAtBottom({ scrollTop: 700 - FOLLOW_THRESHOLD_PX, scrollHeight: 1000, clientHeight: 300 })).toBe(true);
  });

  it("is false once scrolled up beyond the threshold", () => {
    expect(isAtBottom({ scrollTop: 700 - (FOLLOW_THRESHOLD_PX + 1), scrollHeight: 1000, clientHeight: 300 })).toBe(false);
    expect(isAtBottom({ scrollTop: 0, scrollHeight: 1000, clientHeight: 300 })).toBe(false);
  });

  it("honors a caller-supplied threshold", () => {
    expect(isAtBottom({ scrollTop: 600, scrollHeight: 1000, clientHeight: 300 }, 100)).toBe(true); // dist 100
    expect(isAtBottom({ scrollTop: 600, scrollHeight: 1000, clientHeight: 300 }, 50)).toBe(false);
  });
});
