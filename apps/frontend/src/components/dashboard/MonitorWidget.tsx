import { BellOff } from "lucide-react";
import { Link } from "react-router";
import { Badge } from "@/components/ui/badge";
import { StateBadge } from "@/components/common/StateBadge";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { exactTs, timeAgo, useNow } from "@/lib/scheduling";
import { formatComparisonValue, opLabel } from "@/lib/monitors";
import { formatNumeric } from "@/lib/formatNumber";
import { fitFontSize } from "@/lib/fitText";
import type { MonitorTile, Widget } from "@/types/api";

/** One monitor slot resolved against the widget's tiles; `tile` is `null` when the backend dropped
 * the id (deleted or unshared). */
interface ResolvedRow {
  id: number;
  tile: MonitorTile | null;
}

function resolveRows(widget: Widget): ResolvedRow[] {
  const ids = widget.options.monitors ?? [];
  const byId = new Map((widget.monitors ?? []).map((t) => [t.id, t]));
  return ids.map((id) => ({ id, tile: byId.get(id) ?? null }));
}

/** Whatever JSON the warehouse returned for the column; only number/string get grouped digits, the
 * rest is stringified (`formatNumeric` assumes a number-shaped string). */
function renderObserved(value: unknown): string | null {
  if (value == null) return null;
  if (typeof value === "number" || typeof value === "string") return formatNumeric(value);
  return String(value);
}

/** triggered > check failed > unevaluable > unknown > (ok | unavailable); an unavailable tile is
 * neutral, never red/amber. */
function rowSeverity(row: ResolvedRow): number {
  if (!row.tile) return 0;
  if (row.tile.state === "triggered") return 4;
  if (row.tile.check_failed) return 3;
  if (row.tile.state === "unevaluable") return 2;
  if (row.tile.state === "unknown") return 1;
  return 0;
}

const TRIGGER_FOCUS = "rounded-full focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring";

/** State badge. A failed check's reason shows in a tooltip only when the viewer may see it
 * (`last_check_error` is null on public/embed views). `relative z-10` keeps the trigger clickable
 * above a row's link overlay. */
function TileStateBadge({ tile }: { tile: MonitorTile }) {
  const failed = tile.check_failed;
  const error = tile.last_check_error;
  if (!failed || tile.state === "triggered") return <StateBadge state={tile.state} checkFailed={failed} />;
  if (!error) {
    return (
      <span title="The last check didn't finish.">
        <StateBadge state={tile.state} checkFailed />
      </span>
    );
  }
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          aria-label={`Check failed: ${error}`}
          className={`relative z-10 ${TRIGGER_FOCUS}`}
        >
          <StateBadge state={tile.state} checkFailed />
        </button>
      </TooltipTrigger>
      <TooltipContent className="max-w-xs break-words">{error}</TooltipContent>
    </Tooltip>
  );
}

function LearningBadge({ learning, className }: { learning: NonNullable<MonitorTile["learning"]>; className?: string }) {
  return (
    <Badge variant="secondary" className={className} data-testid="monitor-learning-badge">
      Learning ({learning.have}/{learning.need})
      <span className="sr-only">
        : {learning.have} of {learning.need} checks collected, alerts start after {learning.need}
      </span>
    </Badge>
  );
}

/** "> 100"-style threshold text for a comparison monitor. */
function thresholdText(tile: MonitorTile): string {
  return `${opLabel(tile.op ?? "").symbol} ${formatComparisonValue(String(tile.value ?? ""))}`;
}

export interface MonitorCardMeta {
  treatment: "destructive" | "warning" | "neutral";
  title: string;
  monitorId?: number;
}

/** Card-level chrome + header title, used by `WidgetCard` to style its root/header before
 * `MonitorWidget` renders the body. */
export function computeMonitorCardMeta(widget: Widget): MonitorCardMeta {
  const rows = resolveRows(widget);
  const treatment = rows.some((r) => r.tile?.state === "triggered")
    ? "destructive"
    : rows.some((r) => r.tile?.state === "unevaluable")
      ? "warning"
      : "neutral";
  const title = rows.length === 1 ? (rows[0].tile?.name ?? "Monitor status") : "Monitor status";
  const monitorId = rows.length === 1 && rows[0].tile ? rows[0].id : undefined;
  return { treatment, title, monitorId };
}

