/** The "query" parameter control's options poll steps its interval up while the source runs, then
 * gives up. */
import { describe, it, expect, vi, afterEach } from "vitest";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { ParameterControl, ParametersBar, parseNumberParam } from "../ParametersBar";
import type { QueryParameterDefinition } from "@/types/api";

vi.mock("@/lib/api", () => ({
  api: {
    getParameterValues: vi.fn(),
  },
}));

import { api } from "@/lib/api";

const definition: QueryParameterDefinition = {
  name: "region",
  title: "Region",
  type: "query",
  query_id: 5,
};

function renderControl() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <ParameterControl definition={definition} value={undefined} onChange={vi.fn()} queryId={5} />
    </QueryClientProvider>,
  );
}

const pendingResponse = { job: { id: "j1", status: 1, query_result_id: null, error: null } } as const;

afterEach(() => {
  vi.restoreAllMocks();
});

describe("QueryDropdownControl options poll", () => {
  it("shows 'Loading…' (not an empty select) while the source query result is still pending", async () => {
    vi.mocked(api.getParameterValues).mockResolvedValue(pendingResponse);
    renderControl();

    expect(await screen.findByRole("button", { name: /loading…/i })).toBeDisabled();
    expect(screen.queryByRole("combobox")).not.toBeInTheDocument();
  });

  it("renders the real options once the source query finally resolves", async () => {
    vi.mocked(api.getParameterValues).mockResolvedValueOnce(pendingResponse).mockResolvedValueOnce({
      values: [{ name: "US", value: "us" }],
    });
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      renderControl();
      await screen.findByRole("button", { name: /loading…/i });

      await act(async () => {
        await vi.advanceTimersByTimeAsync(1_000);
      });

      await waitFor(() => expect(screen.queryByRole("button", { name: /loading…/i })).not.toBeInTheDocument());
    } finally {
      vi.useRealTimers();
    }
  });

  it("the poll interval grows (1s → 2s → 5s) and stops entirely once it gives up, rendering 'Options not ready' with a Retry", async () => {
    vi.mocked(api.getParameterValues).mockResolvedValue(pendingResponse);
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      renderControl();
      await screen.findByRole("button", { name: /loading…/i });
      expect(api.getParameterValues).toHaveBeenCalledTimes(1);

      // 1s x3 (attempts 2-4), 2s x3 (5-7), then 5s to the ceiling (attempt 9) and give up.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(3 * 1_000);
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(3 * 2_000);
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(3 * 5_000);
      });

      await waitFor(() => expect(screen.getByText("Options not ready")).toBeInTheDocument());
      const callsAtGiveUp = vi.mocked(api.getParameterValues).mock.calls.length;
      expect(callsAtGiveUp).toBeGreaterThanOrEqual(9);

      await act(async () => {
        await vi.advanceTimersByTimeAsync(20_000);
      });
      expect(api.getParameterValues).toHaveBeenCalledTimes(callsAtGiveUp);

      const retryButton = screen.getByRole("button", { name: /retry/i });
      expect(retryButton).toBeEnabled();
    } finally {
      vi.useRealTimers();
    }
  });

  it("Retry after giving up starts a fresh fetch", async () => {
    vi.mocked(api.getParameterValues).mockResolvedValue(pendingResponse);
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      renderControl();
      await screen.findByRole("button", { name: /loading…/i });

      await act(async () => {
        await vi.advanceTimersByTimeAsync(3 * 1_000 + 3 * 2_000 + 3 * 5_000);
      });
      await waitFor(() => expect(screen.getByText("Options not ready")).toBeInTheDocument());
      const callsAtGiveUp = vi.mocked(api.getParameterValues).mock.calls.length;

      await userEvent.setup({ advanceTimers: vi.advanceTimersByTime }).click(
        screen.getByRole("button", { name: /retry/i }),
      );

      await waitFor(() =>
        expect(vi.mocked(api.getParameterValues).mock.calls.length).toBeGreaterThan(callsAtGiveUp),
      );
    } finally {
      vi.useRealTimers();
    }
  });

  it("shows the load-failure state with a Retry when the fetch itself errors", async () => {
    vi.mocked(api.getParameterValues).mockRejectedValue(new Error("boom"));
    renderControl();

    expect(await screen.findByText("Couldn't load options")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /retry/i })).toBeInTheDocument();
  });
});

function renderParametersBar(onDefinitionsChange: (defs: QueryParameterDefinition[]) => void | Promise<void>) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <ParametersBar
        sql="SELECT * FROM t WHERE x = {{ foo }}"
        definitions={[]}
        values={{}}
        onValuesChange={vi.fn()}
        onDefinitionsChange={onDefinitionsChange}
        queryId={null}
      />
    </QueryClientProvider>,
  );
}

