// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent } from "@testing-library/react";
import { Field, FieldError } from "@/components/ui/field";
import { TextInput } from "@/components/ui/text-input";
import { TextArea } from "@/components/ui/text-area";
import { Stepper } from "@/components/ui/stepper";
import { Search } from "@/components/ui/icons";

afterEach(cleanup);

describe("Field / FieldError", () => {
  it("renders label, optional badge, hint and a header action", () => {
    render(
      <Field label="API key" optional hint="Stored in the keychain" action={<button>Reveal</button>}>
        <input />
      </Field>,
    );
    expect(screen.getByText("API key")).toBeTruthy();
    expect(screen.getByText("optional")).toBeTruthy();
    expect(screen.getByText("Stored in the keychain")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Reveal" })).toBeTruthy();
  });

  it("shows an error message", () => {
    render(
      <Field label="Port" error="must be a number">
        <input />
      </Field>,
    );
    expect(screen.getByText("must be a number")).toBeTruthy();
  });

  it("uses a two-column grid when inline", () => {
    const { container } = render(
      <Field label="Concurrency" inline>
        <input />
      </Field>,
    );
    expect((container.firstChild as HTMLElement).style.display).toBe("grid");
  });

  it("FieldError renders a warning glyph", () => {
    const { container } = render(<FieldError>bad</FieldError>);
    expect(container.querySelector("svg")).toBeTruthy();
    expect(screen.getByText("bad")).toBeTruthy();
  });
});

describe("TextInput", () => {
  it("raises the rust focus ring on focus and clears it on blur", () => {
    const { container } = render(<TextInput placeholder="x" />);
    const input = container.querySelector("input") as HTMLInputElement;
    expect(input.style.boxShadow).toBe("none");
    fireEvent.focus(input);
    expect(input.style.boxShadow).toContain("var(--focus-ring)");
    fireEvent.blur(input);
    expect(input.style.boxShadow).toBe("none");
  });

  it("shows the invalid border", () => {
    const { container } = render(<TextInput invalid />);
    expect((container.querySelector("input") as HTMLInputElement).style.border).toContain("var(--red)");
  });

  it("renders a prefix icon and a suffix", () => {
    const { container } = render(<TextInput prefixIcon={Search} suffix="ms" />);
    expect(container.querySelector("svg")).toBeTruthy();
    expect(screen.getByText("ms")).toBeTruthy();
  });

  it("uses the mono font when requested", () => {
    const { container } = render(<TextInput mono />);
    expect((container.querySelector("input") as HTMLInputElement).style.fontFamily).toContain("--font-mono");
  });

  it("forwards value/onChange", () => {
    const onChange = vi.fn();
    const { container } = render(<TextInput value="abc" onChange={onChange} />);
    const input = container.querySelector("input") as HTMLInputElement;
    expect(input.value).toBe("abc");
    fireEvent.change(input, { target: { value: "abcd" } });
    expect(onChange).toHaveBeenCalled();
  });
});

describe("TextArea", () => {
  it("raises the focus ring and supports mono", () => {
    const { container } = render(<TextArea mono />);
    const ta = container.querySelector("textarea") as HTMLTextAreaElement;
    expect(ta.style.fontFamily).toContain("--font-mono");
    fireEvent.focus(ta);
    expect(ta.style.boxShadow).toContain("var(--focus-ring)");
  });
});

describe("Stepper", () => {
  it("increments and decrements", () => {
    const onChange = vi.fn();
    render(<Stepper value={2} min={0} max={10} onChange={onChange} />);
    fireEvent.click(screen.getByLabelText("Increment"));
    expect(onChange).toHaveBeenLastCalledWith(3);
    fireEvent.click(screen.getByLabelText("Decrement"));
    expect(onChange).toHaveBeenLastCalledWith(1);
  });

  it("clamps to min and max", () => {
    const onChange = vi.fn();
    const { rerender } = render(<Stepper value={10} min={0} max={10} onChange={onChange} />);
    fireEvent.click(screen.getByLabelText("Increment"));
    expect(onChange).toHaveBeenLastCalledWith(10);
    rerender(<Stepper value={0} min={0} max={10} onChange={onChange} />);
    fireEvent.click(screen.getByLabelText("Decrement"));
    expect(onChange).toHaveBeenLastCalledWith(0);
  });

  it("parses digits typed into the field", () => {
    const onChange = vi.fn();
    render(<Stepper value={1} min={0} max={99} onChange={onChange} />);
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "4x2" } });
    expect(onChange).toHaveBeenLastCalledWith(42);
  });

  it("renders a suffix", () => {
    render(<Stepper value={2} suffix="ms" onChange={() => {}} />);
    expect(screen.getByText("ms")).toBeTruthy();
  });
});

describe("form primitives — additional state coverage", () => {
  it("Field renders the error in the inline layout too", () => {
    render(
      <Field label="x" inline error="bad value">
        <input />
      </Field>,
    );
    expect(screen.getByText("bad value")).toBeTruthy();
  });

  it("TextArea clears the focus ring on blur", () => {
    const { container } = render(<TextArea />);
    const ta = container.querySelector("textarea") as HTMLTextAreaElement;
    fireEvent.focus(ta);
    expect(ta.style.boxShadow).toContain("var(--focus-ring)");
    fireEvent.blur(ta);
    expect(ta.style.boxShadow).toBe("none");
  });

  it("TextInput keeps the invalid border even while focused", () => {
    const { container } = render(<TextInput invalid />);
    const input = container.querySelector("input") as HTMLInputElement;
    fireEvent.focus(input);
    expect(input.style.border).toContain("var(--red)");
  });
});
