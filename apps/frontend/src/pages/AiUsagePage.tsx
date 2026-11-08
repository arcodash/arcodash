import { useState } from "react";
import { Link } from "react-router";
import { useQuery } from "@tanstack/react-query";
import { Bar, BarChart, CartesianGrid, Legend, ResponsiveContainer, Tooltip, XAxis, YAxis } from "recharts";
import { formatNumeric, formatUsd } from "@/lib/formatNumber";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { PageHeader } from "@/components/common/PageHeader";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { PageLoader } from "@/components/ui/loader";
import { AiTabs } from "@/components/ai/AiTabs";
import { api, apiErrorMessage } from "@/lib/api";
import type { AiUsageResponse } from "@/types/api";
import { BUCKET_COLOR, BUCKET_LABEL, chartData, type Bucket } from "@/lib/usageChart";

const PERIODS = [7, 30, 90] as const;
const PROVIDER_LABELS: Record<string, string> = { gemini: "Gemini", openai: "OpenAI", anthropic: "Anthropic" };
const FEATURE_LABELS: Record<string, string> = {
  sql_generation: "SQL generation",
  ask: "Ask with AI",
  auto_visualize: "Auto visualize",
  describe_schema: "Schema description",
  monitor_suggestions: "Monitor suggestions",
  monitor_suggestions_query: "Query monitor suggestions",
  explain_alert_plan: "Alert explanation (plan)",
  explain_alert: "Alert explanation",
  dashboard_narrative: "Explain dashboard",
  dashboard_brief: "Dashboard morning brief",
};

const compactNumber = new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 1 });

interface UsageTooltipPayloadItem {
  dataKey: Bucket;
  value: number;
  payload?: { cost: number | null };
}

const resetDate = (iso: string) =>
  new Date(iso).toLocaleDateString("en-US", { month: "short", day: "numeric", timeZone: "UTC" });

function BudgetSummary({ budget }: { budget: AiUsageResponse["budget"] }) {
  const { monthly_usd: cap, spent_month_usd: spent } = budget;
  const reset = resetDate(budget.resets_at);
  const pct = cap ? Math.round((spent / cap) * 100) : 0;
  const over = cap !== null && spent >= cap;
  const fill = over ? "bg-destructive" : pct >= 80 ? "bg-warning" : "bg-foreground/60";
  return (
    <Card data-testid="ai-budget-summary">
      <CardContent className="space-y-2 py-4">
        <div className="flex items-baseline justify-between gap-2">
          {cap !== null ? (
            <span className="text-sm">
              Spent this month <span className="font-semibold tabular-nums">{formatUsd(spent)}</span> of {formatUsd(cap)}
            </span>
          ) : (
            <span className="text-sm">
              Spent this month <b className="tabular-nums">{formatUsd(spent)}</b>, no cap
            </span>
          )}
          {cap !== null ? (
            <span className={`text-xs tabular-nums ${over ? "text-destructive" : "text-muted-foreground"}`}>
              {pct}% · resets {reset} (UTC)
            </span>
          ) : (
            <span className="text-xs text-muted-foreground">
              <Link to="/organization/ai" className="underline">
                Set a budget
              </Link>
            </span>
          )}
        </div>
        {cap !== null && (
          <div
            className="h-1.5 w-full overflow-hidden rounded-full bg-muted"
            role="progressbar"
            aria-label="Monthly AI budget used"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={Math.min(pct, 100)}
            aria-valuetext={`${formatUsd(spent)} of ${formatUsd(cap)} (${pct}%)`}
            data-testid="ai-budget-bar"
          >
            <div className={`h-full rounded-full ${fill}`} style={{ width: `${Math.min(pct, 100)}%` }} />
          </div>
        )}
        {over && (
          <p className="text-xs text-destructive" data-testid="ai-budget-paused">
            AI features are paused until {reset}. Alerts still send without AI analysis.
          </p>
        )}
      </CardContent>
    </Card>
  );
}

function UserCell({ row }: { row: AiUsageResponse["by_user"][number] }) {
  if (row.user_id === null) return <span className="text-muted-foreground">System (background jobs)</span>;
  if (row.email === null) return <span className="text-muted-foreground italic">Deleted user</span>;
  if (row.name)
    return (
      <>
        {row.name}
        {row.email !== row.name && <div className="text-xs text-muted-foreground">{row.email}</div>}
      </>
    );
  return <>{row.email}</>;
}

