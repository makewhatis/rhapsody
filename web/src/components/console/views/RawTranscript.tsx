import { memo, useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchRawTranscript, type RawTranscriptLine, type RawTranscriptPage } from "@/lib/api";

const bytes = (text: string) => new TextEncoder().encode(text).length;

// Read only the top-level timestamp value. Skip strings/containers without decoding tool
// payloads: collapsed rows must never JSON.parse a several-MB result just to show its clock.
function lineTimestamp(text: string): Date | null {
  let depth = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (c === "{" || c === "[") depth++;
    else if (c === "}" || c === "]") depth--;
    else if (c === '"') {
      const start = i++;
      while (i < text.length && text[i] !== '"') {
        if (text[i] === "\\") i++;
        i++;
      }
      if (depth !== 1 || i + 1 - start !== 11 || text.slice(start, i + 1) !== '"timestamp"') {
        continue;
      }
      const match = /^\s*:\s*("[^"\\]{1,64}"|\d{1,16}(?!\d))/.exec(text.slice(i + 1));
      if (!match) continue;
      try {
        const value: unknown = JSON.parse(match[1]);
        if (typeof value !== "string" && typeof value !== "number") return null;
        const date = new Date(value);
        return Number.isFinite(date.getTime()) ? date : null;
      } catch {
        return null;
      }
    }
  }
  return null;
}

// Validate once expanded, then change only whitespace between lexical tokens. Serializing
// parsed objects would round large numbers, discard duplicate keys and rewrite escapes.
function prettyLine(text: string): string {
  try {
    JSON.parse(text);
    const tokens = text.match(/"(?:\\[\s\S]|[^"\\])*"|[{}\[\],:]|[^\s{}\[\],:]+/g) ?? [];
    const out: string[] = [];
    let depth = 0;
    const newline = () => out.push("\n", "  ".repeat(depth));
    for (let i = 0; i < tokens.length; i++) {
      const token = tokens[i];
      if (token === "{" || token === "[") {
        out.push(token);
        depth++;
        if (tokens[i + 1] !== "}" && tokens[i + 1] !== "]") newline();
      } else if (token === "}" || token === "]") {
        depth--;
        if (tokens[i - 1] !== "{" && tokens[i - 1] !== "[") newline();
        out.push(token);
      } else if (token === ",") {
        out.push(token);
        newline();
      } else {
        out.push(token === ":" ? ": " : token);
      }
    }
    return out.join("");
  } catch {
    return text;
  }
}

const RawRow = memo(function RawRow({ line }: { line: RawTranscriptLine }) {
  const [expanded, setExpanded] = useState(false);
  const timestamp = useMemo(() => lineTimestamp(line.text), [line.text]);
  const size = useMemo(() => bytes(line.text), [line.text]);
  const full = useMemo(() => expanded ? prettyLine(line.text) : "", [expanded, line.text]);
  const previewEnd = /[\uD800-\uDBFF]/.test(line.text[199] ?? "") ? 199 : 200;
  const clipped = line.text.length > previewEnd;
  return (
    <div className="rawline">
      <button
        className="trraw-row"
        type="button"
        aria-expanded={expanded}
        onClick={() => setExpanded((value) => !value)}
      >
        <span className="trraw-time">
          {timestamp ? (
            <time dateTime={timestamp.toISOString()} title={timestamp.toLocaleString()}>
              {timestamp.toLocaleTimeString()}
            </time>
          ) : "—"}
        </span>
        <span className="trraw-offset">@{line.offset.toLocaleString()}</span>
        <span className="trraw-preview">
          {line.text.slice(0, previewEnd)}{clipped ? "…" : ""}
        </span>
        <span className="trraw-size">
          {size.toLocaleString()} B · {expanded ? "Collapse" : "Expand full line"}
        </span>
      </button>
      {expanded ? <pre className="trraw-full">{full}</pre> : null}
    </div>
  );
});

