// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent } from "@testing-library/react";
import { PromptSource, isLocalPromptPath, promptFileHint } from "@/components/settings/PromptSource";
import { REPO_PROMPT_PATH } from "@/lib/settings-model";

afterEach(() => {
  cleanup();
  // PromptSource Browse availability is keyed off window.go; clear any test-installed bridge.
  delete (window as unknown as { go?: unknown }).go;
});

describe("promptFileHint / isLocalPromptPath", () => {
  it("treats a relative path as repo-relative", () => {
    expect(promptFileHint("prompts/PROMPT.md")).toBe("Repo-relative · read from each run's checkout");
    expect(isLocalPromptPath("prompts/PROMPT.md")).toBe(false);
  });
  it("treats an absolute or ~ path as a local file on this machine", () => {
    expect(promptFileHint("/Users/me/p.md")).toBe("Local file on this machine");
    expect(promptFileHint("~/p.md")).toBe("Local file on this machine");
    expect(promptFileHint("~")).toBe("Local file on this machine");
    expect(isLocalPromptPath("/abs")).toBe(true);
    expect(isLocalPromptPath("~/x")).toBe(true);
  });
  it("an empty path reads as the repo-relative default", () => {
    expect(promptFileHint("")).toBe("Repo-relative · read from each run's checkout");
    expect(promptFileHint("   ")).toBe("Repo-relative · read from each run's checkout");
  });
});

const repoCheckbox = () => screen.getByRole("checkbox", { name: "Use this repo's prompt" }) as HTMLInputElement;
const inlineArea = (body: string) => screen.getByDisplayValue(body) as HTMLTextAreaElement;

describe("PromptSource", () => {
  it("defaults to inline (unchecked) and ticking the checkbox stores the canonical repo path", () => {
    const onFile = vi.fn();
    render(<PromptSource prompt="hi" onPromptChange={vi.fn()} promptFile="" onPromptFileChange={onFile} promptPlaceholder="PH" />);
    // No file → checkbox off, inline editable.
    expect(repoCheckbox().checked).toBe(false);
    expect(inlineArea("hi").disabled).toBe(false);
    // Ticking adopts the canonical repo prompt path.
    fireEvent.click(repoCheckbox());
    expect(onFile).toHaveBeenCalledWith(REPO_PROMPT_PATH);
  });

  it("is checked and greys the inline editor when promptFile is the canonical path", () => {
    render(<PromptSource prompt="body" onPromptChange={vi.fn()} promptFile={REPO_PROMPT_PATH} onPromptFileChange={vi.fn()} />);
    expect(repoCheckbox().checked).toBe(true);
    // Inline body is still shown (the fallback) but disabled.
    expect(inlineArea("body").disabled).toBe(true);
  });

  it("unticking clears the path so the inline body is honored again", () => {
    const onFile = vi.fn();
    render(<PromptSource prompt="body" onPromptChange={vi.fn()} promptFile={REPO_PROMPT_PATH} onPromptFileChange={onFile} />);
    expect(repoCheckbox().checked).toBe(true);
    fireEvent.click(repoCheckbox());
    expect(onFile).toHaveBeenCalledWith("");
  });

  it("a non-canonical custom path shows the Advanced disclosure and leaves the checkbox unticked", () => {
    render(<PromptSource prompt="body" onPromptChange={vi.fn()} promptFile="prompts/custom.md" onPromptFileChange={vi.fn()} />);
    // A custom path is not the repo convention → checkbox off, inline greyed (the file wins).
    expect(repoCheckbox().checked).toBe(false);
    expect(inlineArea("body").disabled).toBe(true);
    // The Advanced disclosure is open with the custom path + the repo-relative hint.
    expect(screen.getByDisplayValue("prompts/custom.md")).toBeTruthy();
    expect(screen.getByText("Repo-relative · read from each run's checkout")).toBeTruthy();
  });

  it("editing the custom path in the Advanced field calls onPromptFileChange", () => {
    const onFile = vi.fn();
    render(<PromptSource prompt="body" onPromptChange={vi.fn()} promptFile="prompts/custom.md" onPromptFileChange={onFile} />);
    fireEvent.change(screen.getByDisplayValue("prompts/custom.md"), { target: { value: "prompts/new.md" } });
    expect(onFile).toHaveBeenCalledWith("prompts/new.md");
  });

  it("reveals the custom-path input on demand via the Advanced toggle", () => {
    render(<PromptSource prompt="body" onPromptChange={vi.fn()} promptFile="" onPromptFileChange={vi.fn()} />);
    // Advanced is collapsed by default when there is no custom path.
    expect(screen.queryByPlaceholderText(/prompts\/PROMPT\.md/)).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /Advanced: custom path/ }));
    expect(screen.getByPlaceholderText(/prompts\/PROMPT\.md/)).toBeTruthy();
  });

  it("checks the box and disables inline when the canonical repo prompt is inherited (override empty)", () => {
    render(
      <PromptSource
        prompt="agent inline"
        onPromptChange={vi.fn()}
        promptFile=""
        onPromptFileChange={vi.fn()}
        inheritedFile={REPO_PROMPT_PATH}
      />,
    );
    // The inherited canonical file wins at run time → checkbox reflects it, inline disabled.
    expect(repoCheckbox().checked).toBe(true);
    expect(inlineArea("agent inline").disabled).toBe(true);
  });

  it("leaves the box unticked but disables inline when a custom global file is inherited", () => {
    render(
      <PromptSource
        prompt="agent inline"
        onPromptChange={vi.fn()}
        promptFile=""
        onPromptFileChange={vi.fn()}
        inheritedFile="prompts/global.md"
      />,
    );
    // A custom inherited file is not the repo convention → checkbox off, but inline still greyed and
    // the inheritance surfaced.
    expect(repoCheckbox().checked).toBe(false);
    expect(inlineArea("agent inline").disabled).toBe(true);
    expect(screen.getByText(/Inheriting/)).toBeTruthy();
    expect(screen.getByText("prompts/global.md")).toBeTruthy();
  });

  it("hides the Browse button when the native file picker binding is absent", () => {
    render(<PromptSource prompt="" onPromptChange={vi.fn()} promptFile="prompts/x.md" onPromptFileChange={vi.fn()} />);
    expect(screen.queryByRole("button", { name: "Browse for prompt file" })).toBeNull();
  });

  it("shows Browse and wires it to the file picker when the binding is present", () => {
    const PickFile = vi.fn().mockResolvedValue("/picked/prompt.md");
    (window as unknown as { go: unknown }).go = { main: { App: { PickFile } } };
    render(<PromptSource prompt="" onPromptChange={vi.fn()} promptFile="prompts/x.md" onPromptFileChange={vi.fn()} />);
    const browse = screen.getByRole("button", { name: "Browse for prompt file" });
    fireEvent.click(browse);
    expect(PickFile).toHaveBeenCalled();
  });
});
