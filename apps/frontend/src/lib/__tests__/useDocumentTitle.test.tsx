import { describe, it, expect } from "vitest";
import { render } from "@testing-library/react";
import { useDocumentTitle } from "@/lib/useDocumentTitle";

function Titled({ title }: { title: string }) {
  useDocumentTitle(title);
  return null;
}

describe("useDocumentTitle", () => {
  it("sets document.title and updates it when the title prop changes", () => {
    const { rerender } = render(<Titled title="Queries · arcodash" />);
    expect(document.title).toBe("Queries · arcodash");

    rerender(<Titled title="Dashboards · arcodash" />);
    expect(document.title).toBe("Dashboards · arcodash");
  });

  it("restores the previous title on unmount", () => {
    document.title = "outer title";
    const { unmount } = render(<Titled title="inner title" />);
    expect(document.title).toBe("inner title");

    unmount();
    expect(document.title).toBe("outer title");
  });
});