describe("NewParameterConfigurator date-token quoting hint", () => {
  async function pickType(configurator: HTMLElement, label: string) {
    await userEvent.click(within(configurator).getByRole("combobox"));
    const listbox = await screen.findByRole("listbox");
    await userEvent.click(within(listbox).getByText(label));
  }

  it("shows the quoted-token hint for Date, Date Range, and Date/Time Range, not for Text/Number/Dropdown", async () => {
    renderParametersBar(vi.fn());
    const configurator = screen.getByTestId("param-foo");

    for (const label of ["Text", "Number", "Dropdown"]) {
      await pickType(configurator, label);
      expect(within(configurator).queryByText(/substitutes unquoted/i)).not.toBeInTheDocument();
    }

    await pickType(configurator, "Date");
    expect(within(configurator).getByText("'{{ foo }}'::date")).toBeInTheDocument();

    await pickType(configurator, "Date Range");
    expect(
      within(configurator).getByText("BETWEEN '{{ foo.start }}' AND '{{ foo.end }}'"),
    ).toBeInTheDocument();

    await pickType(configurator, "Date/Time Range");
    expect(
      within(configurator).getByText("BETWEEN '{{ foo.start }}' AND '{{ foo.end }}'"),
    ).toBeInTheDocument();
  });
});

describe("parseNumberParam", () => {
  it("does not reach the parameter values as NaN for a half-typed or pasted non-number", () => {
    expect(parseNumberParam("1e")).toBe("");
    expect(parseNumberParam("-")).toBe("");
    expect(parseNumberParam("abc")).toBe("");
  });

  it("passes a real number through untouched", () => {
    expect(parseNumberParam("42")).toBe(42);
    expect(parseNumberParam("-3.5")).toBe(-3.5);
  });

  it("treats an empty string as empty, not zero", () => {
    expect(parseNumberParam("")).toBe("");
  });
});

describe("ParameterControl number type", () => {
  const numberDefinition: QueryParameterDefinition = {
    name: "limit",
    title: "Limit",
    type: "number",
  };

  it("a valid number reaches onChange untouched, with no invalid marker", async () => {
    const onChange = vi.fn();
    render(<ParameterControl definition={numberDefinition} value={undefined} onChange={onChange} queryId={null} />);

    const input = screen.getByRole("spinbutton");
    await userEvent.type(input, "42");

    expect(onChange).toHaveBeenLastCalledWith(42);
    expect(input).toHaveAttribute("aria-invalid", "false");
    expect(screen.queryByText("Enter a number")).not.toBeInTheDocument();
  });

  it("an unparseable raw value reaching the change handler is coerced to empty and flagged invalid, never NaN", () => {
    const onChange = vi.fn();
    render(<ParameterControl definition={numberDefinition} value={undefined} onChange={onChange} queryId={null} />);

    const input = screen.getByRole("spinbutton");
    // jsdom's number setter already rejects "1e"; fire change directly so the handler sees the raw
    // string.
    fireEvent.change(input, { target: { value: "1e" } });

    for (const call of onChange.mock.calls) {
      expect(Number.isNaN(call[0])).toBe(false);
    }
  });
});

describe("NewParameterConfigurator save handling", () => {
  it("a rejected save leaves the configurator mounted, the typed title intact, and shows an error with Save re-enabled", async () => {
    const onDefinitionsChange = vi.fn(() => Promise.reject(new Error("boom")));
    renderParametersBar(onDefinitionsChange);

    const configurator = screen.getByTestId("param-foo");
    const titleInput = within(configurator).getByRole("textbox");
    await userEvent.clear(titleInput);
    await userEvent.type(titleInput, "My Param");

    await userEvent.click(within(configurator).getByRole("button", { name: "Add parameter" }));

    expect(await within(configurator).findByText("Couldn't save this parameter. Try again.")).toBeInTheDocument();
    expect(within(configurator).getByRole("button", { name: "Add parameter" })).toBeEnabled();
    expect(within(configurator).getByRole("textbox")).toHaveValue("My Param");
    expect(screen.getByTestId("param-foo")).toBeInTheDocument();
  });

  it("a resolved save unmounts the configurator", async () => {
    const onDefinitionsChange = vi.fn(() => Promise.resolve());
    renderParametersBar(onDefinitionsChange);

    await userEvent.click(screen.getByRole("button", { name: "Add parameter" }));

    await waitFor(() => expect(screen.queryByTestId("param-foo")).not.toBeInTheDocument());
    expect(onDefinitionsChange).toHaveBeenCalledWith([
      expect.objectContaining({ name: "foo", title: "foo", type: "text" }),
    ]);
  });

  it("disables Save and labels it 'Saving…' while the save is pending", async () => {
    let resolveSave!: () => void;
    const onDefinitionsChange = vi.fn(
      () => new Promise<void>((resolve) => { resolveSave = resolve; }),
    );
    renderParametersBar(onDefinitionsChange);

    await userEvent.click(screen.getByRole("button", { name: "Add parameter" }));

    const savingButton = await screen.findByRole("button", { name: "Saving…" });
    expect(savingButton).toBeDisabled();

    resolveSave();
    await waitFor(() => expect(screen.queryByTestId("param-foo")).not.toBeInTheDocument());
  });
});
