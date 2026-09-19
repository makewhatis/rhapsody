// @vitest-environment jsdom
import { readFileSync } from "node:fs";
import path from "node:path";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import {
  BOARD_CARD_FIELDS,
  DEFAULT_BOARD_CARD_FIELDS,
  type BoardCardField,
  type BoardCardFields,
} from "@/hooks/useBoardCardFields";
import type { BoardLaneWidth } from "@/hooks/useBoardLaneWidth";
import type { JobsViewMode } from "@/hooks/useJobsViewMode";
import { DisplayOptions } from "./DisplayOptions";

// STUDIO-932 — the popover's own behaviour, driven directly: the two dismissals, focus return, the
// Board-only lane-width row, the non-default dot and the reset. The persistence and the real view
// wiring are pinned elsewhere; this file is the control itself.

interface Callbacks {
  onView: (v: JobsViewMode) => void;
  onLaneWidth: (w: BoardLaneWidth) => void;
  onToggleField: (f: BoardCardField, on: boolean) => void;
  onReset: () => void;
}

function setup(over: { view?: JobsViewMode; laneWidth?: BoardLaneWidth; fields?: BoardCardFields } = {}): Callbacks {
  const calls: Callbacks = {
    onView: vi.fn(),
    onLaneWidth: vi.fn(),
    onToggleField: vi.fn(),
    onReset: vi.fn(),
  };
  function Wrapper() {
    const [view, setView] = useState<JobsViewMode>(over.view ?? "list");
    const [laneWidth, setLaneWidth] = useState<BoardLaneWidth>(over.laneWidth ?? "default");
    const [fields, setFields] = useState<BoardCardFields>(over.fields ?? DEFAULT_BOARD_CARD_FIELDS);
    return (
      <DisplayOptions
        view={view}
        onView={(v) => {
          setView(v);
          calls.onView(v);
        }}
        laneWidth={laneWidth}
        onLaneWidth={(w) => {
          setLaneWidth(w);
          calls.onLaneWidth(w);
        }}
        fields={fields}
        onToggleField={(f, on) => {
          setFields((prev) => ({ ...prev, [f]: on }));
          calls.onToggleField(f, on);
        }}
        onReset={() => {
          setView("list");
          setLaneWidth("default");
          setFields({ ...DEFAULT_BOARD_CARD_FIELDS });
          calls.onReset();
        }}
      />
    );
  }
  render(<Wrapper />);
  return calls;
}

afterEach(cleanup);

