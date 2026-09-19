import { useEffect, useId, useRef, useState } from "react";
import { Button } from "./Button";
import { Chip } from "./Chip";
import { Seg } from "./Seg";
import { SlidersIcon } from "./icons";
import {
  BOARD_CARD_FIELDS,
  boardCardFieldsAreDefault,
  type BoardCardField,
  type BoardCardFields,
} from "@/hooks/useBoardCardFields";
import { BOARD_LANE_WIDTHS, type BoardLaneWidth } from "@/hooks/useBoardLaneWidth";
import type { JobsViewMode } from "@/hooks/useJobsViewMode";

export interface DisplayOptionsProps {
  /** Which view the Jobs home is showing — the popover's first row. */
  view: JobsViewMode;
  onView: (view: JobsViewMode) => void;
  /** The board's lane width; its row is shown only in Board mode. */
  laneWidth: BoardLaneWidth;
  onLaneWidth: (width: BoardLaneWidth) => void;
  fields: BoardCardFields;
  onToggleField: (field: BoardCardField, on: boolean) => void;
  /** Returns every option to its default. */
  onReset: () => void;
}

const VIEW_OPTIONS: readonly { value: string; label: string }[] = [
  { value: "list", label: "List" },
  { value: "board", label: "Board" },
];

// DisplayOptions — the Jobs home's display preferences, behind one header icon (STUDIO-932).
//
// List/Board and lane width used to sit in the filter row, reading exactly like the status filter
// beside them even though one changes WHICH tickets exist and the others change HOW they look. They
// move here, with the five card-field chips, so the filter row keeps a single kind of control.
//
// The trigger carries a rust dot whenever any option differs from its default: a customised view has
// to explain why the board looks unlike the one everybody else sees. The status filter is not here
// and never returns: in Board mode the lanes ARE that axis (see `JobsView`).
export function DisplayOptions({
  view,
  onView,
  laneWidth,
  onLaneWidth,
  fields,
  onToggleField,
  onReset,
}: DisplayOptionsProps) {
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const popoverRef = useRef<HTMLDivElement>(null);
  const popoverId = useId();

  const customized =
    view !== "list" || laneWidth !== "default" || !boardCardFieldsAreDefault(fields);

  // Close on an outside pointer press, and on Escape with focus handed back to the trigger — the
  // two dismissals the ticket names. Registered only while open so the document is quiet otherwise.
  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) setOpen(false);
    };
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setOpen(false);
        rootRef.current?.querySelector<HTMLButtonElement>(".dptrig")?.focus();
      }
    };
    document.addEventListener("mousedown", onPointerDown);
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("mousedown", onPointerDown);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [open]);

  // Keyboard entry: opening moves focus to the popover's first control, so every option is reachable
  // by tab without a pointer. Escape hands focus back to the trigger.
  useEffect(() => {
    if (open) popoverRef.current?.querySelector<HTMLElement>("button")?.focus();
  }, [open]);

  return (
    <div className="dpwrap" ref={rootRef}>
      <Button
        variant="sec"
        className="dptrig"
        aria-label="Display options"
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-controls={open ? popoverId : undefined}
        onClick={() => setOpen((wasOpen) => !wasOpen)}
      >
        <span className="dpicon">
          <SlidersIcon width={16} height={16} />
          {customized ? <span className="dpdot" aria-hidden="true" /> : null}
        </span>
      </Button>
      {open ? (
        <div className="dpop" id={popoverId} role="dialog" aria-label="Display options" ref={popoverRef}>
          <div className="dprow">
            <span className="dplabel">View</span>
            <Seg aria-label="View" options={VIEW_OPTIONS} value={view} onChange={(v) => onView(v as JobsViewMode)} />
          </div>
          {/* Omitted entirely in List mode rather than disabled: a lane width has no meaning in the
              table, and a dead control only invites a click that does nothing. */}
          {view === "board" ? (
            <div className="dprow">
              <span className="dplabel">Lane width</span>
              <Seg
                aria-label="Lane width"
                options={BOARD_LANE_WIDTHS}
                value={laneWidth}
                onChange={(v) => onLaneWidth(v as BoardLaneWidth)}
              />
            </div>
          ) : null}
          <div className="dprule" />
          <div className="dprow dphfields">
            <span className="dplabel">Card fields</span>
            <div className="dpchips">
              {BOARD_CARD_FIELDS.map((field) => (
                <Chip
                  key={field.id}
                  pressed={fields[field.id]}
                  onClick={() => onToggleField(field.id, !fields[field.id])}
                >
                  {field.label}
                </Chip>
              ))}
            </div>
          </div>
          <div className="dprow dpreset">
            <Button variant="link" onClick={onReset}>
              Reset
            </Button>
          </div>
        </div>
      ) : null}
    </div>
  );
}
