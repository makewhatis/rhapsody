import { describe, expect, it } from "vitest";
import type { ViewStorage } from "@/hooks/useJobsViewMode";
import {
  BOARD_LANE_WIDTH_KEY,
  readBoardLaneWidth,
  writeBoardLaneWidth,
} from "@/hooks/useBoardLaneWidth";

// STUDIO-930 — lane width is a per-viewer preference, so the read/write rules are pinned against a
// fake storage exactly as `useJobsViewMode` is.

function fakeStorage(seed: Record<string, string> = {}): ViewStorage & { data: Record<string, string> } {
  const data = { ...seed };
  return {
    data,
    getItem: (k) => data[k] ?? null,
    setItem: (k, v) => {
      data[k] = v;
    },
  };
}

describe("the remembered board lane width (STUDIO-930)", () => {
  it("defaults when nothing is stored", () => {
    expect(readBoardLaneWidth(fakeStorage())).toBe("default");
  });

  it("round-trips every width through storage", () => {
    const storage = fakeStorage();
    for (const width of ["compact", "wide", "default"] as const) {
      writeBoardLaneWidth(storage, width);
      expect(storage.data[BOARD_LANE_WIDTH_KEY]).toBe(width);
      expect(readBoardLaneWidth(storage)).toBe(width);
    }
  });

  it("treats an unknown stored value as the default", () => {
    expect(readBoardLaneWidth(fakeStorage({ [BOARD_LANE_WIDTH_KEY]: "huge" }))).toBe("default");
  });

  it("survives storage that throws — the page must still render", () => {
    const hostile: ViewStorage = {
      getItem: () => {
        throw new Error("denied");
      },
      setItem: () => {
        throw new Error("denied");
      },
    };
    expect(readBoardLaneWidth(hostile)).toBe("default");
    expect(() => writeBoardLaneWidth(hostile, "wide")).not.toThrow();
  });

  it("survives storage being absent altogether", () => {
    expect(readBoardLaneWidth(undefined)).toBe("default");
    expect(() => writeBoardLaneWidth(undefined, "wide")).not.toThrow();
  });
});
