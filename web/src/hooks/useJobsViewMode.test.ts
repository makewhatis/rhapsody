import { describe, expect, it } from "vitest";
import { JOBS_VIEW_KEY, readJobsView, writeJobsView, type ViewStorage } from "@/hooks/useJobsViewMode";

// STUDIO-925 — the board is an additional view, so "remember the choice" has to survive a remount
// (opening a job unmounts Jobs) and a reload. A fake storage keeps the read/write rules testable in
// the node environment; the browser wiring is one `window.localStorage` accessor.

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

describe("the remembered Jobs view (STUDIO-925)", () => {
  it("defaults to the table when nothing is stored", () => {
    expect(readJobsView(fakeStorage())).toBe("list");
  });

  it("reads back a stored board choice", () => {
    expect(readJobsView(fakeStorage({ [JOBS_VIEW_KEY]: "board" }))).toBe("board");
  });

  it("treats an unknown stored value as the table", () => {
    expect(readJobsView(fakeStorage({ [JOBS_VIEW_KEY]: "gallery" }))).toBe("list");
  });

  it("writes the choice back under the namespaced key", () => {
    const storage = fakeStorage();
    writeJobsView(storage, "board");
    expect(storage.data[JOBS_VIEW_KEY]).toBe("board");
    writeJobsView(storage, "list");
    expect(storage.data[JOBS_VIEW_KEY]).toBe("list");
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
    expect(readJobsView(hostile)).toBe("list");
    expect(() => writeJobsView(hostile, "board")).not.toThrow();
  });

  it("survives storage being absent altogether", () => {
    expect(readJobsView(undefined)).toBe("list");
    expect(() => writeJobsView(undefined, "board")).not.toThrow();
  });
});
