import * as React from "react";
import { TextArea, TextInput } from "@/components/ui";
import { REPO_PROMPT_PATH } from "@/lib/settings-model";

// promptFileHint returns the live relative-vs-absolute description shown under the path input. An
// absolute (/…) or ~-prefixed path is a local file on the daemon host; anything else is treated as
// repo-relative (read from each run's per-issue checkout). Mirrors the daemon's run-time resolution
// (internal/orchestrator/worker.go resolvePromptTemplate).
export function promptFileHint(path: string): string {
  const p = path.trim();
  if (p === "") return "Repo-relative · read from each run's checkout";
  if (p.startsWith("/") || p === "~" || p.startsWith("~/")) return "Local file on this machine";
  return "Repo-relative · read from each run's checkout";
}

// isLocalPromptPath reports whether a path resolves on the daemon host (absolute or ~) rather than
// repo-relative. Exported so callers can decide whether existence is verifiable now (local) or only
// at run time (repo-relative).
export function isLocalPromptPath(path: string): boolean {
  const p = path.trim();
  return p.startsWith("/") || p === "~" || p.startsWith("~/");
}

export interface PromptSourceProps {
  /** The inline prompt body (the textarea value). */
  prompt: string;
  onPromptChange: (v: string) => void;
  /** The prompt-source-file path; when non-empty the file WINS over the inline body. */
  promptFile: string;
  onPromptFileChange: (v: string) => void;
  /** Placeholder for the inline textarea (e.g. the inherited template on the per-agent editor). */
  promptPlaceholder?: string;
  /** When this scope's `promptFile` override is empty, the file inherited from the global default
   *  (the per-agent editor passes the global `promptFile`). A non-empty inherited file WINS at run
   *  time even with an empty override, so the inline editor is disabled and the checkbox reflects the
   *  inherited path — otherwise the editor implies inline edits take effect when the inherited file
   *  actually wins (INF-232). */
  inheritedFile?: string;
  /** Minimum textarea height (default 200). */
  minHeight?: number;
}