function FailedFooter({ tile, checkedMs, now }: { tile: MonitorTile; checkedMs: number | null; now: number }) {
  const text = checkedMs != null ? `Last check failed ${timeAgo(checkedMs, now)}` : "Last check failed";
  const title = checkedMs != null ? exactTs(checkedMs) : undefined;
  const error = tile.last_check_error;
  if (tile.state !== "triggered" || !error) {
    return (
      <p className="text-xs text-destructive" title={title}>
        {text}
      </p>
    );
  }
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          aria-label={`Check failed: ${error}`}
          title={title}
          className={`relative z-10 w-fit text-left text-xs text-destructive ${TRIGGER_FOCUS}`}
        >
          {text}
        </button>
      </TooltipTrigger>
      <TooltipContent className="max-w-xs break-words">{error}</TooltipContent>
    </Tooltip>
  );
}

function SingleMonitorCard({ row }: { row: ResolvedRow }) {
  const now = useNow(true);

  if (!row.tile) {
    return (
      <div className="flex h-full flex-col gap-1" data-testid="monitor-widget">
        <p className="text-sm text-muted-foreground">
          This monitor was deleted or isn&apos;t shared with you.
        </p>
      </div>
    );
  }

  const tile = row.tile;
  const failed = tile.check_failed;
  const valueColor = failed
    ? "text-muted-foreground"
    : tile.state === "triggered"
      ? "text-destructive"
      : tile.state === "ok"
        ? "text-foreground"
        : "text-muted-foreground";
  const checkedMs = tile.last_checked_at ? Date.parse(tile.last_checked_at) : null;
  const observedText = renderObserved(tile.observed) ?? "—";

  return (
    <div
      className="flex h-full flex-col gap-1 [container-type:inline-size]"
      data-testid="monitor-widget"
    >
      <div className="flex items-center gap-1.5">
        <span data-testid="monitor-widget-state">
          <TileStateBadge tile={tile} />
        </span>
        {tile.learning && <LearningBadge learning={tile.learning} className="shrink-0" />}
        {tile.muted && (
          <BellOff
            className="h-3.5 w-3.5 shrink-0 text-muted-foreground"
            aria-label="Notifications muted"
          />
        )}
      </div>
      <p
        className={`max-w-full break-words font-serif font-medium leading-none tabular-nums ${valueColor}`}
        style={{ fontSize: fitFontSize(observedText, 1.875) }}
        title={observedText}
        data-testid="monitor-widget-value"
      >
        {observedText}
      </p>
      {tile.column && tile.kind === "baseline" ? (
        <p
          className="truncate text-xs text-muted-foreground"
          title={`Alerts when ${tile.column} moves well outside its recent pattern.`}
        >
          <span className="font-medium text-foreground">{tile.column}</span> vs. recent pattern
        </p>
      ) : tile.column ? (
        <p
          className="truncate text-xs text-muted-foreground"
          title={`Alerts when ${tile.column} ${opLabel(tile.op ?? "").phrase} ${formatComparisonValue(String(tile.value ?? ""))}.`}
        >
          <span className="font-medium text-foreground">{tile.column}</span> {thresholdText(tile)}
        </p>
      ) : (
        <p className="truncate text-xs text-muted-foreground">No condition set</p>
      )}
      {tile.state === "unevaluable" && !failed && (
        <p className="truncate text-xs text-muted-foreground">
          Last check couldn&apos;t compare {tile.column}.
        </p>
      )}
      {tile.state === "triggered" && tile.incident_summary && (
        <Tooltip>
          <TooltipTrigger asChild>
            <button
              type="button"
              className="min-w-0 truncate text-left text-xs text-muted-foreground rounded-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              data-testid="monitor-widget-incident"
            >
              {tile.incident_summary}
            </button>
          </TooltipTrigger>
          <TooltipContent className="max-w-xs">{tile.incident_summary}</TooltipContent>
        </Tooltip>
      )}
      <div className="flex-1" />
      {failed ? (
        <FailedFooter tile={tile} checkedMs={checkedMs} now={now} />
      ) : (
        tile.state !== "unknown" &&
        checkedMs != null && (
          <p className="text-xs text-muted-foreground" title={exactTs(checkedMs)}>
            Checked {timeAgo(checkedMs, now)}
          </p>
        )
      )}
    </div>
  );
}

function rowChromeClass(row: ResolvedRow): string {
  if (!row.tile) return "border-l-border";
  if (row.tile.state === "triggered") return "border-l-destructive bg-destructive/10";
  if (row.tile.check_failed) return "border-l-destructive";
  if (row.tile.state === "unevaluable") return "border-l-warning bg-warning/10";
  if (row.tile.state === "ok") return "border-l-success";
  return "border-l-border";
}

