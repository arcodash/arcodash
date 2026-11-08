import { describe, it, expect, afterEach, vi } from "vitest";
import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ResultsTable } from "../ResultsTable";
import { formatDateValue } from "@/lib/formatDate";
import type { ResultData } from "@/types/api";

describe("ResultsTable", () => {
  // Page-size preference is a real localStorage key; clear it so tests don't leak.
  afterEach(() => {
    window.localStorage.removeItem("results-page-size");
  });

  it("renders the empty state without rows", () => {
    const data: ResultData = { columns: [{ name: "id", friendly_type: "integer" }], rows: [] };
    render(<ResultsTable data={data} />);
    expect(screen.getByText("No rows returned.")).toBeInTheDocument();
  });

  it("renders duplicate-named columns as distinct cells reading the correct value", () => {
    const data: ResultData = {
      columns: [
        { name: "id", friendly_type: "integer" },
        { name: "id", friendly_type: "integer" },
      ],
      rows: [{ id: 1 }],
    };
    render(<ResultsTable data={data} />);
    expect(screen.getAllByText("id")).toHaveLength(2);
    expect(screen.getAllByText("1")).toHaveLength(2);
  });

  it("formats a bare date as the local calendar day, not the UTC-shifted one", () => {
    const data: ResultData = {
      columns: [{ name: "d", friendly_type: "date" }],
      rows: [{ d: "2024-01-15" }],
    };
    render(<ResultsTable data={data} />);
    const expected = new Date(2024, 0, 15).toLocaleDateString();
    expect(screen.getByText(expected)).toBeInTheDocument();
  });

  it("formats a datetime with a real time component in local time", () => {
    const data: ResultData = {
      columns: [{ name: "ts", friendly_type: "datetime" }],
      rows: [{ ts: "2024-01-15T10:30:00Z" }],
    };
    render(<ResultsTable data={data} />);
    const expected = formatDateValue("2024-01-15T10:30:00Z", "datetime");
    expect(screen.getByText(expected)).toBeInTheDocument();
  });

  it("formats a UTC-midnight day-bucketed datetime as a day, not a shifted local timestamp", () => {
    const data: ResultData = {
      columns: [{ name: "ts", friendly_type: "datetime" }],
      rows: [{ ts: "2026-07-05T00:00:00Z" }],
    };
    render(<ResultsTable data={data} />);
    expect(screen.getByText("Jul 5, 2026")).toBeInTheDocument();
  });

  it("formats a datetime column that is all UTC-midnight on the 1st as a month", () => {
    const data: ResultData = {
      columns: [{ name: "ts", friendly_type: "datetime" }],
      rows: [{ ts: "2022-07-01T00:00:00Z" }, { ts: "2022-08-01T00:00:00Z" }],
    };
    render(<ResultsTable data={data} />);
    expect(screen.getByText("Jul 2022")).toBeInTheDocument();
    expect(screen.getByText("Aug 2022")).toBeInTheDocument();
  });

  it("paginates large result sets instead of rendering every row, matching the Table viz", () => {
    const rows = Array.from({ length: 500 }, (_, i) => ({ id: i, name: `row-${i}` }));
    const data: ResultData = {
      columns: [
        { name: "id", friendly_type: "integer" },
        { name: "name", friendly_type: "string" },
      ],
      rows,
    };
    render(<ResultsTable data={data} />);
    expect(screen.getByText("row-0")).toBeInTheDocument();
    expect(screen.queryByText("row-20")).not.toBeInTheDocument();
    expect(screen.getByText("Showing 1–15 of 500 rows")).toBeInTheDocument();
    expect(screen.getByLabelText("Page number")).toHaveValue(1);
    expect(screen.getByText("of 34")).toBeInTheDocument();
  });

  describe("pagination range, page size, and jump-to-page", () => {
    function bigData(count: number): ResultData {
      return {
        columns: [{ name: "id", friendly_type: "integer" }],
        rows: Array.from({ length: count }, (_, i) => ({ id: i })),
      };
    }

    it("shows the range sentence even on a single page, with no jump-to-page control", () => {
      render(<ResultsTable data={bigData(8)} />);
      expect(screen.getByText("Showing 1–8 of 8 rows")).toBeInTheDocument();
      expect(screen.queryByLabelText("Page number")).not.toBeInTheDocument();
    });

    it("does not render a pagination bar for zero rows", () => {
      render(<ResultsTable data={bigData(0)} />);
      expect(screen.queryByText(/Showing/)).not.toBeInTheDocument();
    });

    it("changing rows-per-page resets to page 1, updates the range, and persists the choice", async () => {
      const user = userEvent.setup();
      render(<ResultsTable data={bigData(60)} />);
      expect(screen.getByText("Showing 1–15 of 60 rows")).toBeInTheDocument();

      await user.click(screen.getByRole("combobox"));
      await user.click(await screen.findByRole("option", { name: "50" }));

      expect(screen.getByText("Showing 1–50 of 60 rows")).toBeInTheDocument();
      expect(window.localStorage.getItem("results-page-size")).toBe("50");
    });

    it("a persisted page-size choice applies to the next table mounted (query editor use only)", () => {
      window.localStorage.setItem("results-page-size", "100");
      render(<ResultsTable data={bigData(150)} />);
      expect(screen.getByText("Showing 1–100 of 150 rows")).toBeInTheDocument();
    });

    it("ignores a stored page size outside the offered options", () => {
      window.localStorage.setItem("results-page-size", "999");
      render(<ResultsTable data={bigData(60)} />);
      expect(screen.getByText("Showing 1–15 of 60 rows")).toBeInTheDocument();
    });

    it("renders with the default page size and doesn't throw when localStorage access is blocked", async () => {
      const getItemSpy = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
        throw new DOMException("blocked", "SecurityError");
      });
      const setItemSpy = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
        throw new DOMException("blocked", "SecurityError");
      });
      try {
        const user = userEvent.setup();
        render(<ResultsTable data={bigData(60)} />);
        expect(screen.getByText("Showing 1–15 of 60 rows")).toBeInTheDocument();

        await user.click(screen.getByRole("combobox"));
        await expect(user.click(await screen.findByRole("option", { name: "50" }))).resolves.not.toThrow();
        expect(screen.getByText("Showing 1–50 of 60 rows")).toBeInTheDocument();
      } finally {
        getItemSpy.mockRestore();
        setItemSpy.mockRestore();
      }
    });

    it("the Table visualization (showTypeBadges=false) ignores the persisted preference and keeps its own itemsPerPage", () => {
      window.localStorage.setItem("results-page-size", "100");
      render(<ResultsTable data={bigData(60)} itemsPerPage={25} showTypeBadges={false} />);
      expect(screen.getByText(/page 1 of 3/i)).toBeInTheDocument();
    });

    it("jump-to-page commits on Enter and clamps an out-of-range value silently", async () => {
      const user = userEvent.setup();
      render(<ResultsTable data={bigData(500)} />);
      const jump = screen.getByLabelText("Page number");

      await user.clear(jump);
      await user.type(jump, "9999{Enter}");
      expect(jump).toHaveValue(34); // clamped to the last page (34 pages of 15)
      expect(screen.getByText("Showing 496–500 of 500 rows")).toBeInTheDocument();
    });

    it("jump-to-page commits on blur and reverts a non-numeric value to the current page", async () => {
      const user = userEvent.setup();
      render(<ResultsTable data={bigData(500)} />);
      const jump = screen.getByLabelText("Page number");

      await user.clear(jump);
      await user.type(jump, "abc");
      await user.tab();
      expect(jump).toHaveValue(1);
      expect(screen.getByText("Showing 1–15 of 500 rows")).toBeInTheDocument();
    });

    it("Previous/Next keep the jump-to-page box in sync", async () => {
      const user = userEvent.setup();
      render(<ResultsTable data={bigData(45)} />);
      await user.click(screen.getByRole("button", { name: "Next" }));
      expect(screen.getByLabelText("Page number")).toHaveValue(2);
      expect(screen.getByText("Showing 16–30 of 45 rows")).toBeInTheDocument();
    });
  });

  it("sorting is available on the results preview, matching the Table viz", async () => {
    const user = userEvent.setup();
    const data: ResultData = {
      columns: [{ name: "name", friendly_type: "string" }],
      rows: [{ name: "b" }, { name: "a" }],
    };
    const { container } = render(<ResultsTable data={data} />);
    await user.click(screen.getByRole("button", { name: /name/i }));
    const cells = Array.from(container.querySelectorAll("tbody tr")).map((tr) => tr.textContent);
    expect(cells).toEqual(["a", "b"]);
  });

  it("sorts a NUMERIC (string-encoded) column numerically, not alphanumerically", async () => {
    const user = userEvent.setup();
    const data: ResultData = {
      columns: [{ name: "amount", friendly_type: "float" }],
      rows: [{ amount: "1.5" }, { amount: "1.25" }, { amount: "-5" }, { amount: "-3" }],
    };
    const { container } = render(<ResultsTable data={data} />);
    await user.click(screen.getByRole("button", { name: /amount/i }));
    const cells = Array.from(container.querySelectorAll("tbody tr")).map((tr) => tr.textContent);
    expect(cells).toEqual(["-5", "-3", "1.25", "1.5"]);
  });

  it("sorts a null numeric cell after every real value", async () => {
    const user = userEvent.setup();
    const data: ResultData = {
      columns: [{ name: "amount", friendly_type: "float" }],
      rows: [{ amount: "2" }, { amount: null }, { amount: "1" }],
    };
    const { container } = render(<ResultsTable data={data} />);
    await user.click(screen.getByRole("button", { name: /amount/i }));
    const cells = Array.from(container.querySelectorAll("tbody tr")).map((tr) => tr.textContent);
    expect(cells).toEqual(["1", "2", "—"]);
  });

  it("renders a null cell as an em-dash", () => {
    const data: ResultData = {
      columns: [{ name: "n", friendly_type: "string" }],
      rows: [{ n: null }],
    };
    render(<ResultsTable data={data} />);
    expect(screen.getByText("—")).toBeInTheDocument();
  });

  describe("numeric formatting", () => {
    it("applies thousands separators to an integer column, right-aligned", () => {
      const data: ResultData = {
        columns: [{ name: "orders", friendly_type: "integer" }],
        rows: [{ orders: 1234567 }],
      };
      const { container } = render(<ResultsTable data={data} />);
      expect(screen.getByText("1,234,567")).toBeInTheDocument();
      expect(container.querySelector("td")).toHaveClass("text-right");
      expect(container.querySelector("th")).toHaveClass("text-right");
    });

    it("preserves the source's exact decimal precision for a string-encoded NUMERIC value", () => {
      const data: ResultData = {
        columns: [{ name: "revenue", friendly_type: "float" }],
        rows: [{ revenue: "6853680.00" }],
      };
      render(<ResultsTable data={data} />);
      expect(screen.getByText("6,853,680.00")).toBeInTheDocument();
    });

    it("formats a plain JS float number without altering its own precision", () => {
      const data: ResultData = {
        columns: [{ name: "avg", friendly_type: "float" }],
        rows: [{ avg: 1159389.52 }],
      };
      render(<ResultsTable data={data} />);
      expect(screen.getByText("1,159,389.52")).toBeInTheDocument();
    });

    it("does not reformat a numeric-looking value in a non-numeric column", () => {
      const data: ResultData = {
        columns: [{ name: "code", friendly_type: "string" }],
        rows: [{ code: "1234567" }],
      };
      const { container } = render(<ResultsTable data={data} />);
      expect(screen.getByText("1234567")).toBeInTheDocument();
      expect(container.querySelector("td")).not.toHaveClass("text-right");
    });

    it("keeps a null numeric cell as an em-dash, right-aligned", () => {
      const data: ResultData = {
        columns: [{ name: "amount", friendly_type: "float" }],
        rows: [{ amount: null }],
      };
      const { container } = render(<ResultsTable data={data} />);
      expect(screen.getByText("—")).toBeInTheDocument();
      expect(container.querySelector("td")).toHaveClass("text-right");
    });

    it("shows NaN and Infinity as themselves instead of guessing a value", () => {
      const data: ResultData = {
        columns: [
          { name: "a", friendly_type: "float" },
          { name: "b", friendly_type: "float" },
        ],
        rows: [{ a: NaN, b: Infinity }],
      };
      render(<ResultsTable data={data} />);
      expect(screen.getByText("NaN")).toBeInTheDocument();
      expect(screen.getByText("Infinity")).toBeInTheDocument();
    });

    it("leaves exponential-notation values unformatted rather than mis-parsing them", () => {
      const data: ResultData = {
        columns: [{ name: "tiny", friendly_type: "float" }],
        rows: [{ tiny: "1e-7" }],
      };
      render(<ResultsTable data={data} />);
      expect(screen.getByText("1e-7")).toBeInTheDocument();
    });

    it("leaves non-numeric junk in a numeric column unformatted", () => {
      const data: ResultData = {
        columns: [{ name: "amount", friendly_type: "integer" }],
        rows: [{ amount: "not-a-number" }],
      };
      render(<ResultsTable data={data} />);
      expect(screen.getByText("not-a-number")).toBeInTheDocument();
    });

    it("renders an empty-string numeric cell as blank, not as 0", () => {
      const data: ResultData = {
        columns: [{ name: "amount", friendly_type: "float" }],
        rows: [{ amount: "" }],
      };
      const { container } = render(<ResultsTable data={data} />);
      expect(screen.queryByText("0")).not.toBeInTheDocument();
      expect(container.querySelector("td")?.textContent).toBe("");
    });

    it("keeps a negative-zero string signed instead of dropping it to a plain 0", () => {
      const data: ResultData = {
        columns: [{ name: "delta", friendly_type: "float" }],
        rows: [{ delta: "-0.00" }],
      };
      render(<ResultsTable data={data} />);
      expect(screen.getByText("-0.00")).toBeInTheDocument();
    });

    it("degrades multi-dot junk to unchanged rather than truncating it", () => {
      const data: ResultData = {
        columns: [{ name: "weird", friendly_type: "float" }],
        rows: [{ weird: "12.34.56" }],
      };
      render(<ResultsTable data={data} />);
      expect(screen.getByText("12.34.56")).toBeInTheDocument();
    });
  });

  describe("aria-sort reflects current sort state", () => {
    it("marks the active column ascending/descending and every other sortable column none", async () => {
      const user = userEvent.setup();
      const data: ResultData = {
        columns: [
          { name: "name", friendly_type: "string" },
          { name: "age", friendly_type: "integer" },
        ],
        rows: [
          { name: "b", age: 2 },
          { name: "a", age: 1 },
        ],
      };
      const { container } = render(<ResultsTable data={data} />);
      const ths = () => Array.from(container.querySelectorAll("th"));

      expect(ths().every((th) => th.getAttribute("aria-sort") === "none")).toBe(true);

      await user.click(screen.getByRole("button", { name: /^name/ }));
      expect(ths()[0]).toHaveAttribute("aria-sort", "ascending");
      expect(ths()[1]).toHaveAttribute("aria-sort", "none");

      await user.click(screen.getByRole("button", { name: /^name/ }));
      expect(ths()[0]).toHaveAttribute("aria-sort", "descending");
    });
  });

  describe("header accessible name separates label from type badge", () => {
    it("does not glue the column type onto the accessible name", () => {
      const data: ResultData = {
        columns: [{ name: "revenue", friendly_type: "float" }],
        rows: [{ revenue: 1 }],
      };
      render(<ResultsTable data={data} />);
      // The visible "float" badge must not fuse into the accessible name ("revenuefloat").
      expect(screen.queryByRole("button", { name: "revenuefloat" })).not.toBeInTheDocument();
      expect(screen.getByRole("button", { name: /^revenue\s*,\s*float$/ })).toBeInTheDocument();
    });

    it("hides the visual type badge from the accessibility tree", () => {
      const data: ResultData = {
        columns: [{ name: "revenue", friendly_type: "float" }],
        rows: [{ revenue: 1 }],
      };
      const { container } = render(<ResultsTable data={data} />);
      const badge = Array.from(container.querySelectorAll("span")).find(
        (el) => el.textContent === "float" && el.getAttribute("aria-hidden") === "true",
      );
      expect(badge).toBeTruthy();
    });
  });

  describe("scroll-edge fade indicators", () => {
    class FakeResizeObserver {
      static instances: FakeResizeObserver[] = [];
      callback: ResizeObserverCallback;
      constructor(callback: ResizeObserverCallback) {
        this.callback = callback;
        FakeResizeObserver.instances.push(this);
      }
      observe() {}
      unobserve() {}
      disconnect() {}
    }

    it("recomputes the bottom fade when the scroll container itself is resized, not just on scroll/window resize", () => {
      // jsdom fires no resize on layout change; stub ResizeObserver and fire its callback to
      // simulate the container shrinking without a scroll or window event.
      const originalRO = window.ResizeObserver;
      FakeResizeObserver.instances = [];
      window.ResizeObserver = FakeResizeObserver as unknown as typeof ResizeObserver;
      try {
        const rows = Array.from({ length: 50 }, (_, i) => ({ id: i }));
        const data: ResultData = { columns: [{ name: "id", friendly_type: "integer" }], rows };
        const { container } = render(<ResultsTable data={data} itemsPerPage={50} />);

        expect(FakeResizeObserver.instances).toHaveLength(1);
        expect(container.querySelector(".bg-gradient-to-t")).not.toBeInTheDocument();

        const scrollEl = container.querySelector(".overflow-auto") as HTMLElement;
        Object.defineProperty(scrollEl, "scrollHeight", { value: 500, configurable: true });
        Object.defineProperty(scrollEl, "clientHeight", { value: 200, configurable: true });
        Object.defineProperty(scrollEl, "scrollTop", { value: 0, configurable: true });

        act(() => {
          FakeResizeObserver.instances[0].callback([], FakeResizeObserver.instances[0] as unknown as ResizeObserver);
        });

        expect(container.querySelector(".bg-gradient-to-t")).toBeInTheDocument();
      } finally {
        window.ResizeObserver = originalRO;
      }
    });
  });

  describe("no spacer rows are exposed to the accessibility tree", () => {
    it("exposes exactly one row per data row, no extra padding rows", () => {
      const rows = Array.from({ length: 2 }, (_, i) => ({ n: i }));
      const data: ResultData = {
        columns: [{ name: "n", friendly_type: "integer" }],
        rows,
      };
      render(<ResultsTable data={data} />);
      expect(screen.getAllByRole("row")).toHaveLength(3);
    });
  });
});
