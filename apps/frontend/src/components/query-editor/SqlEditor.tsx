import { forwardRef, useCallback, useImperativeHandle, useMemo, useRef } from "react";
import CodeMirror, { type ReactCodeMirrorRef } from "@uiw/react-codemirror";
import { sql, PostgreSQL } from "@codemirror/lang-sql";
import { EditorView } from "@codemirror/view";
import type { Text } from "@codemirror/state";
import { snippet as cmSnippet, snippetCompletion, type CompletionSource } from "@codemirror/autocomplete";
import type { DataSourceSchema, Snippet } from "@/types/api";

/** A snippet inserted after other non-whitespace content needs its own line, but not at line start
 * or after whitespace. */
function separatorBefore(doc: Text, pos: number): string {
  return pos > 0 && !/\s/.test(doc.sliceString(pos - 1, pos)) ? "\n" : "";
}

/** Extra completion source (composed with the schema completions from `sql()`) offering each org snippet's
 * trigger via the same `snippet()` applier as `insertSnippet`. Exported for tests. */
export function snippetCompletionSource(snippets: Snippet[]): CompletionSource {
  return (context) => {
    const word = context.matchBefore(/\w+/);
    if (!word || (word.from === word.to && !context.explicit) || snippets.length === 0) {
      return null;
    }
    const sep = separatorBefore(context.state.doc, word.from);
    return {
      from: word.from,
      options: snippets.map((s) =>
        snippetCompletion(sep + s.snippet, {
          label: s.trigger,
          detail: s.description ?? undefined,
        }),
      ),
      validFor: /^\w*$/,
    };
  };
}

export interface SqlEditorHandle {
  insertAtCursor: (text: string) => void;
  /**
   * Insert a TextMate-style snippet (`${1:placeholder}` tab stops) at the cursor; Tab/Shift-Tab move
   * between stops, Escape clears. Appends literally if the view is not mounted yet. `trigger` is the
   * typed word that invoked it: with no selection and matching text before the cursor, that text is
   * replaced.
   */
  insertSnippet: (template: string, trigger?: string) => void;
}

interface SqlEditorProps {
  value: string;
  onChange: (value: string) => void;
  className?: string;
  schema?: DataSourceSchema | null;
  snippets?: Snippet[];
  readOnly?: boolean;
}

export const SqlEditor = forwardRef<SqlEditorHandle, SqlEditorProps>(function SqlEditor(
  { value, onChange, className, schema, snippets, readOnly },
  ref,
) {
  const cmRef = useRef<ReactCodeMirrorRef>(null);

  const sqlSchema = useMemo(() => {
    if (!schema?.tables?.length) return undefined;
    return Object.fromEntries(schema.tables.map((t) => [t.name, t.columns.map((c) => c.name)]));
  }, [schema]);

  // `@uiw/react-codemirror` reconfigures the whole extension set (wiping the snippet tab-stop state
  // `insertSnippet` installs) whenever `extensions`, `basicSetup` or `onChange` change reference,
  // and every keystroke re-renders here. So all three must stay referentially stable.
  const extensions = useMemo(() => {
    const sqlLang = sql({ dialect: PostgreSQL, schema: sqlSchema });
    return [
      sqlLang,
      // A second `autocomplete` source on the language data composes with `sql()`'s schema
      // completions; `autocompletion({ override })` would drop them.
      sqlLang.language.data.of({ autocomplete: snippetCompletionSource(snippets ?? []) }),
      EditorView.lineWrapping,
      EditorView.theme(
        {
          "&": { background: "#ffffff", color: "#0a0a0a" },
          ".cm-gutters": { background: "#fafafa", borderRight: "1px solid #e5e5e5", color: "#a3a3a3" },
          ".cm-activeLineGutter": { background: "#f5f5f5" },
          ".cm-activeLine": { background: "#fafafa" },
          "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, .cm-content ::selection": {
            backgroundColor: "rgba(23, 23, 23, 0.12) !important",
          },
          "&.cm-focused .cm-cursor": { borderLeftColor: "#171717" },
          ".cm-tooltip": { background: "#ffffff", border: "1px solid #e5e5e5", color: "#0a0a0a" },
          ".cm-tooltip.cm-tooltip-autocomplete > ul > li[aria-selected]": {
            background: "rgba(23,23,23,0.08)",
            color: "#0a0a0a",
          },
        },
        { dark: false },
      ),
    ];
  }, [sqlSchema, snippets]);
  const basicSetup = useMemo(() => ({ lineNumbers: true, foldGutter: true, autocompletion: true }), []);

  // An inline `onChange` has a new identity every render, which would reconfigure the editor; give
  // CodeMirror a stable wrapper that reads the latest callback from a ref.
  const onChangeRef = useRef(onChange);
  onChangeRef.current = onChange;
  const stableOnChange = useCallback((value: string) => onChangeRef.current(value), []);

  useImperativeHandle(ref, () => ({
    insertAtCursor: (text: string) => {
      const view = cmRef.current?.view;
      if (!view) {
        onChange(value + text);
        return;
      }
      const { from, to } = view.state.selection.main;
      const insert = separatorBefore(view.state.doc, from) + text;
      view.dispatch({
        changes: { from, to, insert },
        selection: { anchor: from + insert.length },
      });
      view.focus();
    },
    insertSnippet: (template: string, trigger?: string) => {
      const view = cmRef.current?.view;
      if (!view) {
        onChange(value + template);
        return;
      }
      let { from, to } = view.state.selection.main;
      if (trigger && from === to) {
        const triggerStart = from - trigger.length;
        if (triggerStart >= 0 && view.state.doc.sliceString(triggerStart, from) === trigger) {
          from = triggerStart;
        }
      }
      // `cmSnippet(template)` inserts and activates the tab stops; a `null` completion is fine as
      // we skip the list UI.
      cmSnippet(separatorBefore(view.state.doc, from) + template)(view, null, from, to);
      view.focus();
    },
  }));

  return (
    <CodeMirror
      ref={cmRef}
      value={value}
      onChange={stableOnChange}
      editable={!readOnly}
      height="100%"
      className={className}
      extensions={extensions}
      basicSetup={basicSetup}
    />
  );
});