function MonitorRow({ row, now, linkable }: { row: ResolvedRow; now: number; linkable: boolean }) {
  if (!row.tile) {
    return (
      <li
        className={`flex items-center gap-2 border-l-4 px-2 py-1.5 ${rowChromeClass(row)}`}
        data-testid="monitor-widget-row"
      >
        <span className="min-w-0 flex-1 truncate text-sm text-muted-foreground">
          Unavailable monitor
        </span>
        <Badge variant="outline">Unavailable</Badge>
      </li>
    );
  }

  const tile = row.tile;
  const checkedMs = tile.last_checked_at ? Date.parse(tile.last_checked_at) : null;
  const conditionPart = tile.column
    ? tile.kind === "baseline"
      ? `${tile.column} vs. recent pattern · `
      : `${tile.column} ${thresholdText(tile)} · `
    : "";
  const ago = checkedMs != null ? timeAgo(checkedMs, now) : null;
  const statusPart = tile.check_failed ? `Last check failed${ago != null ? ` ${ago}` : ""}` : `Checked ${ago ?? "never"}`;
  const learningPart = tile.learning ? ` · Learning ${tile.learning.have}/${tile.learning.need}` : "";
  const incidentPart = tile.incident_summary && tile.state === "triggered" ? ` · ${tile.incident_summary}` : "";
  const title = `${tile.name} · ${conditionPart}${statusPart}${learningPart}${incidentPart}`;

  return (
    <li
      className={`relative flex items-start gap-2 border-l-4 px-2 py-1.5 ${rowChromeClass(row)} ${linkable ? "hover:bg-muted/50 has-[a:focus-visible]:ring-2 has-[a:focus-visible]:ring-inset has-[a:focus-visible]:ring-ring" : ""}`}
      title={title}
      data-testid="monitor-widget-row"
    >
      <div className="min-w-0 flex-1 break-words text-sm line-clamp-2">
      {linkable ? (
        <Link
          to={`/monitors/${tile.id}`}
          className="outline-none after:absolute after:inset-0 after:content-[''] hover:underline"
        >
          {tile.name}
        </Link>
      ) : (
        tile.name
      )}
      </div>
      {tile.muted && (
        <BellOff className="h-3 w-3 shrink-0 text-muted-foreground" aria-label="Notifications muted" />
      )}
      <span className="hidden shrink-0 tabular-nums text-xs text-muted-foreground @min-[18rem]/mlist:inline">
        {renderObserved(tile.observed) ?? "—"}
      </span>
      {tile.learning && (
        <span className="hidden shrink-0 @min-[18rem]/mlist:inline">
          <LearningBadge learning={tile.learning} />
        </span>
      )}
      <span className="shrink-0" data-testid="monitor-widget-state">
        <TileStateBadge tile={tile} />
      </span>
    </li>
  );
}

function MonitorListCard({ rows, linkable }: { rows: ResolvedRow[]; linkable: boolean }) {
  const now = useNow(true);
  const sorted = [...rows].sort((a, b) => {
    const sev = rowSeverity(b) - rowSeverity(a);
    if (sev !== 0) return sev;
    return (a.tile?.name ?? "").localeCompare(b.tile?.name ?? "");
  });
  const triggeredCount = rows.filter((r) => r.tile?.state === "triggered").length;
  const failedCount = rows.filter((r) => r.tile?.check_failed && r.tile.state !== "triggered").length;
  const okCount = rows.filter((r) => r.tile?.state === "ok" && !r.tile.check_failed).length;

  return (
    <div className="@container/mlist flex h-full flex-col" data-testid="monitor-widget">
      <ul className="min-h-0 flex-1 divide-y divide-border overflow-y-auto">
        {sorted.map((row) => (
          <MonitorRow key={row.id} row={row} now={now} linkable={linkable} />
        ))}
      </ul>
      {rows.length > 4 && (
        <p className="sticky bottom-0 shrink-0 border-t border-border bg-card/95 px-2 py-1 text-xs text-muted-foreground">
          {triggeredCount > 0 && (
            <>
              <span className="font-medium text-destructive">{triggeredCount} alerting</span>
              {" · "}
            </>
          )}
          {failedCount > 0 && (
            <>
              <span className="font-medium text-destructive">{failedCount} check failed</span>
              {" · "}
            </>
          )}
          {okCount} OK
        </p>
      )}
    </div>
  );
}

/** Widget bound to `options.monitors`: single-tile card for one monitor, scrollable severity-sorted
 * list for two or more; shared by the dashboard and public/embed page. */
export function MonitorWidget({ widget, linkable = false }: { widget: Widget; linkable?: boolean }) {
  const rows = resolveRows(widget);
  if (rows.length === 0) {
    return (
      <div className="flex h-full items-center justify-center" data-testid="monitor-widget">
        <p className="text-sm text-muted-foreground">No monitors</p>
      </div>
    );
  }
  if (rows.length === 1) return <SingleMonitorCard row={rows[0]} />;
  return <MonitorListCard rows={rows} linkable={linkable} />;
}
