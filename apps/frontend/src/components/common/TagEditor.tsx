import { useState } from "react";
import { X } from "lucide-react";

/** Tag chips plus, when `editable`, remove buttons and an "add tag…" input (Enter, comma or blur
 * commits). `onChange` receives the full next tag list; edits are ignored while `pending`. */
export function TagEditor({
  tags,
  editable,
  pending,
  onChange,
  suggestions,
}: {
  tags: string[];
  editable: boolean;
  pending: boolean;
  onChange: (tags: string[]) => void;
  /** Offered via a `<datalist>` with this `id`. */
  suggestions?: { id: string; tags: string[] };
}) {
  const [input, setInput] = useState("");

  function commitInput() {
    const t = input.trim();
    setInput("");
    if (!t || tags.includes(t) || pending) return;
    onChange([...tags, t]);
  }

  return (
    <div className="flex flex-wrap items-center gap-1">
      {tags.map((t) => (
        <span
          key={t}
          className="flex items-center gap-1 rounded-full bg-muted px-2 py-0.5 text-xs text-muted-foreground"
        >
          {t}
          {editable && (
            <button
              type="button"
              disabled={pending}
              onClick={() => !pending && onChange(tags.filter((x) => x !== t))}
              className="ml-0.5 cursor-pointer hover:text-destructive disabled:opacity-50"
              aria-label={`Remove tag ${t}`}
            >
              <X className="h-3 w-3" />
            </button>
          )}
        </span>
      ))}
      {editable && (
        <>
          <input
            list={suggestions?.id}
            className="h-6 w-24 rounded border border-input bg-transparent px-2 text-xs outline-none disabled:opacity-50"
            placeholder="add tag…"
            aria-label="Add tag"
            disabled={pending}
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" || e.key === ",") {
                e.preventDefault();
                commitInput();
              }
            }}
            onBlur={() => input.trim() && commitInput()}
          />
          {suggestions && (
            <datalist id={suggestions.id}>
              {suggestions.tags.map((t) => (
                <option key={t} value={t} />
              ))}
            </datalist>
          )}
        </>
      )}
    </div>
  );
}
