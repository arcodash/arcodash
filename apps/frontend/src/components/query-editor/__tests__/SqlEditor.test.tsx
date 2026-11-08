import { describe, it, expect } from "vitest";
import { act, fireEvent, render } from "@testing-library/react";
import { createRef, useState } from "react";
import { CompletionContext } from "@codemirror/autocomplete";
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { sql, PostgreSQL } from "@codemirror/lang-sql";
import {
  SqlEditor,
  snippetCompletionSource,
  type SqlEditorHandle,
} from "@/components/query-editor/SqlEditor";
import type { Snippet } from "@/types/api";

/** Places the cursor at a known doc position; CodeMirror can't measure in jsdom, so simulated
 * keystrokes are unreliable. */
function mountAt(initialText: string, snippets?: Snippet[]) {
  const ref = createRef<SqlEditorHandle>();
  let value = "";
  const onChange = (v: string) => {
    value = v;
  };
  render(<SqlEditor ref={ref} value={value} onChange={onChange} snippets={snippets} />);
  ref.current!.insertAtCursor(initialText);
  return {
    handle: ref.current!,
    getValue: () => value,
  };
}

const TOP10: Snippet = {
  id: 1,
  trigger: "top10",
  description: "Top 10 by count",
  snippet: "SELECT * FROM ${1:table} ORDER BY ${2:count} DESC LIMIT 10",
  user_id: 1,
  created_at: "",
  updated_at: "",
};

describe("SqlEditor.insertSnippet", () => {
  it("replaces a typed trigger word sitting right before the cursor", () => {
    const { handle, getValue } = mountAt("select top10");
    handle.insertSnippet("SELECT * FROM t ORDER BY c DESC LIMIT 10", "top10");
    expect(getValue()).toBe("select SELECT * FROM t ORDER BY c DESC LIMIT 10");
  });

  it("falls back to inserting at the cursor when the preceding text doesn't match the trigger", () => {
    const { handle, getValue } = mountAt("select ");
    handle.insertSnippet("SELECT * FROM t LIMIT 10", "top10");
    expect(getValue()).toBe("select SELECT * FROM t LIMIT 10");
  });

  it("prepends a newline separator when inserted right after other content, with no trigger given", () => {
    const { handle, getValue } = mountAt("select top10");
    handle.insertSnippet("SELECT * FROM t LIMIT 10");
    expect(getValue()).toBe("select top10\nSELECT * FROM t LIMIT 10");
  });

  it("does not treat a trigger shorter than the cursor offset as reaching past the start of the doc, and still separates the insert", () => {
    const { handle, getValue } = mountAt("10");
    handle.insertSnippet("SELECT * FROM t LIMIT 10", "top10");
    expect(getValue()).toBe("10\nSELECT * FROM t LIMIT 10");
  });

  it("adds no separator when the insertion point is already at the start of a line", () => {
    const { handle, getValue } = mountAt("select 1\n");
    handle.insertSnippet("SELECT 2");
    expect(getValue()).toBe("select 1\nSELECT 2");
  });
});

describe("SqlEditor.insertAtCursor", () => {
  it("prepends a newline when inserted right after other non-whitespace content", () => {
    const { handle, getValue } = mountAt("SELECT 1");
    handle.insertAtCursor("SELECT 2");
    expect(getValue()).toBe("SELECT 1\nSELECT 2");
  });

  it("adds no separator when already preceded by whitespace", () => {
    const { handle, getValue } = mountAt("SELECT ");
    handle.insertAtCursor("orders.id");
    expect(getValue()).toBe("SELECT orders.id");
  });
});