interface UsageRow {
  calls: number;
  tokens_in: number;
  tokens_out: number;
  error_rate?: number;
  cost_usd: number | null;
}

function UsageTable<R extends UsageRow>({
  title,
  nameHeader,
  rows,
  rowKey,
  renderName,
  nameClassName,
  showErrorRate = false,
  testId,
}: {
  title: string;
  nameHeader: string;
  rows: R[];
  rowKey: (row: R) => string | number;
  renderName: (row: R) => React.ReactNode;
  nameClassName?: string;
  showErrorRate?: boolean;
  testId?: string;
}) {
  return (
    <Card data-testid={testId}>
      <CardHeader>
        <CardTitle>{title}</CardTitle>
      </CardHeader>
      <CardContent>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>{nameHeader}</TableHead>
              <TableHead>Calls</TableHead>
              <TableHead>Tokens in</TableHead>
              <TableHead>Tokens out</TableHead>
              {showErrorRate && <TableHead>Error rate</TableHead>}
              <TableHead>Cost</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((row) => (
              <TableRow key={rowKey(row)}>
                <TableCell className={nameClassName}>{renderName(row)}</TableCell>
                <TableCell className="tabular-nums">{row.calls}</TableCell>
                <TableCell className="tabular-nums">{compactNumber.format(row.tokens_in)}</TableCell>
                <TableCell className="tabular-nums">{compactNumber.format(row.tokens_out)}</TableCell>
                {showErrorRate && (
                  <TableCell className="tabular-nums">{((row.error_rate ?? 0) * 100).toFixed(1)}%</TableCell>
                )}
                <TableCell className="tabular-nums">{formatUsd(row.cost_usd)}</TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </CardContent>
    </Card>
  );
}

export function UsageTooltip({ active, payload, label }: { active?: boolean; payload?: UsageTooltipPayloadItem[]; label?: string }) {
  if (!active || !payload || payload.length === 0) return null;
  return (
    <div className="rounded-md border bg-popover px-3 py-2 text-sm text-popover-foreground shadow-md">
      <p className="mb-1 font-medium text-foreground">{label}</p>
      {payload.map((item) => (
        <p key={item.dataKey} className="flex items-center gap-2">
          <span className="h-0.5 w-3 shrink-0" style={{ backgroundColor: BUCKET_COLOR[item.dataKey] }} />
          <span className="font-medium tabular-nums">{item.value}</span>
          <span className="text-muted-foreground">{BUCKET_LABEL[item.dataKey]}</span>
        </p>
      ))}
      <p className="mt-1 flex gap-2 border-t pt-1">
        <span className="font-medium tabular-nums">{formatUsd(payload[0].payload?.cost)}</span>
        <span className="text-muted-foreground">cost</span>
      </p>
    </div>
  );
}

export default function AiUsagePage() {
  const [days, setDays] = useState<(typeof PERIODS)[number]>(30);
  const { data, isLoading, isError, error } = useQuery({
    queryKey: ["ai-usage", days],
    queryFn: () => api.getAiUsage(days),
  });
  // Reuses the ["ai-settings"] cache AiSettingsPage already populates — just the active
  // provider's name for the "Active" pill in the by-provider table.
  const settingsQuery = useQuery({ queryKey: ["ai-settings"], queryFn: () => api.getAiSettings() });
  const activeProvider = settingsQuery.data?.provider_order[0];

  return (
    <div className="max-w-4xl space-y-6">
      <PageHeader
        title="AI Usage"
        action={
          <div className="flex items-center gap-1">
            {PERIODS.map((p) => (
              <Button key={p} variant={days === p ? "default" : "outline"} size="sm" onClick={() => setDays(p)}>
                {p}d
              </Button>
            ))}
          </div>
        }
      />
      <AiTabs />
      <p className="text-sm text-muted-foreground">
        Aggregate usage across your organization. Individual requests aren&apos;t logged and can&apos;t be
        downloaded.
      </p>

      {isLoading && <PageLoader />}
      {isError && <p className="text-destructive">{apiErrorMessage(error, "Failed to load AI usage")}</p>}

      {data && <BudgetSummary budget={data.budget} />}

      {data && data.totals.total_calls === 0 && (
        <Card>
          <CardContent className="py-8 text-center text-sm text-muted-foreground">
            No AI usage yet. Numbers appear here once your team starts using Ask with AI.
          </CardContent>
        </Card>
      )}

      {data && data.totals.total_calls > 0 && (
        <>
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-5">
            {[
              { label: "Total calls", value: compactNumber.format(data.totals.total_calls) },
              { label: "Tokens in", value: compactNumber.format(data.totals.tokens_in) },
              { label: "Tokens out", value: compactNumber.format(data.totals.tokens_out) },
              { label: "Success rate", value: `${(data.totals.success_rate * 100).toFixed(1)}%` },
              { label: "Cost", value: formatUsd(data.totals.cost_usd) },
            ].map((tile) => (
              <div key={tile.label} className="rounded-md border bg-card p-3">
                <div className="text-xl font-semibold tabular-nums text-foreground">{tile.value}</div>
                <div className="text-xs tracking-wide text-muted-foreground uppercase">{tile.label}</div>
              </div>
            ))}
          </div>

          <Card>
            <CardHeader>
              <CardTitle>Calls per day</CardTitle>
            </CardHeader>
            <CardContent>
              <ResponsiveContainer width="100%" height={220}>
                <BarChart data={chartData(data.by_day)} barCategoryGap="20%">
                  <CartesianGrid vertical={false} stroke="var(--color-border)" strokeDasharray="0" />
                  <XAxis
                    dataKey="date"
                    tickLine={false}
                    axisLine={{ stroke: "var(--color-border)" }}
                    tick={{ fill: "var(--color-muted-foreground)", fontSize: 11 }}
                    tickFormatter={(d: string) => d.slice(5)}
                    minTickGap={20}
                  />
                  <YAxis
                    width={56}
                    tickLine={false}
                    axisLine={false}
                    allowDecimals={false}
                    tickFormatter={(v: number) => formatNumeric(v)}
                    tick={{ fill: "var(--color-muted-foreground)", fontSize: 11 }}
                  />
                  <Tooltip content={<UsageTooltip />} cursor={{ fill: "var(--color-muted)" }} />
                  <Legend
                    formatter={(_value, entry) => (
                      <span className="text-xs text-muted-foreground">
                        {BUCKET_LABEL[(entry.dataKey as Bucket) ?? "ok"]}
                      </span>
                    )}
                  />
                  <Bar dataKey="ok" name="OK" stackId="status" fill={BUCKET_COLOR.ok} stroke="var(--color-card)" strokeWidth={2} isAnimationActive={false} />
                  <Bar
                    dataKey="warn"
                    name="Quota / invalid key"
                    stackId="status"
                    fill={BUCKET_COLOR.warn}
                    stroke="var(--color-card)"
                    strokeWidth={2}
                    isAnimationActive={false}
                  />
                  <Bar
                    dataKey="error"
                    name="Failed"
                    stackId="status"
                    fill={BUCKET_COLOR.error}
                    stroke="var(--color-card)"
                    strokeWidth={2}
                    radius={[4, 4, 0, 0]}
                    isAnimationActive={false}
                  />
                </BarChart>
              </ResponsiveContainer>
            </CardContent>
          </Card>

          <UsageTable
            title="By provider"
            nameHeader="Provider"
            rows={data.by_provider}
            rowKey={(row) => row.provider}
            nameClassName="flex items-center gap-2"
            renderName={(row) => (
              <>
                {PROVIDER_LABELS[row.provider] ?? row.provider}
                {row.provider === activeProvider && <Badge>Active</Badge>}
              </>
            )}
            showErrorRate
          />
          <UsageTable
            title="By feature"
            nameHeader="Feature"
            rows={data.by_feature}
            rowKey={(row) => row.feature}
            renderName={(row) => FEATURE_LABELS[row.feature] ?? row.feature}
            showErrorRate
          />
          <UsageTable
            title="By user"
            nameHeader="User"
            rows={data.by_user}
            rowKey={(row) => row.user_id ?? "system"}
            renderName={(row) => <UserCell row={row} />}
            testId="ai-usage-by-user"
          />
          <p className="text-xs text-muted-foreground" data-testid="ai-cost-footnote">
            Cost uses list prices; unknown models are not priced. Unpriced calls show — and don&apos;t count toward the
            monthly budget, which resets on the 1st of each month (UTC).
          </p>
        </>
      )}
    </div>
  );
}
