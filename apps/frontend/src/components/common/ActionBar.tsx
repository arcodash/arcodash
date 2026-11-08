import * as React from "react"
import { Fragment } from "react"
import { MoreHorizontal } from "lucide-react"

import { cn } from "@/lib/utils"
import { Button } from "@/components/ui/button"
import { Loader } from "@/components/ui/loader"
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
} from "@/components/ui/dropdown-menu"

export interface ActionItem {
  key: string
  label: string
  icon?: React.ComponentType<{ className?: string }>
  onSelect: () => void
  disabled?: boolean
  pending?: boolean
  pendingLabel?: string
  variant?: "default" | "outline" | "ghost" | "destructive"
  iconOnly?: boolean // wide rendering = ghost icon button with aria-label+title=label
  destructive?: boolean // menu item gets destructive styling; wide button unaffected unless variant says so
  group?: number // menu grouping: separators between different group numbers
  /** Tooltip override (e.g. "why is this disabled") shown on the wide button regardless of
   * iconOnly — falls back to iconOnly's label-as-title behavior when unset. */
  title?: string
  /** Disclosure semantics for an action that toggles a panel. */
  ariaExpanded?: boolean
  ariaControls?: string
}

const COLLAPSE = {
  "36rem": {
    showWideInline: "hidden @min-[36rem]/actionbar:inline-flex",
    showWideFlex: "hidden @min-[36rem]/actionbar:flex",
    hideWide: "@min-[36rem]/actionbar:hidden",
  },
  "54rem": {
    showWideInline: "hidden @min-[54rem]/actionbar:inline-flex",
    showWideFlex: "hidden @min-[54rem]/actionbar:flex",
    hideWide: "@min-[54rem]/actionbar:hidden",
  },
  // Wide-row content can run close to this width; the extra margin over a tighter
  // breakpoint absorbs font-rendering variance across environments.
  "56rem": {
    showWideInline: "hidden @min-[56rem]/actionbar:inline-flex",
    showWideFlex: "hidden @min-[56rem]/actionbar:flex",
    hideWide: "@min-[56rem]/actionbar:hidden",
  },
} as const
export type CollapseAt = keyof typeof COLLAPSE

export interface ActionBarProps {
  primary: ActionItem[]
  secondary?: ActionItem[]
  collapseAt?: CollapseAt
  menuOrder?: string[]
  extra?: React.ReactNode
  menuExtra?: React.ReactNode
  className?: string
}

function orderSecondaries(secondary: ActionItem[], menuOrder?: string[]) {
  if (!menuOrder) return secondary
  const byKey = new Map(secondary.map((item) => [item.key, item]))
  const ordered = menuOrder.flatMap((key) => byKey.get(key) ?? [])
  const remaining = secondary.filter((item) => !menuOrder.includes(item.key))
  return [...ordered, ...remaining]
}

/** Loader + pending label while pending, else icon + label; `hideLabel` for icon-only buttons. */
function ItemContent({ item, loaderSize = 14, hideLabel }: { item: ActionItem; loaderSize?: number; hideLabel?: boolean }) {
  const Icon = item.icon
  return item.pending ? (
    <>
      <Loader size={loaderSize} />
      {!hideLabel && (item.pendingLabel ?? item.label)}
    </>
  ) : (
    <>
      {Icon && <Icon className="size-4" />}
      {!hideLabel && item.label}
    </>
  )
}

export function ActionBar({
  primary,
  secondary = [],
  collapseAt = "54rem",
  menuOrder,
  extra,
  menuExtra,
  className,
}: ActionBarProps) {
  const classes = COLLAPSE[collapseAt]
  const showTrigger = secondary.length > 0 || !!menuExtra
  const orderedSecondary = orderSecondaries(secondary, menuOrder)

  return (
    <div
      data-testid="actionbar-root"
      className={cn("@container/actionbar min-w-0 flex-1", className)}
    >
      <div className="flex items-center justify-end gap-2">
        {extra && (
          <span className={cn(classes.showWideFlex, "items-center gap-2")}>
            {extra}
          </span>
        )}

        {secondary.map((item) => (
          <Button
            key={item.key}
            size={item.iconOnly ? "icon-sm" : "sm"}
            variant={item.variant ?? "outline"}
            className={classes.showWideInline}
            aria-label={item.iconOnly ? item.label : undefined}
            title={item.title ?? (item.iconOnly ? item.label : undefined)}
            disabled={item.disabled || item.pending}
            data-action={item.key}
            onClick={item.onSelect}
          >
            <ItemContent item={item} hideLabel={item.iconOnly} />
          </Button>
        ))}

        {showTrigger && (
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button
                size="icon-sm"
                variant="ghost"
                aria-label="More actions"
                className={classes.hideWide}
              >
                <MoreHorizontal className="size-4" />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              {orderedSecondary.map((item, index) => (
                <Fragment key={item.key}>
                  {index > 0 && orderedSecondary[index - 1].group !== item.group && <DropdownMenuSeparator />}
                  <DropdownMenuItem
                    disabled={item.disabled || item.pending}
                    className={cn(item.destructive && "text-destructive focus:text-destructive")}
                    data-action={item.key}
                    onSelect={item.onSelect}
                  >
                    <ItemContent item={item} />
                  </DropdownMenuItem>
                </Fragment>
              ))}
              {menuExtra && (
                <>
                  <DropdownMenuSeparator />
                  {menuExtra}
                </>
              )}
            </DropdownMenuContent>
          </DropdownMenu>
        )}

        {primary.map((item) => (
          <Button
            key={item.key}
            variant={item.variant ?? "default"}
            disabled={item.disabled || item.pending}
            title={item.title}
            aria-expanded={item.ariaExpanded}
            aria-controls={item.ariaControls}
            data-action={item.key}
            onClick={item.onSelect}
          >
            <ItemContent item={item} loaderSize={16} />
          </Button>
        ))}
      </div>
    </div>
  )
}
