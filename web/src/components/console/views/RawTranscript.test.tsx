// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fetchRawTranscript, type RawTranscriptPage } from "@/lib/api";
import { RawTranscript } from "./RawTranscript";

vi.mock("@/lib/api", async (original) => ({
  ...await original<typeof import("@/lib/api")>(),
  fetchRawTranscript: vi.fn(),
}));

afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.resetAllMocks(); });

function page(texts: string[], overrides: Partial<RawTranscriptPage> = {}): RawTranscriptPage {
  let offset = 0;
  return {
    run_id: 42, lines: texts.map((text) => {
      const line = { offset, text };
      offset += new TextEncoder().encode(text).length;
      return line;
    }), size_bytes: texts.join("").length, prev_cursor: null, next_cursor: null,
    at_start: true, at_end: true, ...overrides,
  };
}

function mount(inFlight = false) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const view = render(<QueryClientProvider client={client}><RawTranscript runId={42} inFlight={inFlight} /></QueryClientProvider>);
  return { ...view, client };
}

describe("lossless Raw transcript", () => {
  it("shows system/result records and expands a tool result in full, with download endpoints", async () => {
    const output = "x".repeat(300);
    vi.mocked(fetchRawTranscript).mockResolvedValue(page([
      '{"type":"system","timestamp":"2026-10-09T12:00:00Z","tools":["Bash"]}\n',
      JSON.stringify({ type: "user", message: { content: [{ type: "tool_result", content: output }] } }) + "\n",
      '{"type":"result","usage":{"output_tokens":9},"duration_ms":100}\n',
    ]));
    const { container } = mount();
    await screen.findByText(/"type":"system"/);
    expect(screen.getByText(/"type":"result"/)).toBeTruthy();
    expect(screen.queryByText(new RegExp(output))).toBeNull();
    const row = container.querySelectorAll(".rawline")[1];
    expect(row.textContent).toContain("Expand full line");
    fireEvent.click(row.querySelector("button")!);
    expect(row.querySelector("pre")?.textContent).toContain(output);
    expect(row.querySelector("pre")?.textContent).toContain('"type": "user"');
    expect(container.querySelector("time")?.getAttribute("datetime")).toBe("2026-10-09T12:00:00.000Z");
    expect(screen.getByRole("link", { name: "Download .jsonl" }).getAttribute("href")).toBe("/api/v1/runs/42/transcript.jsonl");
    expect(screen.getByRole("link", { name: "Download stderr" }).getAttribute("href")).toBe("/api/v1/runs/42/stderr.log");
    expect(fetchRawTranscript).toHaveBeenCalledWith(42);
  });

  it.each([false, true])("keeps appended bytes reachable after loading earlier (initially live: %s)", async (inFlight) => {
    vi.mocked(fetchRawTranscript)
      .mockResolvedValueOnce(page(["later\n"], { lines: [{ offset: 8, text: "later\n" }], size_bytes: 14, prev_cursor: 8, at_start: false }))
      // The file grew during the earlier read. Later must start at the loaded end (14), not
      // this newer size (18), or the new line is silently skipped.
      .mockResolvedValueOnce(page(["earlier\n"], { next_cursor: 8, at_end: false, size_bytes: 18 }))
      .mockResolvedValueOnce(page(["new\n"], { lines: [{ offset: 14, text: "new\n" }], size_bytes: 18, prev_cursor: 14 }));
    const { client, rerender } = mount(inFlight);
    await screen.findByText("later");
    fireEvent.click(screen.getByRole("button", { name: "Load earlier" }));
    await screen.findByText("earlier");
    expect(fetchRawTranscript).toHaveBeenLastCalledWith(42, 8, "backward");
    expect(screen.getByText("later")).toBeTruthy();
    rerender(<QueryClientProvider client={client}><RawTranscript runId={42} inFlight={false} /></QueryClientProvider>);
    expect((screen.getByRole("button", { name: "Load later" }) as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(screen.getByRole("button", { name: "Load later" }));
    await screen.findByText("new");
    expect(fetchRawTranscript).toHaveBeenLastCalledWith(42, 14, "forward");
    expect((screen.getByRole("button", { name: "Load later" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("pretty-prints original JSON tokens without rounding numbers or dropping duplicate keys", async () => {
    const text = '{"id":9007199254740993,"k":1,"k":2,"number":1.2300e+45,"escaped":"\\u0061\\\" , {}","nested":[{},[],true,null,-0]}\n';
    vi.mocked(fetchRawTranscript).mockResolvedValue(page([text]));
    const { container } = mount();
    await screen.findByText(/9007199254740993/);
    fireEvent.click(container.querySelector(".rawline button")!);
    expect(container.querySelector("pre")?.textContent).toBe([
      '{',
      '  "id": 9007199254740993,',
      '  "k": 1,',
      '  "k": 2,',
      '  "number": 1.2300e+45,',
      '  "escaped": "\\u0061\\\" , {}",',
      '  "nested": [',
      '    {},',
      '    [],',
      '    true,',
      '    null,',
      '    -0',
      '  ]',
      '}',
    ].join("\n"));
  });

  it("allows a final later read while paused even when no page observed the final append", async () => {
    vi.mocked(fetchRawTranscript)
      .mockResolvedValueOnce(page(["partial"], { lines: [{ offset: 8, text: "partial" }], size_bytes: 15, prev_cursor: 8, at_start: false }))
      .mockResolvedValueOnce(page(["earlier\n"], { next_cursor: 8, at_end: false, size_bytes: 15 }))
      .mockResolvedValueOnce(page(["partial complete\n"], { lines: [{ offset: 8, text: "partial complete\n" }], size_bytes: 25, prev_cursor: 8 }));
    const { client, rerender } = mount(true);
    await screen.findByText("partial");
    fireEvent.click(screen.getByRole("button", { name: "Load earlier" }));
    await screen.findByText("earlier");
    rerender(<QueryClientProvider client={client}><RawTranscript runId={42} inFlight={false} /></QueryClientProvider>);
    fireEvent.click(screen.getByRole("button", { name: "Load later" }));
    await screen.findByText("partial complete");
    expect(fetchRawTranscript).toHaveBeenLastCalledWith(42, 8, "forward");
    expect(document.querySelectorAll(".rawline")).toHaveLength(2);
    expect((screen.getByRole("button", { name: "Load later" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("keeps the final later read available when an older earlier-page response arrives after termination", async () => {
    let finishEarlier!: (value: RawTranscriptPage) => void;
    vi.mocked(fetchRawTranscript)
      .mockResolvedValueOnce(page(["partial"], { lines: [{ offset: 8, text: "partial" }], size_bytes: 15, prev_cursor: 8, at_start: false }))
      .mockImplementationOnce(() => new Promise((resolve) => { finishEarlier = resolve; }))
      .mockResolvedValueOnce(page(["partial complete\n"], { lines: [{ offset: 8, text: "partial complete\n" }], size_bytes: 25, prev_cursor: 8 }));
    const { client, rerender } = mount(true);
    await screen.findByText("partial");
    fireEvent.click(screen.getByRole("button", { name: "Load earlier" }));
    await waitFor(() => expect(fetchRawTranscript).toHaveBeenLastCalledWith(42, 8, "backward"));
    rerender(<QueryClientProvider client={client}><RawTranscript runId={42} inFlight={false} /></QueryClientProvider>);
    finishEarlier(page(["earlier\n"], { next_cursor: 8, at_end: false, size_bytes: 15 }));
    await screen.findByText("earlier");
    expect((screen.getByRole("button", { name: "Load later" }) as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(screen.getByRole("button", { name: "Load later" }));
    await screen.findByText("partial complete");
    expect(fetchRawTranscript).toHaveBeenLastCalledWith(42, 8, "forward");
    expect((screen.getByRole("button", { name: "Load later" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("keeps invalid JSON verbatim and reads OpenCode epoch timestamps", async () => {
    const text = "partial **markdown**\r\n";
    vi.mocked(fetchRawTranscript).mockResolvedValue(page([text, '{"type":"step_finish","timestamp":1791547200000,"part":{"tokens":{"total":12}}}\n']));
    const { container } = mount();
    await screen.findByText(/partial/);
    fireEvent.click(container.querySelector(".rawline button")!);
    expect(container.querySelector("pre")?.textContent).toBe(text);
    expect(container.querySelector("strong")).toBeNull();
    expect(container.querySelector("time")?.getAttribute("datetime")).toBe(new Date(1791547200000).toISOString());
  });

  it("refreshes the live tail including a growing unterminated line, then final output", async () => {
    vi.mocked(fetchRawTranscript).mockResolvedValue(page(['{"type":"res']));
    const { client, rerender } = mount(true);
    await screen.findByText(/"type":"res/);
    vi.mocked(fetchRawTranscript).mockResolvedValue(page(['{"type":"result"}\n']));
    await client.refetchQueries({ queryKey: ["raw-transcript-tail", 42] });
    await screen.findByText(/"type":"result"/);
    expect(document.querySelectorAll(".rawline")).toHaveLength(1);
    vi.mocked(fetchRawTranscript).mockResolvedValue(page(['{"type":"result","final":true}\n']));
    rerender(<QueryClientProvider client={client}><RawTranscript runId={42} inFlight={false} /></QueryClientProvider>);
    await screen.findByText(/"final":true/);
  });

  it("states missing files and read failures explicitly", async () => {
    vi.mocked(fetchRawTranscript).mockResolvedValue(page([], { missing: true }));
    const { client } = mount();
    await screen.findByText(/missing or pruned/);
    vi.mocked(fetchRawTranscript).mockRejectedValue(new Error("refused"));
    await client.refetchQueries({ queryKey: ["raw-transcript-tail", 42] });
    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("refused"));
    expect(screen.queryByText(/No transcript recorded/)).toBeNull();
  });

  it("does not decode collapsed payloads or mistake a nested timestamp for the line's clock", async () => {
    const text = JSON.stringify({ type: "tool_use", part: { timestamp: 1, output: "é".repeat(300) }, timestamp: 1791547200000 }) + "\n";
    vi.mocked(fetchRawTranscript).mockResolvedValue(page([text]));
    const parse = vi.spyOn(JSON, "parse");
    const { container } = mount();
    await screen.findByText(/"type":"tool_use"/);
    expect(parse.mock.calls.some(([input]) => input === text)).toBe(false);
    expect(container.querySelector("time")?.getAttribute("datetime")).toBe(new Date(1791547200000).toISOString());
    expect(container.querySelector(".trraw-size")?.textContent).toContain(`${new TextEncoder().encode(text).length.toLocaleString()} B`);
    fireEvent.click(container.querySelector(".rawline button")!);
    expect(parse.mock.calls.some(([input]) => input === text)).toBe(true);
    parse.mockRestore();
  });

  it("never reports an empty run when the initial read failed", async () => {
    vi.mocked(fetchRawTranscript).mockRejectedValue(new Error("refused"));
    mount();
    expect((await screen.findByRole("alert")).textContent).toContain("refused");
    expect(screen.queryByText(/No transcript recorded/)).toBeNull();
  });
});
