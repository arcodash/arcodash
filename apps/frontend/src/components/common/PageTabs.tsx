import { Link } from "react-router";
import { cn } from "@/lib/utils";

export interface PageTab {
  key: string;
  label: string;
  to: string;
  /** Wires up `aria-controls`/`id` for a page that swaps a tabpanel in place instead of routing
   * to a different page (e.g. Access Control's `?tab=`). Omit for tab strips that just navigate. */
  id?: string;
  ariaControls?: string;
}

/** The one tab strip every tabbed page renders, always directly below its `<h1>` — shared so
 * every tab bar has the same position, markup and visual language. */
export function PageTabs({ tabs, activeKey }: { tabs: PageTab[]; activeKey: string }) {
  return (
    <div role="tablist" className="flex items-center gap-1 border-b border-border">
      {tabs.map((tab) => {
        const active = tab.key === activeKey;
        return (
          <Link
            key={tab.key}
            id={tab.id}
            role="tab"
            aria-selected={active}
            aria-controls={active ? tab.ariaControls : undefined}
            to={tab.to}
            className={cn(
              "-mb-px cursor-pointer border-b-2 px-4 py-2 text-sm font-medium transition-colors",
              active
                ? "border-primary text-foreground"
                : "border-transparent text-muted-foreground hover:text-foreground",
            )}
          >
            {tab.label}
          </Link>
        );
      })}
    </div>
  );
}
