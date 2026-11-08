import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { DocsLink } from "@/components/common/DocsLink";
import { DOCS } from "@/lib/docs";

describe("DocsLink", () => {
  it("shows the exact 'Learn more' label, carries the topic only in an sr-only span, and opens in a new tab", () => {
    render(<DocsLink topic="monitorEvaluation" label="how a check is evaluated" />);

    const link = screen.getByRole("link", { name: /learn more about how a check is evaluated/i });
    expect(link).toHaveAttribute("target", "_blank");
    expect(link).toHaveAttribute("rel", "noreferrer");
    expect(link).toHaveAttribute("href", `https://www.arcodash.com/docs${DOCS.monitorEvaluation}`);

    // Visible text is just "Learn more" — the topic lives in an sr-only span, not on the page.
    const srSpan = link.querySelector("span.sr-only");
    expect(srSpan?.textContent).toBe(" about how a check is evaluated (opens in a new tab)");
    expect(link.textContent).toBe("Learn more about how a check is evaluated (opens in a new tab)");
  });

  it("resolves each topic to the documented base + path", () => {
    render(<DocsLink topic="dataSourceHealth" label="connection health" />);
    expect(screen.getByRole("link")).toHaveAttribute(
      "href",
      "https://www.arcodash.com/docs/connecting-data/overview#connection-health",
    );
  });
});
