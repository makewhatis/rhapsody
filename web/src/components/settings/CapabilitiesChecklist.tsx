import { useEffect, useState } from "react";
import { fetchCapabilitiesRegistry, type CapabilityDefDTO } from "@/lib/api";

// CapabilitiesChecklist renders the daemon's capability registry (GET /api/v1/capabilities) as an
// opt-in checkbox list for the per-project config screen (BO-14). It is a controlled control: the
// parent Field owns the value (`selected`) and persists via `onChange`; `inheritedDefault` is the
// global list shown as a hint when the agent has selected nothing (mirrors the "Required labels"
// inherit hint in AgentDetail). Styling mirrors AgentDetail's inline conventions (the `var(--tx-3)`
// hint text, the 11.5/12.5 font sizes).
interface CapabilitiesChecklistProps {
  selected: string[];
  onChange: (next: string[]) => void;
  inheritedDefault: string[];
}

export function CapabilitiesChecklist({
  selected,
  onChange,
  inheritedDefault,
}: CapabilitiesChecklistProps) {
  const [registry, setRegistry] = useState<CapabilityDefDTO[]>([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    fetchCapabilitiesRegistry()
      .then(setRegistry)
      .catch((e) => setError(String(e)));
  }, []);

  if (error) {
    return (
      <span style={{ fontSize: 11.5, color: "var(--tx-3)" }}>Could not load capabilities: {error}</span>
    );
  }

  const toggle = (name: string) => {
    if (selected.includes(name)) {
      onChange(selected.filter((n) => n !== name));
    } else {
      onChange([...selected, name]);
    }
  };

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
      {registry.map((cap) => (
        <label key={cap.name} style={{ display: "flex", alignItems: "flex-start", gap: 8, fontSize: 12.5 }}>
          <input
            type="checkbox"
            checked={selected.includes(cap.name)}
            onChange={() => toggle(cap.name)}
          />
          <span style={{ display: "flex", flexDirection: "column", gap: 2 }}>
            <strong>{cap.label}</strong>
            <span style={{ fontSize: 11.5, color: "var(--tx-3)" }}>{cap.description}</span>
          </span>
        </label>
      ))}
      {selected.length === 0 && inheritedDefault.length > 0 ? (
        <span style={{ fontSize: 11.5, color: "var(--tx-3)" }}>
          Inheriting global default: {inheritedDefault.join(", ")}
        </span>
      ) : null}
    </div>
  );
}
