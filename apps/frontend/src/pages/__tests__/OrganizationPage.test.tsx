import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "@/__tests__/msw-server";
import OrganizationPage from "@/pages/OrganizationPage";

const BASE = "http://localhost:8080";

function renderPage() {
  server.use(
    http.get(`${BASE}/api/organization`, () =>
      HttpResponse.json({ id: 1, name: "Lumen Coffee", slug: "default" }),
    ),
    http.get(`${BASE}/api/orgs/me/embed_settings`, () =>
      HttpResponse.json({ public_urls_enabled: true }),
    ),
  );
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const view = render(
    <QueryClientProvider client={qc}>
      <OrganizationPage />
    </QueryClientProvider>,
  );
  return { ...view, qc };
}

describe("OrganizationPage", () => {
  it("sets the document title", async () => {
    renderPage();
    await screen.findByText("default");
    expect(document.title).toBe("Organization · arcodash");
  });

  it("renders the slug as a full-opacity read-only field, not a disabled input", async () => {
    renderPage();

    const value = await screen.findByText("default");
    expect(value.tagName).toBe("P");
    expect(value).toHaveClass("text-foreground");
    expect(value).not.toHaveClass("opacity-50");
    expect(screen.getByText("Used in URLs. Fixed once the organization is created.")).toBeInTheDocument();
    expect(screen.queryByDisplayValue("default")).not.toBeInTheDocument();
  });

  it("shows the server's own message when saving the org name fails", async () => {
    renderPage();
    server.use(
      http.post(`${BASE}/api/organization`, () =>
        HttpResponse.json({ message: "name already taken" }, { status: 400 }),
      ),
    );

    const nameInput = await screen.findByLabelText("Organization Name");
    await userEvent.type(nameInput, " Renamed");
    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    expect(await screen.findByText("name already taken")).toBeInTheDocument();
  });

  it("shows an inline error when toggling Public URLs fails, instead of silently snapping back", async () => {
    renderPage();
    server.use(
      http.post(`${BASE}/api/orgs/me/embed_settings`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );

    const toggle = await screen.findByLabelText("Public URLs");
    await userEvent.click(toggle);

    expect(await screen.findByText("database unavailable")).toBeInTheDocument();
  });

  it("does not clobber a typed-but-unsaved name when the organization query refetches with the same name", async () => {
    const { qc } = renderPage();

    const nameInput = await screen.findByLabelText("Organization Name");
    await userEvent.clear(nameInput);
    await userEvent.type(nameInput, "Not Yet Saved");
    expect(nameInput).toHaveValue("Not Yet Saved");

    // Unrelated refetch returns a freshly-parsed object with the same name — object identity
    // changed but the value didn't, so the typed edit must survive.
    await qc.refetchQueries({ queryKey: ["organization"] });

    expect(nameInput).toHaveValue("Not Yet Saved");
  });
});