export function RawTranscript({ runId, inFlight }: { runId: number; inFlight: boolean }) {
  const [following, setFollowing] = useState(true);
  const [window, setWindow] = useState<RawTranscriptPage | null>(null);
  const [loading, setLoading] = useState(false);
  const [pageError, setPageError] = useState("");
  const end = useRef<HTMLDivElement>(null);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);
  const tail = useQuery({
    queryKey: ["raw-transcript-tail", runId],
    queryFn: () => fetchRawTranscript(runId),
    enabled: runId > 0 && following,
    refetchInterval: inFlight && following ? 1500 : false,
    staleTime: inFlight ? 0 : Infinity,
    // Raw can be closed while live and reopened after completion with a cached pre-final tail.
    refetchOnMount: "always",
    refetchOnWindowFocus: false,
  });
  // Following retains only the current tail page. The displayed byte range and Load earlier
  // expose its boundary; a long live run never accumulates its whole history in the DOM.
  useEffect(() => {
    if (following && tail.data) setWindow(tail.data);
  }, [tail.data, following]);
  useEffect(() => {
    if (following && inFlight) end.current?.scrollIntoView?.({ block: "nearest" });
  }, [window, following, inFlight]);
  const wasInFlight = useRef(inFlight);
  const needsFinalRead = useRef(false);
  useEffect(() => {
    if (wasInFlight.current && !inFlight) {
      if (following) void tail.refetch();
      // A paused EOF is an older snapshot: final output can arrive after the last page read.
      // Leave one later read available even when no intervening page saw the file grow.
      else {
        needsFinalRead.current = true;
        setWindow((current) => current ? { ...current, at_end: false } : current);
      }
    }
    wasInFlight.current = inFlight;
  }, [inFlight, following, tail.refetch]);

  async function load(direction: "backward" | "forward") {
    if (!window || loading) return;
    const last = window.lines.at(-1);
    // An EOF cursor remains useful on a live file even though next_cursor is null.
    // Re-read an unterminated last line from its start rather than splitting its growing bytes.
    const later = last ? last.offset + (last.text.endsWith("\n") ? bytes(last.text) : 0) : 0;
    const cursor = direction === "backward" ? window.prev_cursor : (window.next_cursor ?? later);
    if (cursor === null) return;
    // An earlier read begun before termination cannot satisfy the final forward-read obligation.
    const finalRead = direction === "forward" && !inFlight;
    setFollowing(false);
    setLoading(true);
    setPageError("");
    try {
      const page = await fetchRawTranscript(runId, cursor, direction);
      if (!mounted.current) return;
      if (finalRead) needsFinalRead.current = false;
      setWindow((current) => {
        if (!current || page.missing) return page;
        const lines = new Map(current.lines.map((line) => [line.offset, line]));
        for (const line of page.lines) lines.set(line.offset, line);
        const merged = [...lines.values()].sort((a, b) => a.offset - b.offset);
        const last = merged.at(-1);
        const loadedEnd = last ? last.offset + bytes(last.text) : 0;
        const size = Math.max(current.size_bytes, page.size_bytes);
        return {
          ...page,
          lines: merged,
          size_bytes: size,
          prev_cursor: direction === "backward" ? page.prev_cursor : current.prev_cursor,
          next_cursor: direction === "forward" ? page.next_cursor : current.next_cursor,
          at_start: direction === "backward" ? page.at_start : current.at_start,
          at_end: loadedEnd >= size && !needsFinalRead.current,
        };
      });
    } catch (error) {
      if (mounted.current) {
        setPageError(error instanceof Error ? error.message : "Raw transcript unavailable");
      }
    } finally {
      if (mounted.current) setLoading(false);
    }
  }

  const first = window?.lines[0];
  const last = window?.lines.at(-1);
  const rangeEnd = last ? last.offset + bytes(last.text) : 0;
  return (
    <div className="trraw">
      <div className="trraw-toolbar">
        <span>
          {window
            ? `Showing bytes ${first?.offset.toLocaleString() ?? "0"}–${rangeEnd.toLocaleString()} of ${window.size_bytes.toLocaleString()}`
            : "Raw transcript"}
        </span>
        <a href={`/api/v1/runs/${runId}/transcript.jsonl`} download>Download .jsonl</a>
        <a href={`/api/v1/runs/${runId}/stderr.log`} download>Download stderr</a>
      </div>
      <div className="trraw-toolbar">
        <button
          type="button"
          disabled={loading || !window || window.at_start}
          onClick={() => void load("backward")}
        >Load earlier</button>
        <button
          type="button"
          disabled={loading || !window || (window.at_end && !inFlight)}
          onClick={() => void load("forward")}
        >Load later</button>
        <button
          type="button"
          disabled={loading || following}
          onClick={() => {
            setPageError("");
            setFollowing(true);
            void tail.refetch();
          }}
        >{following ? (inFlight ? "Following tail" : "At tail") : "Jump to tail"}</button>
        <span className="trraw-hint">
          Rows preview 200 characters. Expand for full JSON or verbatim text.
        </span>
      </div>
      {pageError || tail.error ? (
        <div role="alert">{pageError || tail.error?.message}</div>
      ) : null}
      {window?.lines.map((line) => <RawRow key={line.offset} line={line} />)}
      {!tail.error && !pageError && (!window || window.lines.length === 0) ? (
        <div className="empty">
          {tail.isPending ? "Loading transcript…" : window?.missing
            ? "Transcript file is missing or pruned."
            : "No transcript recorded for this run."}
        </div>
      ) : null}
      <div ref={end} />
    </div>
  );
}
