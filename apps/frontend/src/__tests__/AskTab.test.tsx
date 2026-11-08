import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createMemoryRouter, RouterProvider } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { AuthProvider } from "@/lib/auth";
import { PERMISSION_KEYS } from "@/lib/permissions";
import { api } from "@/lib/api";
import type { AskEvent } from "@/types/api";
import { QueryEditorPage } from "@/pages/QueryEditorPage";
import { AskTimeline } from "@/components/ai/AskTimeline";
import { IDLE_RUN, trimError, type AskRunState } from "@/components/ai/askRun";

vi.mock("@/components/query-editor/SqlEditor", () => ({
  SqlEditor: ({ value }: { value: string }) => <textarea data-testid="sql-editor" value={value} readOnly />,
}));
vi.mock("@/components/query-editor/SchemaBrowser", () => ({ SchemaBrowser: () => null }));
vi.mock("@/components/data-sources/SchemaBrowser", () => ({ SchemaBrowser: () => null }));
vi.mock("@/components/query-editor/SnippetMenu", () => ({ SnippetMenu: () => null }));
vi.mock("@uiw/react-codemirror", () => ({
  default: ({ value }: { value: string }) => <pre data-testid="sql-preview">{value}</pre>,
}));
vi.mock("@/lib/jobs", () => ({
  usePollJob: () => ({
    isSuccess: false, isDone: false, isPending: false, isStarted: false, isFailure: false,
    isCancelled: false, isPollError: false, error: null, queryResultId: null,
  }),
}));

const BASE = "http://localhost:8080";
const DS = { id: 1, name: "Postgres", type: "pg", paused: false, pause_reason: null, schema_refreshed_at: null, created_at: "", updated_at: "" };

function session(isAdmin: boolean) {
  server.use(
    http.get(`${BASE}/api/session`, () =>
      HttpResponse.json({
        user: {
          id: 1, name: "T", email: "t@example.com", groups: [1], org_id: 1,
          is_invitation_pending: false, disabled_at: null, profile_image_url: null, details: {},
          created_at: "2025-01-01T00:00:00Z", updated_at: "2025-01-01T00:00:00Z",
        },
        org: { id: 1, name: "O", slug: "o", settings: {} },
        permissions: isAdmin ? [...PERMISSION_KEYS] : PERMISSION_KEYS.filter((k) => k !== "admin"),
        is_admin: isAdmin,
        client_config: { disable_public_urls: false, saml_enabled: false, ldap_enabled: false, ai_sql_generation: true },
      }),
    ),
  );
}

type Driver = (emit: (e: AskEvent) => void, signal: AbortSignal) => Promise<void>;

async function openAsk(isAdmin: boolean, driver: Driver, sources: (typeof DS)[] = [DS]) {
  session(isAdmin);
  server.use(
    http.get(`${BASE}/api/data_sources`, () => HttpResponse.json({ data_sources: sources, can_create: true })),
    http.get(`${BASE}/api/data_sources/1/schema`, () => HttpResponse.json({ schema: [] })),
    http.get(`${BASE}/api/query_snippets`, () => HttpResponse.json([])),
  );
  const spy = vi.spyOn(api, "askStream").mockImplementation((_b, onEvent, signal) =>
    driver(onEvent, signal as AbortSignal),
  );
  const router = createMemoryRouter([{ path: "/queries/:id", element: <QueryEditorPage /> }], {
    initialEntries: ["/queries/new"],
  });
  render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <AuthProvider>
        <RouterProvider router={router} />
      </AuthProvider>
    </QueryClientProvider>,
  );
  const user = userEvent.setup();
  await user.click(await screen.findByRole("button", { name: /Ask with AI/ }));
  await user.type(await screen.findByLabelText("Ask AI a question about your data"), "top users");
  return { user, spy };
}

const generate = (user: ReturnType<typeof userEvent.setup>) =>
  user.click(screen.getByRole("button", { name: "Generate SQL" }));

const PREVIEW: AskEvent = {
  type: "preview", attempt: 1, columns: [{ name: "id", friendly_type: "integer" }],
  rows: [{ id: 1 }, { id: 2 }], row_count: 2, truncated: false,
};
const final = (over: Partial<Extract<AskEvent, { type: "final" }>> = {}): AskEvent => ({
  type: "final", sql: "SELECT id FROM t", explanation: "Lists ids", served_by: "Gemini",
  attempts: 1, tokens_in: 1, tokens_out: 1, executed: true, error: null, ...over,
});

