// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent, within } from "@testing-library/react";
import { Toggle } from "@/components/ui/toggle";
import { Checkbox } from "@/components/ui/checkbox";
import { Chips } from "@/components/ui/chips";
import { Collapsible } from "@/components/ui/collapsible";
import { Select } from "@/components/ui/select";
import { Boxes } from "@/components/ui/icons";

afterEach(cleanup);

describe("Toggle", () => {
  it("exposes switch role + aria-checked and toggles", () => {
    const onChange = vi.fn();
    const { rerender } = render(<Toggle checked={false} onChange={onChange} aria-label="Pause" />);
    const sw = screen.getByRole("switch", { name: "Pause" });
    expect(sw.getAttribute("aria-checked")).toBe("false");
    fireEvent.click(sw);
    expect(onChange).toHaveBeenCalledWith(true);
    rerender(<Toggle checked onChange={onChange} aria-label="Pause" />);
    expect(screen.getByRole("switch").getAttribute("aria-checked")).toBe("true");
  });

  it("slides the knob when on", () => {
    const { container, rerender } = render(<Toggle checked={false} onChange={() => {}} />);
    const knobOff = (container.querySelector("span") as HTMLElement).style.left;
    rerender(<Toggle checked onChange={() => {}} />);
    const knobOn = (container.querySelector("span") as HTMLElement).style.left;
    expect(knobOff).not.toBe(knobOn);
  });
});

describe("Checkbox", () => {
  it("toggles and shows a check when on", () => {
    const onChange = vi.fn();
    const { container, rerender } = render(<Checkbox checked={false} onChange={onChange} aria-label="ok" />);
    const cb = screen.getByRole("checkbox", { name: "ok" });
    expect(container.querySelector("svg")).toBeNull();
    fireEvent.click(cb);
    expect(onChange).toHaveBeenCalledWith(true);
    rerender(<Checkbox checked onChange={onChange} aria-label="ok" />);
    expect(container.querySelector("svg")).toBeTruthy();
  });
});

describe("Chips", () => {
  it("adds on Enter and on comma, ignoring duplicates", () => {
    const onAdd = vi.fn();
    render(<Chips items={["x"]} onAdd={onAdd} onRemove={() => {}} />);
    const input = screen.getByRole("textbox");
    fireEvent.change(input, { target: { value: "new" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onAdd).toHaveBeenLastCalledWith("new");
    fireEvent.change(input, { target: { value: "two" } });
    fireEvent.keyDown(input, { key: "," });
    expect(onAdd).toHaveBeenLastCalledWith("two");
    // duplicate is ignored
    onAdd.mockClear();
    fireEvent.change(input, { target: { value: "x" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onAdd).not.toHaveBeenCalled();
  });

  it("removes the last chip on Backspace when empty", () => {
    const onRemove = vi.fn();
    render(<Chips items={["a", "b"]} onAdd={() => {}} onRemove={onRemove} />);
    fireEvent.keyDown(screen.getByRole("textbox"), { key: "Backspace" });
    expect(onRemove).toHaveBeenCalledWith("b");
  });

  it("removes via the chip's X button", () => {
    const onRemove = vi.fn();
    render(<Chips items={["a"]} onAdd={() => {}} onRemove={onRemove} />);
    fireEvent.click(screen.getByLabelText("Remove a"));
    expect(onRemove).toHaveBeenCalledWith("a");
  });

  it("does not blur-commit a typed draft when removing a chip", () => {
    const onAdd = vi.fn();
    render(<Chips items={["a"]} onAdd={onAdd} onRemove={() => {}} />);
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "draft" } });
    // The remove control prevents its mousedown default so the input keeps focus — onBlur
    // never fires to commit "draft" as a stray chip before the remove click runs.
    expect(fireEvent.mouseDown(screen.getByLabelText("Remove a"))).toBe(false);
    expect(onAdd).not.toHaveBeenCalled();
  });

  it("renders invalid items in the red state", () => {
    render(<Chips items={["bad@"]} onAdd={() => {}} onRemove={() => {}} invalidItem={(i) => i.includes("@")} />);
    expect((screen.getByText("bad@") as HTMLElement).style.color).toBe("var(--red)");
  });
});

describe("Collapsible", () => {
  it("is closed by default and toggles open", () => {
    render(
      <Collapsible label="Advanced" icon={Boxes} badge={<span>2</span>}>
        <div>hidden body</div>
      </Collapsible>,
    );
    const trigger = screen.getByRole("button", { name: /Advanced/ });
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByText("hidden body")).toBeNull();
    fireEvent.click(trigger);
    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByText("hidden body")).toBeTruthy();
  });
});

