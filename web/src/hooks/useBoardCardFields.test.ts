import { describe, expect, it } from "vitest";
import type { ViewStorage } from "@/hooks/useJobsViewMode";
import {
  BOARD_CARD_FIELDS,
  BOARD_CARD_FIELDS_KEY,
  DEFAULT_BOARD_CARD_FIELDS,
  boardCardFieldsAreDefault,
  readBoardCardFields,
  writeBoardCardFields,
} from "@/hooks/useBoardCardFields";

// STUDIO-932 — the five card-field chips are a per-viewer preference, so the read/write rules are
// pinned against a fake storage exactly as `useJobsViewMode` and `useBoardLaneWidth` are. The
// throwing-storage case is the one the ticket calls out by name: a preference must never cost the
// operator the page.

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

describe("the remembered board card fields (STUDIO-932)", () => {
  it("shows every field when nothing is stored", () => {
    expect(readBoardCardFields(fakeStorage())).toEqual(DEFAULT_BOARD_CARD_FIELDS);
  });

  it("round-trips every field through storage", () => {
    const storage = fakeStorage();
    for (const field of BOARD_CARD_FIELDS) {
      const off = { ...DEFAULT_BOARD_CARD_FIELDS, [field.id]: false };
      writeBoardCardFields(storage, off);
      expect(readBoardCardFields(storage)).toEqual(off);
    }
  });

  it("round-trips an all-hidden choice, which is a real choice and not the absence of one", () => {
    const storage = fakeStorage();
    const none = BOARD_CARD_FIELDS.reduce(
      (acc, field) => ({ ...acc, [field.id]: false }),
      {} as typeof DEFAULT_BOARD_CARD_FIELDS,
    );
    writeBoardCardFields(storage, none);
    expect(storage.data[BOARD_CARD_FIELDS_KEY]).toBe("");
    expect(readBoardCardFields(storage)).toEqual(none);
  });

  it("ignores an unknown stored id rather than letting it turn a field on", () => {
    const storage = fakeStorage({ [BOARD_CARD_FIELDS_KEY]: "assignee,closedAt" });
    const read = readBoardCardFields(storage);
    expect(read.assignee).toBe(true);
    expect(read.project).toBe(false);
  });

  it("knows the default from a customised set", () => {
    expect(boardCardFieldsAreDefault(DEFAULT_BOARD_CARD_FIELDS)).toBe(true);
    expect(boardCardFieldsAreDefault({ ...DEFAULT_BOARD_CARD_FIELDS, reviews: false })).toBe(false);
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
    expect(readBoardCardFields(hostile)).toEqual(DEFAULT_BOARD_CARD_FIELDS);
    expect(() => writeBoardCardFields(hostile, { ...DEFAULT_BOARD_CARD_FIELDS, reviews: false })).not.toThrow();
  });

  it("survives storage being absent altogether", () => {
    expect(readBoardCardFields(undefined)).toEqual(DEFAULT_BOARD_CARD_FIELDS);
    expect(() => writeBoardCardFields(undefined, DEFAULT_BOARD_CARD_FIELDS)).not.toThrow();
  });
});
