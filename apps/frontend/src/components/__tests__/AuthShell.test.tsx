import type { ReactNode } from "react";
import { describe, it, expect, afterEach } from "vitest";
import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { extensions } from "@/extensions";
import { AuthShell } from "@/components/AuthShell";

afterEach(() => {
  extensions.length = 0;
});

function renderShell(children: ReactNode) {
  return render(<AuthShell>{children}</AuthShell>, { wrapper: MemoryRouter });
}

describe("AuthShell", () => {
  it("renders children directly when no extension supplies authShellSlot", () => {
    renderShell(<div>form content</div>);
    expect(screen.getByText("form content")).toBeInTheDocument();
  });

  it("shows the arcodash brand mark above the card, lowercase with a violet 'arc'", () => {
    renderShell(<div>form content</div>);
    const link = screen.getByRole("link");
    expect(link).toHaveTextContent("arcodash");
    expect(link.querySelector("img")).toHaveAttribute("alt", "");
    expect(link.querySelector("span > span")).toHaveTextContent("arc");
  });

  it("delegates to the extension's authShellSlot when one is present", () => {
    extensions.push({
      navItems: [],
      routes: [],
      authShellSlot: ({ children }) => <div data-testid="ext-shell">{children}</div>,
    });

    renderShell(<div>form content</div>);
    expect(screen.getByTestId("ext-shell")).toBeInTheDocument();
    expect(screen.getByText("form content")).toBeInTheDocument();
  });
});
