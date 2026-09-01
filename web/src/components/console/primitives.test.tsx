// @vitest-environment jsdom
// STUDIO-681 §10 box 1.3 — "Each component in §1.3 exists as a reusable component with the
// states listed, rendered from props." One block per component; AppShell/NavItem, Pill,
// Stepper and TagInput have their own files because their acceptance runs deeper.
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { Button } from "./Button";
import { Card } from "./Card";
import { Chip } from "./Chip";
import { Note } from "./Note";
import { Seg } from "./Seg";
import { Select } from "./Select";
import { TicketChip } from "./TicketChip";
import { Toggle } from "./Toggle";
import { Grid, GridSide, Mate, Mono, NowMates, NowStats, NowStrip, Stat, TeammateAvatar, Timestamp } from "./layout";
import { teammateColor } from "@/theme/teammates";

afterEach(cleanup);

describe("Card", () => {
  it("renders the header's title, sub and right slot", () => {
    const { container } = render(
      <Card title="Runs" sub="newest first · 3 dispatches" right={<a className="link">Open →</a>}>
        <p>body</p>
      </Card>,
    );
    expect(container.querySelector(".card > .hd h2")?.textContent).toBe("Runs");
    expect(container.querySelector(".hd .sub")?.textContent).toBe("newest first · 3 dispatches");
    expect(container.querySelector(".hd .rt")?.textContent).toBe("Open →");
    expect(screen.getByText("body")).toBeTruthy();
  });

  it("omits the header entirely when no header slot is given", () => {
    const { container } = render(<Card>bare</Card>);
    expect(container.querySelector(".card")).not.toBeNull();
    expect(container.querySelector(".hd")).toBeNull();
  });
});

describe("Chip", () => {
  it("is unpressed by default and reports its state as aria-pressed", () => {
    const { rerender } = render(<Chip>All</Chip>);
    expect(screen.getByRole("button", { name: "All" }).getAttribute("aria-pressed")).toBe("false");
    rerender(<Chip pressed>All</Chip>);
    expect(screen.getByRole("button", { name: "All" }).getAttribute("aria-pressed")).toBe("true");
  });

  it("renders the optional trailing count", () => {
    const { container } = render(<Chip count={3}>Quorum</Chip>);
    expect(container.querySelector(".chip .k")?.textContent).toBe("3");
  });

  it("omits the count slot when there is no count", () => {
    const { container } = render(<Chip>Quorum</Chip>);
    expect(container.querySelector(".k")).toBeNull();
  });

  it("clicks", () => {
    const onClick = vi.fn();
    render(<Chip onClick={onClick}>All</Chip>);
    fireEvent.click(screen.getByRole("button", { name: "All" }));
    expect(onClick).toHaveBeenCalled();
  });

  it("does not submit a surrounding form", () => {
    render(<Chip>All</Chip>);
    expect(screen.getByRole("button", { name: "All" }).getAttribute("type")).toBe("button");
  });
});

describe("TicketChip", () => {
  it("renders a plain ticket key with no variant class", () => {
    const { container } = render(<TicketChip>STUDIO-682</TicketChip>);
    const chip = container.querySelector(".tk");
    expect(chip?.textContent).toBe("STUDIO-682");
    expect(chip?.className).toBe("tk");
  });

  it("renders the pr and sha variants", () => {
    const { container } = render(
      <>
        <TicketChip variant="pr">#70</TicketChip>
        <TicketChip variant="sha">24e83c5</TicketChip>
      </>,
    );
    expect(container.querySelector(".tk.pr")?.textContent).toBe("#70");
    expect(container.querySelector(".tk.sha")?.textContent).toBe("24e83c5");
  });
});