describe("Select", () => {
  const opts = [
    { value: "a", label: "Alpha" },
    { value: "b-1", label: "Beta", note: "the second one" },
  ];

  it("shows the placeholder, opens, selects, and closes", () => {
    const onChange = vi.fn();
    render(<Select value="" options={opts} onChange={onChange} placeholder="Pick one" />);
    const trigger = screen.getByRole("button");
    expect(trigger.textContent).toContain("Pick one");
    fireEvent.click(trigger);
    expect(screen.getByRole("listbox")).toBeTruthy();
    expect(screen.getAllByRole("option")).toHaveLength(2);
    fireEvent.click(screen.getByRole("option", { name: /Beta/ }));
    expect(onChange).toHaveBeenCalledWith("b-1");
    expect(screen.queryByRole("listbox")).toBeNull();
  });

  it("closes on outside mousedown", () => {
    render(<Select value="a" options={opts} onChange={() => {}} />);
    fireEvent.click(screen.getByRole("button"));
    expect(screen.getByRole("listbox")).toBeTruthy();
    fireEvent.mouseDown(document.body);
    expect(screen.queryByRole("listbox")).toBeNull();
  });

  it("closes when focus leaves the control (Tab-out)", () => {
    render(
      <>
        <Select value="a" options={opts} onChange={() => {}} />
        <button type="button">outside</button>
      </>,
    );
    fireEvent.click(screen.getByRole("button", { name: /Alpha/ }));
    expect(screen.getByRole("listbox")).toBeTruthy();
    // simulate Tab walking focus out to the sibling button
    fireEvent.focusOut(screen.getByRole("listbox"), {
      relatedTarget: screen.getByRole("button", { name: "outside" }),
    });
    expect(screen.queryByRole("listbox")).toBeNull();
  });

  it("closes on Escape and refocuses the trigger", () => {
    render(<Select value="a" options={opts} onChange={() => {}} />);
    const trigger = screen.getByRole("button", { name: /Alpha/ });
    fireEvent.click(trigger);
    expect(screen.getByRole("listbox")).toBeTruthy();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("listbox")).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it("marks the current option selected", () => {
    render(<Select value="a" options={opts} onChange={() => {}} />);
    fireEvent.click(screen.getByRole("button"));
    const alpha = screen.getByRole("option", { name: "Alpha" });
    expect(alpha.getAttribute("aria-selected")).toBe("true");
    expect(within(alpha).getByText("Alpha")).toBeTruthy();
  });

  it("renders a per-option note", () => {
    render(<Select value="" options={opts} onChange={() => {}} />);
    fireEvent.click(screen.getByRole("button"));
    expect(screen.getByText("the second one")).toBeTruthy();
  });

  it("uses the mono font for id-like values and inherit otherwise", () => {
    const { rerender } = render(<Select value="b-1" options={opts} onChange={() => {}} />);
    expect((screen.getByRole("button") as HTMLElement).style.fontFamily).toBe("var(--font-mono)");
    rerender(<Select value="a" options={opts} onChange={() => {}} />);
    expect((screen.getByRole("button") as HTMLElement).style.fontFamily).toBe("inherit");
  });

  it("honors a per-option mono opt-out in the dropdown list (matching the trigger)", () => {
    render(
      <Select value="" options={[{ value: "with-dash", label: "Plain", mono: false }]} onChange={() => {}} />,
    );
    fireEvent.click(screen.getByRole("button"));
    // an id-like value (`with-dash`) would auto-mono, but mono:false must opt the option out
    expect((screen.getByText("Plain") as HTMLElement).style.fontFamily).toBe("inherit");
  });

  it("shows the invalid border", () => {
    render(<Select value="" options={opts} onChange={() => {}} invalid />);
    expect((screen.getByRole("button") as HTMLElement).style.border).toContain("var(--red)");
  });

  it("raises the focus ring and rotates the chevron when open", () => {
    const { container } = render(<Select value="a" options={opts} onChange={() => {}} />);
    const trigger = screen.getByRole("button") as HTMLElement;
    fireEvent.click(trigger);
    expect(trigger.style.boxShadow).toContain("var(--focus-ring)");
    expect((container.querySelector("svg") as SVGElement).style.transform).toBe("rotate(180deg)");
  });
});

describe("interactive primitives — additional state coverage", () => {
  it("Toggle uses the smaller dimensions for size=sm", () => {
    render(<Toggle size="sm" checked={false} onChange={() => {}} aria-label="s" />);
    expect((screen.getByRole("switch") as HTMLElement).style.width).toBe("36px");
  });

  it("Chips commits the typed value on blur, trimmed", () => {
    const onAdd = vi.fn();
    render(<Chips items={[]} onAdd={onAdd} onRemove={() => {}} />);
    const input = screen.getByRole("textbox");
    fireEvent.change(input, { target: { value: "  spaced  " } });
    fireEvent.blur(input);
    expect(onAdd).toHaveBeenCalledWith("spaced");
  });
});
