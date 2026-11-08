import { describe, it, expect, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ActionBar, type ActionItem } from "@/components/common/ActionBar";

function makePrimary(overrides: Partial<ActionItem> = {}): ActionItem {
  return { key: "save", label: "Save", onSelect: vi.fn(), ...overrides };
}

describe("ActionBar", () => {
  it("renders primary buttons and fires onSelect on click", async () => {
    const user = userEvent.setup();
    const onSelect = vi.fn();
    render(<ActionBar primary={[makePrimary({ onSelect })]} />);
    const btn = screen.getByRole("button", { name: "Save" });
    expect(btn).toHaveAttribute("data-action", "save");
    await user.click(btn);
    expect(onSelect).toHaveBeenCalledTimes(1);
  });

  it("shows pending label and disables primary button while pending", () => {
    render(
      <ActionBar
        primary={[
          makePrimary({ pending: true, pendingLabel: "Saving…" }),
        ]}
      />,
    );
    const btn = screen.getByRole("button", { name: /Saving…/ });
    expect(btn).toBeDisabled();
  });

  it("disables primary button when disabled=true", () => {
    render(<ActionBar primary={[makePrimary({ disabled: true })]} />);
    expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
  });

  it("renders the title attribute on a disabled primary button", () => {
    render(
      <ActionBar
        primary={[
          makePrimary({ disabled: true, title: "Cannot run: limited access" }),
        ]}
      />,
    );
    const btn = screen.getByRole("button", { name: "Save" });
    expect(btn).toBeDisabled();
    expect(btn).toHaveAttribute("title", "Cannot run: limited access");
  });

  it("renders wide secondary buttons with data-slot/data-action wiring", () => {
    render(
      <ActionBar
        primary={[makePrimary()]}
        secondary={[{ key: "export", label: "Export", onSelect: vi.fn() }]}
      />,
    );
    const wideButtons = screen.getAllByText("Export");
    const wideBtn = wideButtons
      .map((el) => el.closest("button"))
      .find((btn) => btn?.getAttribute("data-action") === "export");
    expect(wideBtn).toBeTruthy();
    expect(wideBtn).toHaveAttribute("data-slot", "button");
  });

  it('renders "More actions" trigger only when secondaries or menuExtra exist', () => {
    const { rerender } = render(<ActionBar primary={[makePrimary()]} />);
    expect(
      screen.queryByRole("button", { name: "More actions" }),
    ).not.toBeInTheDocument();

    rerender(
      <ActionBar
        primary={[makePrimary()]}
        secondary={[{ key: "export", label: "Export", onSelect: vi.fn() }]}
      />,
    );
    expect(
      screen.getByRole("button", { name: "More actions" }),
    ).toBeInTheDocument();
  });

  it("shows the trigger when only menuExtra is provided (no secondaries)", () => {
    render(
      <ActionBar primary={[makePrimary()]} menuExtra={<div>Extra</div>} />,
    );
    expect(
      screen.getByRole("button", { name: "More actions" }),
    ).toBeInTheDocument();
  });

  it("opens the overflow menu and shows menu items in menuOrder, with group separators and destructive styling, firing onSelect", async () => {
    const user = userEvent.setup();
    const onSelectB = vi.fn();
    const secondary: ActionItem[] = [
      { key: "a", label: "Alpha", onSelect: vi.fn(), group: 1 },
      { key: "b", label: "Bravo", onSelect: onSelectB, group: 1 },
      {
        key: "c",
        label: "Charlie",
        onSelect: vi.fn(),
        group: 2,
        destructive: true,
      },
    ];
    render(
      <ActionBar
        primary={[makePrimary()]}
        secondary={secondary}
        menuOrder={["b", "a", "c"]}
      />,
    );

    await user.click(screen.getByRole("button", { name: "More actions" }));
    const menu = screen.getByRole("menu");
    const items = within(menu).getAllByRole("menuitem");
    expect(items.map((el) => el.textContent)).toEqual([
      "Bravo",
      "Alpha",
      "Charlie",
    ]);

    // separator between group 1 (Bravo/Alpha) and group 2 (Charlie)
    const separators = menu.querySelectorAll('[data-slot="dropdown-menu-separator"]');
    expect(separators.length).toBe(1);

    const charlie = within(menu).getByText("Charlie").closest('[role="menuitem"]');
    expect(charlie).toHaveClass("text-destructive");

    await user.click(within(menu).getByText("Bravo"));
    expect(onSelectB).toHaveBeenCalledTimes(1);
  });

  it("disables a pending menuitem", async () => {
    const user = userEvent.setup();
    render(
      <ActionBar
        primary={[makePrimary()]}
        secondary={[
          {
            key: "export",
            label: "Export",
            pending: true,
            pendingLabel: "Exporting…",
            onSelect: vi.fn(),
          },
        ]}
      />,
    );
    await user.click(screen.getByRole("button", { name: "More actions" }));
    const menu = screen.getByRole("menu");
    const item = within(menu).getByText("Exporting…").closest('[role="menuitem"]');
    expect(item).toHaveAttribute("data-disabled");
  });

  it("gives the iconOnly wide button an aria-label", () => {
    render(
      <ActionBar
        primary={[makePrimary()]}
        secondary={[
          { key: "export", label: "Export", iconOnly: true, onSelect: vi.fn() },
        ]}
      />,
    );
    expect(
      screen.getByRole("button", { name: "Export" }),
    ).toHaveAttribute("aria-label", "Export");
  });

  it("renders menuExtra content after a separator", async () => {
    const user = userEvent.setup();
    render(
      <ActionBar
        primary={[makePrimary()]}
        secondary={[{ key: "export", label: "Export", onSelect: vi.fn() }]}
        menuExtra={<div data-testid="menu-extra">Extra content</div>}
      />,
    );
    await user.click(screen.getByRole("button", { name: "More actions" }));
    expect(screen.getByTestId("menu-extra")).toBeInTheDocument();
  });
});