describe("Select", () => {
  it("renders bare-string options as their own value and label", () => {
    render(<Select aria-label="Teammate" options={["all", "alice", "jimmy"]} defaultValue="all" />);
    const select = screen.getByRole("combobox", { name: "Teammate" }) as HTMLSelectElement;
    expect([...select.options].map((o) => o.value)).toEqual(["all", "alice", "jimmy"]);
  });

  it("renders {value,label} options and reports changes", () => {
    const onChange = vi.fn();
    render(
      <Select
        aria-label="Project"
        options={[
          { value: "", label: "All projects" },
          { value: "rhapsody", label: "Rhapsody" },
        ]}
        value=""
        onChange={onChange}
      />,
    );
    fireEvent.change(screen.getByRole("combobox", { name: "Project" }), { target: { value: "rhapsody" } });
    expect(onChange).toHaveBeenCalled();
  });

  it("wraps the select so the caret has something to sit on", () => {
    const { container } = render(<Select aria-label="Teammate" options={["all"]} />);
    expect(container.querySelector(".selwrap > select.sel")).not.toBeNull();
  });
});

describe("Seg", () => {
  const options = ["All", "In review", "Running"];

  it("presses exactly the selected option", () => {
    render(<Seg options={options} value="In review" onChange={vi.fn()} aria-label="Status" />);
    expect(screen.getByRole("button", { name: "All" }).getAttribute("aria-pressed")).toBe("false");
    expect(screen.getByRole("button", { name: "In review" }).getAttribute("aria-pressed")).toBe("true");
  });

  it("reports the chosen value", () => {
    const onChange = vi.fn();
    render(<Seg options={options} value="All" onChange={onChange} />);
    fireEvent.click(screen.getByRole("button", { name: "Running" }));
    expect(onChange).toHaveBeenCalledWith("Running");
  });

  it("carries the accent variant only when asked", () => {
    const { container, rerender } = render(<Seg options={options} value="All" onChange={vi.fn()} />);
    expect(container.querySelector(".seg")?.classList.contains("acc")).toBe(false);
    rerender(<Seg options={options} value="All" onChange={vi.fn()} accent />);
    expect(container.querySelector(".seg")?.classList.contains("acc")).toBe(true);
  });

  it("supports a disabled option", () => {
    render(
      <Seg
        options={[{ value: "labels" }, { value: "off", disabled: true }]}
        value="labels"
        onChange={vi.fn()}
      />,
    );
    expect((screen.getByRole("button", { name: "off" }) as HTMLButtonElement).disabled).toBe(true);
  });
});

