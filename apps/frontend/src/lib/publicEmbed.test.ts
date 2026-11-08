import { describe, expect, it } from "vitest";
import { toGridWidget } from "./publicEmbed";
import type { PublicWidget } from "@/types/api";

describe("toGridWidget", () => {
  it("carries monitor tiles and their ids through to the grid widget shape", () => {
    const pw: PublicWidget = {
      id: 1,
      text: null,
      width: 1,
      visualization: null,
      monitors: [
        {
          id: 7,
          kind: "threshold",
          name: "Order backlog",
          state: "ok",
          column: "backlog",
          op: "<",
          value: 100,
          observed: 42,
          last_checked_at: "2026-01-01T00:00:00.000Z",
          muted: false,
          check_failed: false,
        },
      ],
    };

    const widget = toGridWidget(pw);

    expect(widget.monitors).toEqual(pw.monitors);
    expect(widget.options.monitors).toEqual([7]);
  });

  it("omits monitors from options when the widget has none", () => {
    const pw: PublicWidget = { id: 1, text: "hi", width: 1, visualization: null };

    const widget = toGridWidget(pw);

    expect(widget.monitors).toBeNull();
    expect(widget.options.monitors).toBeUndefined();
  });
});
