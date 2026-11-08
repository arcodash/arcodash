import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { SnippetMenu } from "../SnippetMenu";
import type * as ApiModule from "@/lib/api";

vi.mock("@/lib/permissions", () => ({
  usePermissions: () => ({ has: () => false }),
}));
vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof ApiModule>();
  return {
    ...actual,
    api: {
      ...actual.api,
      listSnippets: vi.fn().mockResolvedValue([
        { id: 1, trigger: "top10", description: "Top 10 rows", snippet: "SELECT ${1:col} FROM ${2:table} LIMIT 10", user_id: 1, created_at: "", updated_at: "" },
      ]),
    },
  };
});

function renderMenu(onInsert = vi.fn()) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return {
    onInsert,
    ...render(
      <QueryClientProvider client={qc}>
        <SnippetMenu onInsert={onInsert} />
      </QueryClientProvider>,
    ),
  };
}

describe("SnippetMenu", () => {
  beforeEach(() => vi.clearAllMocks());

  it("calls onInsert with both the snippet body and its trigger word", async () => {
    const user = userEvent.setup();
    const { onInsert } = renderMenu();

    await user.click(screen.getByRole("button", { name: /snippets/i }));
    await user.click(await screen.findByText("top10"));

    expect(onInsert).toHaveBeenCalledWith("SELECT ${1:col} FROM ${2:table} LIMIT 10", "top10");
  });
});