describe("SqlEditor snippet tab-stop chaining across a real re-render", () => {
  /** Mirrors QueryEditorPage: controlled `value` with a fresh inline `onChange` each render;
   * `mountAt` never re-renders so can't catch state lost across renders. */
  function Controlled({ handleRef, snippets }: { handleRef: React.RefObject<SqlEditorHandle | null>; snippets?: Snippet[] }) {
    const [value, setValue] = useState("");
    return <SqlEditor ref={handleRef} value={value} onChange={(v) => setValue(v)} snippets={snippets} />;
  }

  it("keeps the tab-stop session alive after the insert-triggered re-render, so Tab reaches the second field", () => {
    const handle = createRef<SqlEditorHandle>();
    render(<Controlled handleRef={handle} />);

    act(() => {
      handle.current!.insertSnippet("SELECT * FROM ${1:table} ORDER BY ${2:col} DESC LIMIT 10");
    });

    const editorEl = document.querySelector(".cm-editor");
    expect(editorEl).not.toBeNull();
    const view = EditorView.findFromDOM(editorEl as HTMLElement);
    expect(view).not.toBeNull();
    const contentEl = editorEl!.querySelector(".cm-content") as HTMLElement;

    const doc = view!.state.doc.toString();
    const firstFrom = view!.state.selection.main.from;
    const firstTo = view!.state.selection.main.to;
    expect(doc.slice(firstFrom, firstTo)).toBe("table");

    act(() => {
      fireEvent.keyDown(contentEl, { key: "Tab", code: "Tab", keyCode: 9 });
    });

    // An unstable `onChange` reconfigure would drop the snippet keymap, leaving the selection on
    // "table" instead of "col".
    const { from, to } = view!.state.selection.main;
    expect(view!.state.doc.toString().slice(from, to)).toBe("col");
  });
});

describe("snippetCompletionSource", () => {
  /** A bare state suffices: the source only reads `state`/`pos`/`explicit`. */
  function contextAt(doc: string, pos: number) {
    const state = EditorState.create({ doc });
    return new CompletionContext(state, pos, false);
  }

  it("returns no completions when there are no snippets", async () => {
    const source = snippetCompletionSource([]);
    expect(await source(contextAt("select top10", 12))).toBeNull();
  });

  it("returns no completions when the cursor isn't inside a word", async () => {
    const source = snippetCompletionSource([TOP10]);
    expect(await source(contextAt("select ", 7))).toBeNull();
  });

  it("produces a completion entry labelled with the trigger, anchored at the start of the typed word", async () => {
    const source = snippetCompletionSource([TOP10]);
    const result = await source(contextAt("select top10", 12));
    expect(result).not.toBeNull();
    expect(result!.from).toBe("select ".length);
    expect(result!.options.map((o) => o.label)).toEqual(["top10"]);
    expect(result!.options[0].detail).toBe("Top 10 by count");
  });

  it("matches on a partial prefix of the trigger, same as CodeMirror's own fuzzy filtering expects", async () => {
    const source = snippetCompletionSource([TOP10]);
    const result = await source(contextAt("select top", 10));
    expect(result?.options.map((o) => o.label)).toEqual(["top10"]);
  });

  it("applying the completion expands the body with tab stops intact, via the same snippet() applier insertSnippet uses", async () => {
    const source = snippetCompletionSource([TOP10]);
    const doc = "select top10";
    const result = (await source(contextAt(doc, doc.length)))!;
    const option = result.options[0];

    const state = EditorState.create({ doc, extensions: [sql({ dialect: PostgreSQL })] });
    const view = new EditorView({ state });
    (option.apply as (view: EditorView, completion: typeof option, from: number, to: number) => void)(view, option, result.from, doc.length);

    // Placeholders expand with the first field selected, proving the real snippet applier ran.
    expect(view.state.doc.toString()).toBe(
      "select SELECT * FROM table ORDER BY count DESC LIMIT 10",
    );
    view.destroy();
  });

  it("prepends a newline separator in the expanded body when the trigger sits right after other content", async () => {
    const source = snippetCompletionSource([TOP10]);
    // A non-word char before the trigger stops `\w+` swallowing it, with no whitespace ahead.
    const doc = "LIMIT 10)top10";
    const result = (await source(contextAt(doc, doc.length)))!;
    const option = result.options[0];

    const state = EditorState.create({ doc, extensions: [sql({ dialect: PostgreSQL })] });
    const view = new EditorView({ state });
    (option.apply as (view: EditorView, completion: typeof option, from: number, to: number) => void)(view, option, result.from, doc.length);

    expect(view.state.doc.toString()).toBe(
      "LIMIT 10)\nSELECT * FROM table ORDER BY count DESC LIMIT 10",
    );
    view.destroy();
  });
});
