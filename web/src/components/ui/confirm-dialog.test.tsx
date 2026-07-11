// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent } from "@testing-library/react";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";

afterEach(cleanup);

describe("ConfirmDialog", () => {
  it("renders nothing when closed", () => {
    render(<ConfirmDialog open={false} title="t" confirmLabel="Stop" onConfirm={() => {}} onClose={() => {}} />);
    expect(screen.queryByText("t")).toBeNull();
  });
  it("fires onConfirm and onClose on the buttons", () => {
    const onConfirm = vi.fn(),
      onClose = vi.fn();
    render(
      <ConfirmDialog open title="Stop INF-9?" body="kills it" confirmLabel="Stop" danger onConfirm={onConfirm} onClose={onClose} />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Stop" }));
    expect(onConfirm).toHaveBeenCalledOnce();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onClose).toHaveBeenCalledOnce();
  });
});
