import type { LucideIcon } from "lucide-react";
import { LayoutGrid, SlidersHorizontal, Table2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Loader } from "@/components/ui/loader";
import type { ListView } from "@/lib/useListView";
import type { ScopeFilter } from "@/types/api";

export interface ChipGroupFilter {
  key: string;
  groupLabel: string;
  value: string;
  onChange: (value: string) => void;
  options: { value: string; label: string }[];
}

export interface ToggleFilterDef {
  key: string;
  label: string;
  icon?: LucideIcon;
  active: boolean;
  onToggle: () => void;
}

/** The "Owner" chip group shared by every list page. */
export function ownerChipGroup(value: ScopeFilter, onChange: (value: ScopeFilter) => void): ChipGroupFilter {
  return {
    key: "scope",
    groupLabel: "Owner",
    value,
    onChange: (v) => onChange(v as ScopeFilter),
    options: [
      { value: "all", label: "All owners" },
      { value: "by-me", label: "By me" },
      { value: "shared-with-me", label: "Shared with me" },
    ],
  };
}

interface ListToolbarProps {
  search: string;
  onSearchChange: (value: string) => void;
  searchPlaceholder: string;
  chipGroups?: ChipGroupFilter[];
  toggles?: ToggleFilterDef[];
  activeFilterCount?: number;
  isFetching?: boolean;
  view: ListView;
  onViewChange: (view: ListView) => void;
  /** False on mobile, where the view is forced to cards. */
  showViewToggle: boolean;
}

const VIEW_BUTTONS: { value: ListView; label: string; Icon: LucideIcon }[] = [
  { value: "table", label: "Table view", Icon: Table2 },
  { value: "cards", label: "Card view", Icon: LayoutGrid },
];

function ChipButtons({ group }: { group: ChipGroupFilter }) {
  return (
    <>
      {group.options.map((o) => (
        <Button
          key={o.value}
          variant={group.value === o.value ? "default" : "outline"}
          aria-pressed={group.value === o.value}
          size="sm"
          onClick={() => group.onChange(o.value)}
        >
          {o.label}
        </Button>
      ))}
    </>
  );
}

function ToggleButton({ toggle }: { toggle: ToggleFilterDef }) {
  const Icon = toggle.icon;
  return (
    <Button
      variant={toggle.active ? "default" : "outline"}
      aria-pressed={toggle.active}
      size="sm"
      onClick={toggle.onToggle}
      className="flex items-center gap-1"
    >
      {Icon && <Icon className={`h-3.5 w-3.5 ${toggle.active ? "fill-current" : ""}`} />}
      {toggle.label}
    </Button>
  );
}

/** Shared toolbar for the Queries/Dashboards/Monitors lists. At `md`+ filter groups render inline;
 * below, they collapse into one "Filter" popover instead of wrapping into a clipped row. */
export function ListToolbar({
  search,
  onSearchChange,
  searchPlaceholder,
  chipGroups = [],
  toggles = [],
  activeFilterCount = 0,
  isFetching,
  view,
  onViewChange,
  showViewToggle,
}: ListToolbarProps) {
  const hasFilters = chipGroups.length > 0 || toggles.length > 0;

  return (
    <div className="flex flex-wrap items-center gap-2">
      <Input
        placeholder={searchPlaceholder}
        value={search}
        onChange={(e) => onSearchChange(e.target.value)}
        className="max-w-md"
      />

      <div className="hidden items-center gap-2 md:flex md:flex-wrap">
        {chipGroups.map((g, i) => (
          <div key={g.key} className={`flex items-center gap-2 ${i > 0 ? "ml-2" : ""}`}>
            <ChipButtons group={g} />
          </div>
        ))}
        {toggles.map((t) => (
          <ToggleButton key={t.key} toggle={t} />
        ))}
      </div>

      {hasFilters && (
        <Popover>
          <PopoverTrigger asChild>
            <Button
              variant={activeFilterCount > 0 ? "default" : "outline"}
              size="sm"
              className="flex items-center gap-1.5 md:hidden"
            >
              <SlidersHorizontal className="h-3.5 w-3.5" />
              Filter
              {activeFilterCount > 0 && (
                <span className="flex h-4 min-w-4 items-center justify-center rounded-full bg-background px-1 text-[10px] text-foreground">
                  {activeFilterCount}
                </span>
              )}
            </Button>
          </PopoverTrigger>
          <PopoverContent align="start" className="w-64 space-y-4">
            {chipGroups.map((g) => (
              <div key={g.key} className="space-y-1.5">
                <p className="text-xs font-medium text-muted-foreground">{g.groupLabel}</p>
                <div className="flex flex-wrap gap-1.5">
                  <ChipButtons group={g} />
                </div>
              </div>
            ))}
            {toggles.length > 0 && (
              <div className="space-y-1.5">
                {toggles.map((t) => (
                  <ToggleButton key={t.key} toggle={t} />
                ))}
              </div>
            )}
          </PopoverContent>
        </Popover>
      )}

      {isFetching && <Loader size={16} announce />}

      {showViewToggle && (
        <div className="ml-auto hidden items-center gap-0.5 rounded-md border border-border p-0.5 md:flex">
          {VIEW_BUTTONS.map(({ value, label, Icon }) => (
            <Button
              key={value}
              variant={view === value ? "secondary" : "ghost"}
              size="icon-sm"
              aria-pressed={view === value}
              aria-label={label}
              title={label}
              onClick={() => onViewChange(value)}
            >
              <Icon className="h-3.5 w-3.5" />
            </Button>
          ))}
        </div>
      )}
    </div>
  );
}

/** Single-select tag filter; clicking the active tag clears it. */
export function TagFilterPills({
  tags,
  active,
  onChange,
}: {
  tags: string[];
  active: string | null;
  onChange: (tag: string | null) => void;
}) {
  if (tags.length === 0) return null;
  return (
    <div className="flex flex-wrap gap-1.5">
      {tags.map((tag) => (
        <button
          key={tag}
          type="button"
          onClick={() => onChange(active === tag ? null : tag)}
          className={`cursor-pointer rounded-full px-2.5 py-0.5 text-xs transition ${
            active === tag ? "bg-foreground text-background" : "bg-muted text-muted-foreground hover:bg-muted/70"
          }`}
        >
          {tag}
        </button>
      ))}
      {active && (
        <button
          type="button"
          onClick={() => onChange(null)}
          className="cursor-pointer text-xs text-muted-foreground hover:text-foreground"
        >
          clear
        </button>
      )}
    </div>
  );
}
