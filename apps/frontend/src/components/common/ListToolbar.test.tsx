import { describe, it, expect, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Star } from "lucide-react";
import { ListToolbar, type ChipGroupFilter, type ToggleFilterDef } from "./ListToolbar";

function chipGroups(overrides: Partial<ChipGroupFilter> = {}): ChipGroupFilter[] {
  return [
    {
      key: "scope",
      groupLabel: "Owner",
      value: "all",
      onChange: vi.fn(),
      options: [
        { value: "all", label: "All owners" },
        { value: "by-me", label: "By me" },
      ],
      ...overrides,
    },
  ];
}

function baseProps() {
  return {
    search: "",
    onSearchChange: vi.fn(),
    searchPlaceholder: "Search things…",
    view: "table" as const,
    onViewChange: vi.fn(),
    showViewToggle: true,
  };
}

describe("ListToolbar search", () => {
  it("renders the search input with the given placeholder and reports changes", async () => {
    const onSearchChange = vi.fn();
    render(<ListToolbar {...baseProps()} onSearchChange={onSearchChange} />);

    const input = screen.getByPlaceholderText("Search things…");
    await userEvent.type(input, "x");
    expect(onSearchChange).toHaveBeenCalledWith("x");
  });
});

describe("ListToolbar chip filters", () => {
  it("renders each chip group's options and calls onChange with the clicked value", async () => {
    const onChange = vi.fn();
    render(<ListToolbar {...baseProps()} chipGroups={chipGroups({ onChange })} />);

    // Two copies render (desktop row + mobile popover content), so scope to one visible button.
    const buttons = screen.getAllByRole("button", { name: "By me" });
    await userEvent.click(buttons[0]);
    expect(onChange).toHaveBeenCalledWith("by-me");
  });

  it("marks the active chip pressed", () => {
    render(<ListToolbar {...baseProps()} chipGroups={chipGroups({ value: "by-me" })} />);
    const active = screen.getAllByRole("button", { name: "By me" })[0];
    expect(active).toHaveAttribute("aria-pressed", "true");
  });
});

describe("ListToolbar toggle filters", () => {
  it("renders a toggle button reflecting active state and fires onToggle", async () => {
    const onToggle = vi.fn();
    const toggles: ToggleFilterDef[] = [
      { key: "favorites", label: "Favorites", icon: Star, active: false, onToggle },
    ];
    render(<ListToolbar {...baseProps()} toggles={toggles} />);

    const btn = screen.getAllByRole("button", { name: "Favorites" })[0];
    expect(btn).toHaveAttribute("aria-pressed", "false");
    await userEvent.click(btn);
    expect(onToggle).toHaveBeenCalled();
  });
});

describe("ListToolbar mobile filter collapse", () => {
  it("collapses filter groups into a single Filter popover control, with a count badge", async () => {
    render(
      <ListToolbar
        {...baseProps()}
        chipGroups={chipGroups({ value: "by-me" })}
        activeFilterCount={1}
      />,
    );

    const filterBtn = screen.getByRole("button", { name: /filter/i });
    expect(filterBtn).toHaveClass("md:hidden");
    expect(within(filterBtn).getByText("1")).toBeInTheDocument();

    await userEvent.click(filterBtn);
    const popover = await screen.findByText("Owner");
    expect(popover).toBeInTheDocument();
  });

  it("renders no Filter control when there are no filters to collapse", () => {
    render(<ListToolbar {...baseProps()} />);
    expect(screen.queryByRole("button", { name: /filter/i })).not.toBeInTheDocument();
  });
});

describe("ListToolbar view toggle", () => {
  it("shows Table/Card view buttons reflecting the current view when showViewToggle is true", () => {
    render(<ListToolbar {...baseProps()} view="cards" />);
    expect(screen.getByRole("button", { name: "Table view" })).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByRole("button", { name: "Card view" })).toHaveAttribute("aria-pressed", "true");
  });

  it("hides the view toggle entirely when showViewToggle is false (forced mobile view)", () => {
    render(<ListToolbar {...baseProps()} showViewToggle={false} />);
    expect(screen.queryByRole("button", { name: "Table view" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Card view" })).not.toBeInTheDocument();
  });

  it("calls onViewChange when a view button is clicked", async () => {
    const onViewChange = vi.fn();
    render(<ListToolbar {...baseProps()} onViewChange={onViewChange} />);
    await userEvent.click(screen.getByRole("button", { name: "Card view" }));
    expect(onViewChange).toHaveBeenCalledWith("cards");
  });
});