describe("Ask with AI tab (stream)", () => {
  beforeEach(() => vi.restoreAllMocks());
  afterEach(() => vi.restoreAllMocks());

  it("happy path collapses to the summary and offers Apply/Regenerate/Discard", async () => {
    const { user } = await openAsk(true, async (emit) => {
      emit({ type: "status", text: "Reading schema" });
      emit({ type: "draft", attempt: 1, sql: "SELECT id FROM t" });
      emit({ type: "executing", attempt: 1 });
      emit(PREVIEW);
      emit(final());
    });
    await generate(user);
    const summary = await screen.findByRole("button", { name: /Solved in 1 attempt · 2 rows · Gemini/ });
    expect(summary).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByText("Reading schema")).not.toBeInTheDocument();
    expect(screen.getByTestId("sql-preview")).toHaveTextContent("SELECT id FROM t");
    expect(within(screen.getByTestId("ask-preview-grid")).getByText("id")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Apply" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "Regenerate" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Discard" })).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    await user.click(summary);
    expect(screen.getByText("Reading schema")).toBeInTheDocument();
    expect(screen.getByText("Preview: 2 rows")).toBeInTheDocument();
  });

  it("repair path shows the error stage and the truncated preview label", async () => {
    const { user } = await openAsk(true, async (emit) => {
      emit({ type: "draft", attempt: 1, sql: "SELECT bad" });
      emit({ type: "repair", attempt: 1, error: "column bad does not exist\nLINE 1" });
      emit({ type: "draft", attempt: 2, sql: "SELECT id FROM t" });
      emit({ ...PREVIEW, rows: [{ id: 1 }], row_count: 20, truncated: true });
      emit(final({ attempts: 2 }));
    });
    await generate(user);
    await user.click(await screen.findByRole("button", { name: /Solved in 2 attempts · 1 of 20\+ rows/ }));
    expect(screen.getByText("column bad does not exist")).toBeInTheDocument();
    expect(screen.getByText(/→ repairing/)).toBeInTheDocument();
    expect(screen.getByText("Preview: 1 of 20+ rows")).toBeInTheDocument();
    expect(screen.getByTestId("sql-preview")).toHaveTextContent("SELECT id FROM t");
  });

  it("failed stays expanded with the mapped message and no Apply", async () => {
    const { user } = await openAsk(true, async (emit) => {
      emit({ type: "status", text: "Reading schema" });
      emit({ type: "failed", status: "rate_limited", message: "Slow down", retry_after_secs: 7 });
    });
    await generate(user);
    expect(await screen.findByText("Slow down (try again in 7s)")).toBeInTheDocument();
    expect(screen.getByText("Reading schema")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Apply" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Discard" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Generate SQL" })).toBeInTheDocument();
  });

  it("Cancel aborts the stream and applies nothing", async () => {
    let aborted = false;
    const { user } = await openAsk(true, (emit, signal) => {
      emit({ type: "draft", attempt: 1, sql: "SELECT 1" });
      return new Promise<void>((_res, rej) =>
        signal.addEventListener("abort", () => {
          aborted = true;
          rej(new DOMException("aborted", "AbortError"));
        }),
      );
    });
    await generate(user);
    expect(screen.queryByRole("button", { name: "Discard" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Generate SQL" })).not.toBeInTheDocument();
    await user.click(await screen.findByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(aborted).toBe(true));
    expect(screen.getByText("Cancelled", { selector: "li" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Apply" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Generate SQL" })).toBeEnabled();
  });

  it("a stream that ends without final or failed becomes a failed state", async () => {
    const { user } = await openAsk(true, async (emit) => {
      emit({ type: "draft", attempt: 1, sql: "SELECT 1" });
    });
    await generate(user);
    expect(await screen.findByText(/The AI stream ended unexpectedly/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Apply" })).not.toBeInTheDocument();
  });

  it("events emitted after the run is aborted are ignored", async () => {
    let lateEmit: ((e: AskEvent) => void) | null = null;
    const { user } = await openAsk(true, (emit, signal) => {
      lateEmit = emit;
      emit({ type: "draft", attempt: 1, sql: "SELECT 1" });
      return new Promise<void>((_res, rej) =>
        signal.addEventListener("abort", () => rej(new DOMException("aborted", "AbortError"))),
      );
    });
    await generate(user);
    await user.click(await screen.findByRole("button", { name: "Cancel" }));
    await screen.findByText("Cancelled", { selector: "li" });
    act(() => lateEmit!(final()));
    expect(screen.getByText("Cancelled", { selector: "li" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Apply" })).not.toBeInTheDocument();
  });

  it("changing the data source clears the Ask run", async () => {
    const DS2 = { ...DS, id: 2, name: "Other", type_display_name: "PostgreSQL" };
    const { user } = await openAsk(
      true,
      async (emit) => {
        emit({ type: "draft", attempt: 1, sql: "SELECT id FROM t" });
        emit(final());
      },
      [{ ...DS, type_display_name: "PostgreSQL" }, DS2],
    );
    await generate(user);
    await screen.findByRole("button", { name: "Apply" });
    await user.click(screen.getByTestId("query-editor-data-source-select"));
    await user.click(await screen.findByRole("option", { name: /Other/ }));
    await waitFor(() => expect(screen.queryByRole("button", { name: "Apply" })).not.toBeInTheDocument());
    expect(screen.getByRole("button", { name: "Generate SQL" })).toBeInTheDocument();
  });

  it("exhausted attempts show the amber warning and keep Apply enabled", async () => {
    const { user } = await openAsk(true, async (emit) => {
      emit({ type: "draft", attempt: 3, sql: "SELECT nope" });
      emit({ type: "repair", attempt: 3, error: "relation nope does not exist" });
      emit(final({ sql: "SELECT nope", attempts: 3, executed: false }));
    });
    await generate(user);
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent(
      "This SQL didn't run successfully (last error: relation nope does not exist). Apply anyway to edit it by hand.",
    );
    expect(screen.getByRole("button", { name: "Apply" })).toBeEnabled();
    expect(screen.getByRole("button", { name: /Didn't run successfully after 3 attempts/ })).toBeInTheDocument();
  });

  it("a zero-row preview is a success: summary, empty-state grid, no warning", async () => {
    const { user } = await openAsk(true, async (emit) => {
      emit({ type: "draft", attempt: 1, sql: "SELECT id FROM t" });
      emit({ type: "preview", attempt: 1, columns: [{ name: "id", friendly_type: "number" }], rows: [], row_count: 0, truncated: false });
      emit(final());
    });
    await generate(user);
    expect(await screen.findByRole("button", { name: /Solved in 1 attempt · 0 rows · Gemini/ })).toBeInTheDocument();
    expect(screen.getByText("Query ran successfully — no rows")).toBeVisible();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Apply" })).toBeEnabled();
  });

  it("the warning shows final.error, never the model explanation", async () => {
    const { user } = await openAsk(true, async (emit) => {
      emit({ type: "draft", attempt: 1, sql: "SELECT x FROM t" });
      emit(final({ executed: false, error: "col x missing", explanation: "Model says hi" }));
    });
    await generate(user);
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("col x missing");
    expect(alert).not.toHaveTextContent("Model says hi");
  });

  it("a legacy final without executed falls back to stream inference", async () => {
    const { user } = await openAsk(true, async (emit) => {
      emit({ type: "draft", attempt: 1, sql: "SELECT nope" });
      emit({ type: "repair", attempt: 1, error: "relation nope does not exist" });
      const { executed: _e, error: _r, ...legacy } = final() as Extract<AskEvent, { type: "final" }>;
      emit(legacy);
    });
    await generate(user);
    expect(await screen.findByRole("button", { name: /Didn't run successfully/ })).toBeInTheDocument();
    expect(screen.getByRole("alert")).toHaveTextContent("relation nope does not exist");
  });

  it("links AI Settings only for admins", async () => {
    await openAsk(true, async () => {});
    expect(screen.getByRole("link", { name: "AI Settings" })).toHaveAttribute("href", "/organization/ai");
    expect(screen.getByText(/never your query results/)).toBeInTheDocument();
  });

  it("renders AI Settings as plain text for non-admins", async () => {
    await openAsk(false, async () => {});
    expect(screen.getByText(/never your query results/)).toBeInTheDocument();
    expect(screen.queryByRole("link", { name: "AI Settings" })).not.toBeInTheDocument();
  });
});

describe("AskTimeline", () => {
  it("is empty when idle and offers Cancel only while running", async () => {
    const onCancel = vi.fn();
    const { rerender, container } = render(<AskTimeline state={IDLE_RUN} onCancel={onCancel} />);
    expect(container).toBeEmptyDOMElement();
    const running: AskRunState = { ...IDLE_RUN, phase: "running", steps: [{ kind: "status", text: "Reading schema" }] };
    rerender(<AskTimeline state={running} onCancel={onCancel} />);
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onCancel).toHaveBeenCalled();
    rerender(<AskTimeline state={{ ...running, phase: "failed", error: "x" }} onCancel={onCancel} />);
    expect(screen.queryByRole("button", { name: "Cancel" })).not.toBeInTheDocument();
  });

  it("Draft entries expand inline and long errors toggle to the full text", async () => {
    const long = `${"e".repeat(250)}\nsecond line`;
    const state: AskRunState = {
      ...IDLE_RUN,
      phase: "failed",
      error: "boom",
      steps: [
        { kind: "draft", attempt: 1, sql: "SELECT old" },
        { kind: "repair", attempt: 1, error: long },
      ],
    };
    render(<AskTimeline state={state} onCancel={() => {}} />);
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "Draft 1" }));
    expect(screen.getByText("SELECT old")).toBeInTheDocument();
    expect(screen.queryByText("second line", { exact: false })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Show full error" }));
    expect(screen.getByText(/second line/)).toBeInTheDocument();
  });

  it("trimError keeps the first line and caps at 200 chars", () => {
    expect(trimError("a\nb")).toEqual({ short: "a", truncated: true });
    expect(trimError("a")).toEqual({ short: "a", truncated: false });
    expect(trimError("x".repeat(300)).short).toHaveLength(201);
  });
});