describe("the display-options popover (STUDIO-932)", () => {
  it("opens from the header trigger and closes on Escape, returning focus to the trigger", () => {
    setup();
    const trigger = screen.getByRole("button", { name: "Display options" });
    expect(screen.queryByRole("dialog", { name: "Display options" })).toBeNull();

    fireEvent.click(trigger);
    expect(screen.getByRole("dialog", { name: "Display options" })).toBeTruthy();
    // Focus moved into the popover, so the options are reachable from the keyboard.
    expect((document.activeElement as HTMLElement).closest(".dpop")).not.toBeNull();

    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("dialog", { name: "Display options" })).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it("closes on an outside click", () => {
    setup();
    fireEvent.click(screen.getByRole("button", { name: "Display options" }));
    expect(screen.getByRole("dialog", { name: "Display options" })).toBeTruthy();

    fireEvent.mouseDown(document.body);
    expect(screen.queryByRole("dialog", { name: "Display options" })).toBeNull();
  });

  it("omits the lane-width row in List mode and shows it in Board mode", () => {
    setup();
    fireEvent.click(screen.getByRole("button", { name: "Display options" }));
    expect(screen.queryByText("Lane width")).toBeNull();
    expect(screen.getByText("View")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Board" }));
    expect(screen.getByText("Lane width")).toBeTruthy();
  });

  it("shows the five card-field chips, in order, as pressed toggles", () => {
    setup();
    fireEvent.click(screen.getByRole("button", { name: "Display options" }));
    const chips = ["Assignee", "Project", "Harness", "Reviews", "Pull request"].map((label) =>
      screen.getByRole("button", { name: label }),
    );
    for (const chip of chips) expect(chip.getAttribute("aria-pressed")).toBe("true");

    fireEvent.click(chips[0]);
    expect(chips[0].getAttribute("aria-pressed")).toBe("false");
  });

  it("draws the non-default dot only when an option differs from its default", () => {
    setup({ view: "board" });
    expect(document.querySelector(".dpdot")).not.toBeNull();
    cleanup();

    setup({ laneWidth: "wide" });
    expect(document.querySelector(".dpdot")).not.toBeNull();
    cleanup();

    setup({ fields: { ...DEFAULT_BOARD_CARD_FIELDS, reviews: false } });
    expect(document.querySelector(".dpdot")).not.toBeNull();
    cleanup();

    setup();
    expect(document.querySelector(".dpdot")).toBeNull();
  });

  it("resets every option from the subtle text button", () => {
    const calls = setup({ view: "board", laneWidth: "wide", fields: { ...DEFAULT_BOARD_CARD_FIELDS, reviews: false } });
    fireEvent.click(screen.getByRole("button", { name: "Display options" }));
    const reset = screen.getByRole("button", { name: "Reset" });
    // A text button, not a filled one — the escape hatch is not a call to action.
    expect(reset.classList.contains("link")).toBe(true);

    fireEvent.click(reset);
    expect(calls.onReset).toHaveBeenCalledOnce();
    expect(document.querySelector(".dpdot")).toBeNull();
    expect(screen.queryByText("Lane width")).toBeNull();
    for (const field of BOARD_CARD_FIELDS) {
      expect(screen.getByRole("button", { name: field.label }).getAttribute("aria-pressed")).toBe("true");
    }
  });
});

// The ticket names its tokens one by one, so the popover being classed is not enough — it has to be
// PAINTED with them. A rule that quietly falls back to a console token would ship an overlay with no
// separation from the board and a grey dot, which no render test would catch.
describe("the display-options popover is painted with the Podium tokens (STUDIO-932)", () => {
  const css = readFileSync(path.resolve(__dirname, "../../theme/console-views.css"), "utf8");

  it("gives the surface the surface token, the card hairline, the card radius and the overlay shadow", () => {
    expect(css).toMatch(/\.dpop \{[^}]*background: var\(--surface\)/);
    expect(css).toMatch(/\.dpop \{[^}]*border: 1px solid var\(--hair-card\)/);
    expect(css).toMatch(/\.dpop \{[^}]*border-radius: var\(--r-card\)/);
    expect(css).toMatch(/\.dpop \{[^}]*box-shadow: var\(--shadow-pop\)/);
  });

  it("rules the Card fields section off from the rows above", () => {
    expect(css).toMatch(/\.dprule \{[^}]*background: var\(--hair-section\)/);
  });

  it("tints an on chip rust and leaves an off chip on the control hairline", () => {
    expect(css).toMatch(/\.dpop \.chip\[aria-pressed="true"\] \{[^}]*background: var\(--tint-rust\)/);
    expect(css).toMatch(/\.dpop \.chip\[aria-pressed="true"\] \{[^}]*color: var\(--rust-text\)/);
    expect(css).toMatch(/\.dpop \.chip\[aria-pressed="true"\] \{[^}]*border-color: transparent/);
    expect(css).toMatch(/\.dpop \.chip \{[^}]*border: 1px solid var\(--hair-control\)/);
    expect(css).toMatch(/\.dpop \.chip \{[^}]*color: var\(--btn-label\)/);
  });

  it("paints the non-default dot rust, never blue", () => {
    expect(css).toMatch(/\.dpdot \{[^}]*background: var\(--rust-text\)/);
  });
});