describe("Toggle", () => {
  it("reports its state and flips to the opposite value", () => {
    const onChange = vi.fn();
    render(<Toggle pressed={false} onChange={onChange} label="Review quorum" />);
    const toggle = screen.getByRole("button", { name: "Review quorum" });
    expect(toggle.getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(toggle);
    expect(onChange).toHaveBeenCalledWith(true);
  });

  it("flips back from the pressed state", () => {
    const onChange = vi.fn();
    render(<Toggle pressed onChange={onChange} label="Review quorum" />);
    fireEvent.click(screen.getByRole("button", { name: "Review quorum" }));
    expect(onChange).toHaveBeenCalledWith(false);
  });

  it("carries the small variant only when asked", () => {
    const { container, rerender } = render(<Toggle pressed onChange={vi.fn()} label="Teams" />);
    expect(container.querySelector(".toggle")?.classList.contains("sm")).toBe(false);
    rerender(<Toggle pressed onChange={vi.fn()} label="Teams" small />);
    expect(container.querySelector(".toggle")?.classList.contains("sm")).toBe(true);
  });

  it("does not fire while disabled", () => {
    const onChange = vi.fn();
    render(<Toggle pressed={false} onChange={onChange} label="Teams" disabled />);
    fireEvent.click(screen.getByRole("button", { name: "Teams" }));
    expect(onChange).not.toHaveBeenCalled();
  });
});

describe("Note", () => {
  it("defaults to the info variant", () => {
    const { container } = render(<Note>boot-loaded; applies on restart</Note>);
    const note = container.querySelector(".note");
    expect(note?.classList.contains("info")).toBe(true);
    expect(note?.classList.contains("warn")).toBe(false);
  });

  it("renders the warn variant with its own leading glyph", () => {
    const { container } = render(<Note variant="warn">below 15000 ms the model turn always times out</Note>);
    expect(container.querySelector(".note.warn")).not.toBeNull();
    expect(container.querySelector(".note > svg")).not.toBeNull();
    expect(container.textContent).toContain("15000");
  });

  it("accepts a caller-supplied icon", () => {
    const { container } = render(<Note icon={<i data-testid="glyph" />}>hi</Note>);
    expect(container.querySelector('[data-testid="glyph"]')).not.toBeNull();
    expect(container.querySelector(".note > svg")).toBeNull();
  });
});

describe("Button", () => {
  it("is the accent button by default, with no extra variant class", () => {
    const { container } = render(<Button>Summon</Button>);
    expect(container.querySelector("button")?.className).toBe("btn");
  });

  for (const variant of ["pri", "sec", "link"] as const) {
    it(`renders the ${variant} variant`, () => {
      const { container } = render(<Button variant={variant}>Save</Button>);
      expect(container.querySelector(`.btn.${variant}`)).not.toBeNull();
    });
  }

  it("defaults to type=button so it cannot submit the manage-team form by accident", () => {
    render(<Button>Save</Button>);
    expect(screen.getByRole("button", { name: "Save" }).getAttribute("type")).toBe("button");
  });

  it("still allows an explicit submit", () => {
    render(<Button type="submit">Save</Button>);
    expect(screen.getByRole("button", { name: "Save" }).getAttribute("type")).toBe("submit");
  });
});

describe("layout primitives (§1.4)", () => {
  it("renders the two-column grid and its side stack", () => {
    const { container } = render(
      <Grid>
        <div>main</div>
        <GridSide>side</GridSide>
      </Grid>,
    );
    expect(container.querySelector(".grid > .side")?.textContent).toBe("side");
  });

  it("renders the now strip with teammate states and stat pills", () => {
    const { container } = render(
      <NowStrip>
        <NowMates>
          <Mate name="alice" task="STUDIO-682" running />
          <Mate name="jimmy" task="idle" />
        </NowMates>
        <NowStats>
          <Stat value={0} label="running" />
          <Stat value={5} label="in review" tone="acc" />
          <Stat value={1} label="blocked" tone="bad" />
        </NowStats>
      </NowStrip>,
    );
    expect(container.querySelector(".now > .who")).not.toBeNull();
    expect(container.querySelector(".now > .stats")).not.toBeNull();
    expect(container.querySelectorAll(".mate").length).toBe(2);
    expect(container.querySelector(".mate.run b")?.textContent).toBe("alice");
    expect(container.querySelector(".mate.run .task")?.textContent).toBe("STUDIO-682");
    expect(container.querySelector(".stat.acc .n")?.textContent).toBe("5");
    expect(container.querySelector(".stat.bad .l")?.textContent).toBe("blocked");
  });

  it("renders an idle teammate without the running treatment", () => {
    const { container } = render(<Mate name="jimmy" task="idle" />);
    expect(container.querySelector(".mate")?.classList.contains("run")).toBe(false);
  });

  it("renders a stat with no tone as the plain variant", () => {
    const { container } = render(<Stat value={0} label="running" />);
    expect(container.querySelector(".stat")?.className).toBe("stat");
  });

  it("paints a teammate avatar from the positional ramp, never from a name", () => {
    const roster = ["zed", "alice"];
    const { container } = render(<TeammateAvatar color={teammateColor(roster, "alice")} />);
    // Second on the roster, so the second ramp color — not amber, which "alice" would get
    // under a per-name mapping.
    expect((container.querySelector(".av") as HTMLElement).style.background).toBe("var(--mate-2)");
  });

  it("renders mono spans for ids and timestamps (§1.2)", () => {
    const { container } = render(
      <>
        <Mono>symphony/STUDIO-682</Mono>
        <Timestamp>19:11</Timestamp>
      </>,
    );
    expect(container.querySelector(".mono")?.textContent).toBe("symphony/STUDIO-682");
    expect(container.querySelector(".at")?.textContent).toBe("19:11");
  });
});
