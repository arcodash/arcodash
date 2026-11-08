import { useQuery } from "@tanstack/react-query";
import { Check, RefreshCw } from "lucide-react";
import { api } from "@/lib/api";
import { PageLoader } from "@/components/ui/loader";
import { PageHeader } from "@/components/common/PageHeader";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { cn } from "@/lib/utils";
import { timeAgo, useNow } from "@/lib/scheduling";
import { useTransientMessage } from "@/hooks/useTransientMessage";

export default function SystemStatusPage() {
  const { data, isLoading, isFetching, error, dataUpdatedAt, refetch } = useQuery({
    queryKey: ["system-status"],
    queryFn: () => api.getStatus(),
    refetchInterval: 10_000,
  });
  const now = useNow(false);
  const workersOnline = data?.workers?.online ?? 0;

  const [justRefreshed, setJustRefreshed] = useTransientMessage<true>(2500);

  function handleManualRefresh() {
    // Only the button's own click chains into justRefreshed; the 10s background poll must not pop
    // the confirmation.
    void refetch().then((result) => {
      // react-query refetch() resolves even on failure (rejects only with throwOnError); check
      // result.error before confirming.
      if (!result.error) setJustRefreshed(true);
    });
  }

  return (
    <div className="max-w-3xl space-y-6">
      <PageHeader
        title="System Status"
        action={
          data && (
            <div className="flex items-center gap-3">
              <p className="text-xs text-muted-foreground">Updated {timeAgo(dataUpdatedAt, now)}</p>
              {justRefreshed && (
                <span className="flex items-center gap-1 text-sm text-success">
                  <Check className="size-4" aria-hidden="true" />
                  Refreshed
                </span>
              )}
              <Button variant="outline" size="sm" onClick={handleManualRefresh} disabled={isFetching}>
                <RefreshCw className={cn("size-4", isFetching && "animate-spin")} />
                Refresh
              </Button>
            </div>
          )
        }
      />

      {isLoading && <PageLoader />}
      {error && <p className="text-destructive">Failed to load status.</p>}
      {data && (
        <>
          <p className="flex items-center gap-2">
            {data.status === "ok" ? (
              <Badge className="bg-green-100 text-green-800">Operational</Badge>
            ) : (
              <Badge variant="destructive">Degraded</Badge>
            )}
            <span className="text-muted-foreground">Version {data.version}</span>
          </p>

          <div className="space-y-2">
            <h2 className="text-lg font-semibold">Workers</h2>
            {workersOnline === 0 ? (
              <p className="rounded-md border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
                No worker has checked in. Scheduled refreshes and monitor checks are not running.
              </p>
            ) : (
              <p className="flex items-center gap-1.5 text-sm">
                <span className="size-2.5 rounded-full bg-success" aria-hidden="true" />
                {workersOnline} worker{workersOnline === 1 ? "" : "s"} online
              </p>
            )}
          </div>

          <CountTable
            title="Right now"
            rows={[
              { label: "Queued", value: data.jobs.queued },
              { label: "In flight", value: data.jobs.in_flight },
            ]}
          />
          <CountTable
            title="Last hour"
            rows={[
              { label: "Succeeded", value: data.jobs.recent_success, meaning: "success" },
              { label: "Failed", value: data.jobs.recent_failure, meaning: "failure" },
              { label: "Cancelled", value: data.jobs.recent_cancelled },
            ]}
          />
        </>
      )}
    </div>
  );
}

// Colour by what the number means, not "non-zero is a warning": a healthy succeeded count must not
// read like the failed one.
const MEANING_CLASS = { success: "text-success", failure: "text-destructive" };

function CountTable({
  title,
  rows,
}: {
  title: string;
  rows: { label: string; value: number; meaning?: keyof typeof MEANING_CLASS }[];
}) {
  return (
    <div className="space-y-2">
      <h2 className="text-lg font-semibold">{title}</h2>
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>Status</TableHead>
            <TableHead>Count</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((row) => (
            <TableRow key={row.label}>
              <TableCell>{row.label}</TableCell>
              <TableCell
                className={row.value === 0 ? "text-muted-foreground" : row.meaning && MEANING_CLASS[row.meaning]}
              >
                {row.value}
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </div>
  );
}
