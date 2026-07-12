import { describe, expect, it, vi } from "vitest";
import { overrideError, runOverrideSave } from "./ToolDoctor";

describe("overrideError", () => {
  it("uses the Error message", () => {
    expect(overrideError(new Error("not executable"))).toBe("not executable");
  });
  it("stringifies non-Errors", () => {
    expect(overrideError("prefs write failed")).toBe("prefs write failed");
  });
  it("falls back when empty", () => {
    expect(overrideError(new Error("  "))).toBe("Failed to save override.");
  });
});

describe("runOverrideSave", () => {
  function harness(persist: (name: string, path: string) => Promise<void>) {
    const onChanged = vi.fn();
    let error: string | null = null;
    const saving: boolean[] = [];
    return {
      onChanged,
      error: () => error,
      saving,
      run: () =>
        runOverrideSave({
          name: "claude",
          path: " /bin/claude ",
          persist,
          onChanged,
          setSaving: (v) => saving.push(v),
          setError: (v) => {
            error = v;
          },
        }),
    };
  }

  it("on rejection: surfaces the error and does NOT call onChanged", async () => {
    const h = harness(() => Promise.reject(new Error("path not executable")));
    await h.run();
    expect(h.error()).toBe("path not executable");
    expect(h.onChanged).not.toHaveBeenCalled();
    // saving was toggled on then off so the row stays editable.
    expect(h.saving).toEqual([true, false]);
  });

  it("on success: clears the error, trims the path, and calls onChanged", async () => {
    const persist = vi.fn().mockResolvedValue(undefined);
    const h = harness(persist);
    await h.run();
    expect(persist).toHaveBeenCalledWith("claude", "/bin/claude");
    expect(h.error()).toBeNull();
    expect(h.onChanged).toHaveBeenCalledTimes(1);
    expect(h.saving).toEqual([true, false]);
  });
});
