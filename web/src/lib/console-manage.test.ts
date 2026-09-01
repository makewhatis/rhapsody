import { describe, expect, it } from "vitest";
import { MANAGER_MODES, emptyDraft, type TeamsDraft } from "@/lib/teams-model";
import {
  BUILTIN_PROFILES,
  LEAST_LOADED_LABEL,
  MANAGER_MODE_OPTIONS,
  defaultIdentityOptions,
  joinRowLabels,
  managerModelDisabled,
  profileOptions,
  rowLabels,
  showsHindsightFields,
  starvedTimeoutMs,
} from "@/lib/console-manage";

// The pure half of the §7 manage-team form (STUDIO-686). Everything here decides what the
// form REVEALS or DISABLES, which is what §10's boxes 5.2-5.4 are actually about — pinning
// those rules here keeps the view's own tests about the rendered result rather than the rule.

function draft(patch: Partial<TeamsDraft> = {}): TeamsDraft {
  return { ...emptyDraft(), ...patch };
}

describe("MANAGER_MODE_OPTIONS", () => {
  // §7 and the prototype both print the Seg as `labels / labels + model / off`. The schema's own
  // array starts with `off`, so rendering it directly would put the opt-out first.
  it("is ordered the way §7 prints it", () => {
    expect(MANAGER_MODE_OPTIONS.map((o) => o.value)).toEqual(["labels", "labels+model", "off"]);
    expect(MANAGER_MODE_OPTIONS.map((o) => o.label)).toEqual(["labels", "labels + model", "off"]);
  });

  // Reordering is a design choice; DROPPING one would be a mode the operator cannot select and
  // cannot see, so the set is pinned against the schema rather than restated.
  it("covers exactly the modes the schema declares", () => {
    expect(new Set(MANAGER_MODE_OPTIONS.map((o) => o.value))).toEqual(new Set(MANAGER_MODES));
  });
});

describe("profileOptions", () => {
  it("offers the built-in profiles", () => {
    expect(profileOptions("swe")).toEqual([...BUILTIN_PROFILES]);
  });

  // A user profile is any `~/.rhapsody/teams/profiles/<name>.md` (crates/config/src/profiles.rs),
  // so a fixed three-option list would silently rewrite one the moment the form was saved.
  it("keeps a custom profile as an option so saving cannot rewrite it", () => {
    expect(profileOptions("data-eng")).toEqual([...BUILTIN_PROFILES, "data-eng"]);
  });

  it("ignores a blank or whitespace-only current value", () => {
    expect(profileOptions("")).toEqual([...BUILTIN_PROFILES]);
    expect(profileOptions("   ")).toEqual([...BUILTIN_PROFILES]);
  });
});

describe("defaultIdentityOptions", () => {
  it("leads with least-loaded, whose value is the empty string the schema uses", () => {
    const opts = defaultIdentityOptions(draft({ roster: [] }));
    expect(opts).toEqual([{ value: "", label: LEAST_LOADED_LABEL }]);
  });

  // Half-typed rows are not teammates: `toConfig` drops them, so offering one would let the
  // operator pick a default identity the save would then clear.
  it("offers only named roster rows, trimmed", () => {
    const opts = defaultIdentityOptions(
      draft({
        roster: [
          { name: " alice ", profile: "swe", labels: "", bank: "", maxConcurrent: 0 },
          { name: "", profile: "swe", labels: "", bank: "", maxConcurrent: 0 },
          { name: "jimmy", profile: "sre", labels: "", bank: "", maxConcurrent: 0 },
        ],
      }),
    );
    expect(opts.map((o) => o.value)).toEqual(["", "alice", "jimmy"]);
  });
});

describe("managerModelDisabled", () => {
  // Box 5.2: `off` is single-identity Teams — no routing at all, so no model is consulted.
  it("disables the model field only in off", () => {
    expect(managerModelDisabled("off")).toBe(true);
    expect(managerModelDisabled("labels")).toBe(false);
    expect(managerModelDisabled("labels+model")).toBe(false);
  });
});

describe("showsHindsightFields", () => {
  // Box 5.4.
  it("reveals endpoint + key for hindsight and nothing else", () => {
    expect(showsHindsightFields("hindsight")).toBe(true);
    expect(showsHindsightFields("local")).toBe(false);
    expect(showsHindsightFields("none")).toBe(false);
  });
});

describe("starvedTimeoutMs", () => {
  // Mirrors `Teams::starved_manager_timeout_ms` (crates/config/src/teams.rs) exactly, including
  // the three things it deliberately does not do.
  it("reports the operator's own number below the floor", () => {
    expect(starvedTimeoutMs(draft({ managerTimeoutMs: 5000 }))).toBe(5000);
  });

  it("does not fire at or above the floor", () => {
    expect(starvedTimeoutMs(draft({ managerTimeoutMs: 15000 }))).toBeNull();
    expect(starvedTimeoutMs(draft({ managerTimeoutMs: 60000 }))).toBeNull();
  });

  // No other mode runs a model turn, so no other mode can be starved by this value — a warning
  // there would tell the operator something untrue.
  it("does not fire outside labels+model", () => {
    expect(starvedTimeoutMs(draft({ managerTimeoutMs: 5000, managerMode: "labels" }))).toBeNull();
    expect(starvedTimeoutMs(draft({ managerTimeoutMs: 5000, managerMode: "off" }))).toBeNull();
  });

  // Non-positive means "no value": the triage task substitutes the schema default for it.
  it("does not fire on a non-positive value", () => {
    expect(starvedTimeoutMs(draft({ managerTimeoutMs: 0 }))).toBeNull();
  });

  it("does not fire when teams is disabled outright", () => {
    expect(starvedTimeoutMs(draft({ managerTimeoutMs: 5000, enabled: false }))).toBeNull();
  });
});

describe("the TagInput bridge", () => {
  // `RosterDraft.labels` is the comma text the Podium editor stores; the console renders chips.
  it("round-trips a row's labels through the chip list", () => {
    const row = { name: "alice", profile: "swe", labels: "rust, config", bank: "", maxConcurrent: 0 };
    expect(rowLabels(row)).toEqual(["rust", "config"]);
    expect(joinRowLabels(rowLabels(row))).toBe("rust, config");
  });

  it("drops blanks rather than storing labels that match nothing", () => {
    expect(rowLabels({ name: "a", profile: "swe", labels: " , rust , ", bank: "", maxConcurrent: 0 })).toEqual(["rust"]);
    expect(joinRowLabels([])).toBe("");
  });
});
