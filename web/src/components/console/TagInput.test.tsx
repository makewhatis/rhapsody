// @vitest-environment jsdom
// STUDIO-681 §1.3 — TagInput: chip-tags with an inline add field, used for a teammate's
// extra routing labels on the manage-team form (§7).
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { TagInput } from "./TagInput";

afterEach(cleanup);

function field(name = "Extra labels") {
  return screen.getByRole("textbox", { name });
}

describe("adding", () => {
  it("adds the typed tag on Enter and clears the field", () => {
    const onChange = vi.fn();
    render(<TagInput tags={["sre"]} onChange={onChange} label="Extra labels" />);

    fireEvent.change(field(), { target: { value: "reviewer" } });
    fireEvent.keyDown(field(), { key: "Enter" });

    expect(onChange).toHaveBeenCalledWith(["sre", "reviewer"]);
    expect(field()).toHaveProperty("value", "");
  });

  it("also commits on comma, so a pasted comma-separated habit works", () => {
    const onChange = vi.fn();
    render(<TagInput tags={[]} onChange={onChange} label="Extra labels" />);
    fireEvent.change(field(), { target: { value: "sre" } });
    fireEvent.keyDown(field(), { key: "," });
    expect(onChange).toHaveBeenCalledWith(["sre"]);
  });

  it("trims surrounding whitespace", () => {
    const onChange = vi.fn();
    render(<TagInput tags={[]} onChange={onChange} label="Extra labels" />);
    fireEvent.change(field(), { target: { value: "  sre  " } });
    fireEvent.keyDown(field(), { key: "Enter" });
    expect(onChange).toHaveBeenCalledWith(["sre"]);
  });

  it("ignores an empty or whitespace-only entry instead of adding a blank label", () => {
    const onChange = vi.fn();
    render(<TagInput tags={[]} onChange={onChange} label="Extra labels" />);
    fireEvent.keyDown(field(), { key: "Enter" });
    fireEvent.change(field(), { target: { value: "   " } });
    fireEvent.keyDown(field(), { key: "Enter" });
    expect(onChange).not.toHaveBeenCalled();
  });

  it("refuses a duplicate — a label routes work, and routing twice means nothing", () => {
    const onChange = vi.fn();
    render(<TagInput tags={["sre"]} onChange={onChange} label="Extra labels" />);
    fireEvent.change(field(), { target: { value: "sre" } });
    fireEvent.keyDown(field(), { key: "Enter" });
    expect(onChange).not.toHaveBeenCalled();
    expect(field()).toHaveProperty("value", "");
  });
});

describe("removing", () => {
  it("renders one chip per tag and removes the one whose × is clicked", () => {
    const onChange = vi.fn();
    render(<TagInput tags={["sre", "reviewer"]} onChange={onChange} label="Extra labels" />);

    expect(screen.getByText("sre")).toBeTruthy();
    expect(screen.getByText("reviewer")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Remove sre" }));
    expect(onChange).toHaveBeenCalledWith(["reviewer"]);
  });

  it("removes the last tag on Backspace in an empty field, and not otherwise", () => {
    const onChange = vi.fn();
    render(<TagInput tags={["sre", "reviewer"]} onChange={onChange} label="Extra labels" />);

    fireEvent.change(field(), { target: { value: "x" } });
    fireEvent.keyDown(field(), { key: "Backspace" });
    expect(onChange).not.toHaveBeenCalled();

    fireEvent.change(field(), { target: { value: "" } });
    fireEvent.keyDown(field(), { key: "Backspace" });
    expect(onChange).toHaveBeenCalledWith(["sre"]);
  });

  it("does nothing on Backspace when there is nothing left to remove", () => {
    const onChange = vi.fn();
    render(<TagInput tags={[]} onChange={onChange} label="Extra labels" />);
    fireEvent.keyDown(field(), { key: "Backspace" });
    expect(onChange).not.toHaveBeenCalled();
  });
});

describe("presentation", () => {
  it("shows the placeholder only while the field is empty of tags and text", () => {
    render(<TagInput tags={[]} onChange={vi.fn()} label="Extra labels" placeholder="add a label…" />);
    expect(field()).toHaveProperty("placeholder", "add a label…");
  });

  it("renders tags as mono ticket chips so they read as identifiers", () => {
    const { container } = render(<TagInput tags={["sre"]} onChange={vi.fn()} label="Extra labels" />);
    expect(container.querySelector(".tags .tk")?.textContent).toContain("sre");
  });
});
