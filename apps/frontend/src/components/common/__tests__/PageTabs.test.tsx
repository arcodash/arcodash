import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { PageTabs, type PageTab } from "@/components/common/PageTabs";

const TABS: PageTab[] = [
  { key: "a", label: "Alpha", to: "/a" },
  { key: "b", label: "Beta", to: "/b" },
];

describe("PageTabs", () => {
  it("renders a tab per entry, marking only the active one selected", () => {
    render(<PageTabs tabs={TABS} activeKey="b" />, { wrapper: MemoryRouter });

    const alpha = screen.getByRole("tab", { name: "Alpha" });
    const beta = screen.getByRole("tab", { name: "Beta" });
    expect(alpha).toHaveAttribute("aria-selected", "false");
    expect(beta).toHaveAttribute("aria-selected", "true");
    expect(alpha).toHaveClass("border-transparent");
    expect(beta).toHaveClass("border-primary");
  });

  it("links each tab to its route", () => {
    render(<PageTabs tabs={TABS} activeKey="a" />, { wrapper: MemoryRouter });
    expect(screen.getByRole("tab", { name: "Alpha" })).toHaveAttribute("href", "/a");
    expect(screen.getByRole("tab", { name: "Beta" })).toHaveAttribute("href", "/b");
  });

  it("wires aria-controls/id through for the active tab, whose panel is actually rendered", () => {
    render(
      <PageTabs
        tabs={[{ key: "a", label: "Alpha", to: "/a", id: "tab-a", ariaControls: "panel-a" }]}
        activeKey="a"
      />,
      { wrapper: MemoryRouter },
    );
    const tab = screen.getByRole("tab", { name: "Alpha" });
    expect(tab).toHaveAttribute("id", "tab-a");
    expect(tab).toHaveAttribute("aria-controls", "panel-a");
  });

  it("omits aria-controls on an inactive tab, since only the active tab's panel is in the DOM", () => {
    render(
      <PageTabs
        tabs={[
          { key: "a", label: "Alpha", to: "/a", id: "tab-a", ariaControls: "panel-a" },
          { key: "b", label: "Beta", to: "/b", id: "tab-b", ariaControls: "panel-b" },
        ]}
        activeKey="a"
      />,
      { wrapper: MemoryRouter },
    );
    expect(screen.getByRole("tab", { name: "Beta" })).not.toHaveAttribute("aria-controls");
  });
});
