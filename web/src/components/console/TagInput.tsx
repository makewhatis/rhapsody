import { useState, type KeyboardEvent } from "react";
import { cn } from "@/lib/utils";

export interface TagInputProps {
  tags: readonly string[];
  onChange: (next: string[]) => void;
  /** Required: the inline field has no visible label of its own. */
  label: string;
  placeholder?: string;
  className?: string;
}

// TagInput — chip-tags with an inline add field (STUDIO-681 §1.3). The draft text is local
// state; the committed tags are the caller's, so the manage-team form (§7) stays the single
// owner of what will be written to teams.yaml.
export function TagInput({ tags, onChange, label, placeholder, className }: TagInputProps) {
  const [draft, setDraft] = useState("");

  const commit = () => {
    const value = draft.trim();
    setDraft("");
    // A blank label routes nothing and a duplicate routes nothing twice — drop both
    // silently rather than writing a tag the daemon would ignore.
    if (value === "" || tags.includes(value)) return;
    onChange([...tags, value]);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "Enter" || event.key === ",") {
      event.preventDefault();
      commit();
      return;
    }
    if (event.key === "Backspace" && draft === "" && tags.length > 0) {
      event.preventDefault();
      onChange(tags.slice(0, -1));
    }
  };

  return (
    <div className={cn("tags", className)}>
      {tags.map((tag) => (
        <span className="tk" key={tag}>
          {tag}
          <button type="button" className="rm" aria-label={`Remove ${tag}`} onClick={() => onChange(tags.filter((t) => t !== tag))}>
            ×
          </button>
        </span>
      ))}
      <input
        type="text"
        aria-label={label}
        placeholder={placeholder}
        value={draft}
        onChange={(event) => setDraft(event.target.value)}
        onKeyDown={onKeyDown}
        onBlur={commit}
      />
    </div>
  );
}
