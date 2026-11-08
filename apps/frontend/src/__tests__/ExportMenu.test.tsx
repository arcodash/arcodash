import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { ExportMenu } from "@/components/query-editor/ExportMenu";

const BASE = "http://localhost:8080";

describe("ExportMenu", () => {
  beforeEach(() => {
    // jsdom doesn't implement the Blob URL APIs the download path uses.
    URL.createObjectURL = vi.fn(() => "blob:mock-url");
    URL.revokeObjectURL = vi.fn();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("a successful export fetches the result, hands the browser a blob, and never navigates", async () => {
    const assignSpy = vi.fn();
    vi.stubGlobal("location", { ...window.location, assign: assignSpy });
    // jsdom attempts a real navigation on an anchor click; the assertions only need the element
    // that was clicked, not an actual page load.
    vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
    const createElementSpy = vi.spyOn(document, "createElement");
    server.use(
      http.get(`${BASE}/api/query_results/7.csv`, () => new HttpResponse("a,b\n1,2")),
    );

    render(<ExportMenu resultId={7} />);
    await userEvent.click(screen.getByRole("button", { name: /export/i }));
    await userEvent.click(await screen.findByRole("button", { name: /download csv/i }));

    await waitFor(() => expect(HTMLAnchorElement.prototype.click).toHaveBeenCalledTimes(1));
    const anchor = createElementSpy.mock.results.find(
      (r) => r.value instanceof HTMLAnchorElement,
    )?.value as HTMLAnchorElement;
    expect(anchor.download).toBe("query-result-7.csv");
    expect(anchor.href).toBe("blob:mock-url");
    expect(URL.createObjectURL).toHaveBeenCalledTimes(1);
    expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:mock-url");
    expect(assignSpy).not.toHaveBeenCalled();

    vi.unstubAllGlobals();
  });

  it("a failed export shows an inline error and stays on the page instead of navigating to the raw error body", async () => {
    const assignSpy = vi.fn();
    vi.stubGlobal("location", { ...window.location, assign: assignSpy });
    server.use(
      http.get(`${BASE}/api/query_results/7.csv`, () =>
        HttpResponse.json({ message: "Result has expired" }, { status: 410 }),
      ),
    );

    render(<ExportMenu resultId={7} />);
    await userEvent.click(screen.getByRole("button", { name: /export/i }));
    await userEvent.click(await screen.findByRole("button", { name: /download csv/i }));

    expect(await screen.findByText("Result has expired")).toBeInTheDocument();
    // The button and popover are still there — a real navigation would have torn down the tree.
    expect(screen.getByRole("button", { name: /export/i })).toBeInTheDocument();
    expect(assignSpy).not.toHaveBeenCalled();

    vi.unstubAllGlobals();
  });
});