// PromptSource lets a scope (global or per-agent) choose its prompt source. The PRIMARY control is a
// checkbox that adopts the repo's canonical `.rhapsody/PROMPT.md` (the repo-level prompt convention,
// INF-279): checked stores that path and greys the inline editor; unchecked clears the path (back to
// inline, or to an inherited file). An "Advanced: custom path" disclosure keeps the original
// free-form path input + Browse for a non-canonical file. The inline body remains visible as the
// soft-fallback that a missing relative prompt_file falls back to.
export function PromptSource({
  prompt,
  onPromptChange,
  promptFile,
  onPromptFileChange,
  promptPlaceholder,
  inheritedFile,
  minHeight = 200,
}: PromptSourceProps) {
  const ownPath = promptFile.trim();
  const inherited = (inheritedFile ?? "").trim();
  // A FILE wins at run time when this scope sets a path OR — when the override is empty — the global
  // default supplies one.
  const effectiveFile = ownPath !== "" ? ownPath : inherited;
  // This scope contributes no path of its own but the global default supplies one (INF-232).
  const inheritsFile = ownPath === "" && inherited !== "";
  // The canonical repo prompt is active when the effective file (own or inherited) is exactly the
  // convention path — that's what the checkbox reflects.
  const repoPromptOn = effectiveFile === REPO_PROMPT_PATH;
  // A non-canonical OWN path drives the Advanced disclosure (a custom file the user provided).
  const customPath = ownPath !== "" && ownPath !== REPO_PROMPT_PATH;
  // Any effective file (own repo prompt, own custom path, or an inherited file) wins at run time, so
  // inline editing has no effect — grey it out. The inline body still matters as the soft-fallback.
  const inlineDisabled = effectiveFile !== "";

  // Whether the user is actively typing in the custom-path input. The open/close sync effect honours
  // props (agent switch) but must not collapse the disclosure while the user clears the field to
  // retype.
  const pathFocused = React.useRef(false);
  const [advancedOpen, setAdvancedOpen] = React.useState(customPath);
  // Keep the disclosure open whenever a custom own path exists; collapse it when the custom path goes
  // away (e.g. switching to an agent on the repo prompt) unless the user is mid-edit. Keyed on the
  // controlled promptFile (not just the derived bool) so an agent switch re-evaluates even when both
  // agents share a falsy customPath.
  React.useEffect(() => {
    if (customPath) setAdvancedOpen(true);
    else if (!pathFocused.current) setAdvancedOpen(false);
    // customPath is derived from promptFile; depend on the raw prop so every change re-runs.
  }, [promptFile, customPath]);

  const toggleRepoPrompt = () => {
    if (repoPromptOn) {
      // Untick → clear this scope's path: back to the inline body, or to an inherited file when one
      // exists (the daemon treats a non-empty prompt_file as the winning source).
      onPromptFileChange("");
    } else {
      onPromptFileChange(REPO_PROMPT_PATH);
      setAdvancedOpen(false);
    }
  };

  const local = isLocalPromptPath(promptFile);

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 14 }}>
      {/* Primary control: adopt the repo's own prompt. */}
      <label style={{ display: "flex", alignItems: "flex-start", gap: 10, cursor: "pointer" }}>
        <input
          type="checkbox"
          checked={repoPromptOn}
          onChange={toggleRepoPrompt}
          aria-label="Use this repo's prompt"
          style={{ marginTop: 2, cursor: "pointer" }}
        />
        <span style={{ display: "flex", flexDirection: "column", gap: 2 }}>
          <span style={{ fontSize: 13, fontWeight: 600, color: "var(--tx-1)" }}>
            Use this repo's prompt (<span className="mono">{REPO_PROMPT_PATH}</span>)
          </span>
          <span style={{ fontSize: 12, color: "var(--tx-3)" }}>
            {repoPromptOn
              ? inheritsFile
                ? "Inherited from the global default · read from each run's checkout (falls back to the inline prompt below if absent)"
                : "Read from each run's checkout · falls back to the inline prompt below if the file is absent"
              : "Version-control the prompt in the repo. The inline prompt below is used until the file exists."}
          </span>
        </span>
      </label>

      {/* Inline prompt — the fallback; greyed when a file wins at run time. */}
      <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
        <TextArea
          mono
          value={prompt}
          placeholder={promptPlaceholder}
          onChange={(e) => onPromptChange(e.target.value)}
          disabled={inlineDisabled}
          style={{ minHeight, opacity: inlineDisabled ? 0.5 : 1 }}
        />
        {inlineDisabled ? (
          <div style={{ fontSize: 12, color: "var(--tx-3)" }}>
            {inheritsFile && !repoPromptOn ? (
              <>
                Inheriting{" "}
                <span className="mono" style={{ color: "var(--tx-2)" }}>
                  {inherited}
                </span>{" "}
                from the global default · type a custom path below to override
              </>
            ) : (
              <>The prompt file wins at run time · this inline body is the fallback</>
            )}
          </div>
        ) : null}
      </div>

      {/* Advanced: a non-canonical custom path (the original free-form input). */}
      <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
        <button
          type="button"
          onClick={() => setAdvancedOpen((v) => !v)}
          aria-expanded={advancedOpen}
          style={{
            alignSelf: "flex-start",
            background: "transparent",
            border: "none",
            padding: 0,
            color: "var(--tx-2)",
            fontSize: 12.5,
            fontWeight: 600,
            cursor: "pointer",
          }}
        >
          {advancedOpen ? "▾" : "▸"} Advanced: custom path
        </button>
        {advancedOpen ? (
          <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
            <TextInput
              mono
              value={promptFile}
              placeholder={inheritsFile ? inherited : "prompts/PROMPT.md  or  /Users/me/prompt.md"}
              onChange={(e) => onPromptFileChange(e.target.value)}
              onFocus={() => {
                pathFocused.current = true;
              }}
              onBlur={() => {
                pathFocused.current = false;
              }}
            />
            <div style={{ fontSize: 12, color: "var(--tx-3)" }}>
              {promptFileHint(promptFile)}
              {ownPath !== "" ? (
                <span style={{ color: "var(--tx-faint)" }}>
                  {" · "}
                  {local ? "verified on this machine" : "verified when the agent runs"}
                </span>
              ) : null}
            </div>
          </div>
        ) : null}
      </div>
    </div>
  );
}
