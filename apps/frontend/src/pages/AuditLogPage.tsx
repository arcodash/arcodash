import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router";
import { ScrollText } from "lucide-react";
import { api, apiErrorMessage } from "@/lib/api";
import { PageLoader } from "@/components/ui/loader";
import { PageHeader } from "@/components/common/PageHeader";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { EmptyState } from "@/components/common/EmptyState";
import { exactTs } from "@/lib/scheduling";
import type { AuditEvent } from "@/types/api";

const PAGE_SIZE = 50;

/** Turns a dotted/underscored action string into a label; also the fallback for object_types not in
 * `OBJECT_TYPE_LABELS`. The backend translates legacy action names to `<object>.<verb>` first. */
function formatAction(action: string): string {
  return action
    .split(/[._]/)
    .filter(Boolean)
    .map((word) => (word.toLowerCase() === "ai" ? "AI" : word[0].toUpperCase() + word.slice(1)))
    .join(" ");
}

const OBJECT_TYPE_LABELS: Record<string, string> = {
  query: "Query",
  dashboard: "Dashboard",
  visualization: "Visualization",
  monitor: "Monitor",
  data_source: "Data source",
  destination: "Destination",
  user: "User",
  group: "Group",
  role: "Role",
  organization: "Organization",
  query_result: "Query result",
  embed: "Public embed",
};

function typeLabel(objectType: string): string {
  return OBJECT_TYPE_LABELS[objectType] ?? formatAction(objectType);
}

function ObjectCell({ event }: { event: AuditEvent }) {
  if (event.action === "user.login_failed") {
    const email = typeof event.additional_properties.attempted_email === "string"
      ? event.additional_properties.attempted_email
      : null;
    const ip = typeof event.additional_properties.ip === "string" ? event.additional_properties.ip : null;
    return (
      <div className="min-w-0">
        <span className="block truncate font-medium">{email ?? "Unknown account"}</span>
        <span className="text-xs text-muted-foreground">from {ip ?? "an unknown address"}</span>
      </div>
    );
  }

  if (!event.object_type) return null;

  // An action can have an object_type but no object_id (e.g. a failed run on an unsaved query),
  // unlike a deleted row; render a plain label, not `Type #null`.
  if (event.object_id == null) {
    const label =
      event.object_type === "query" ? "An ad-hoc query" : `An unsaved ${typeLabel(event.object_type).toLowerCase()}`;
    return <span className="block truncate italic text-muted-foreground">{label}</span>;
  }

  const deleted = event.object == null;
  const name = event.object?.name ?? `${typeLabel(event.object_type)} #${event.object_id}`;
  const path = event.object?.path;

  return (
    <div className="min-w-0">
      {path ? (
        <Link to={path} className="block truncate text-primary hover:underline">
          {name}
        </Link>
      ) : (
        <span className={`block truncate ${deleted ? "text-muted-foreground" : ""}`}>
          {name}
          {deleted && <span className="italic"> (deleted)</span>}
        </span>
      )}
      <span className="text-xs text-muted-foreground">{typeLabel(event.object_type)}</span>
    </div>
  );
}

export default function AuditLogPage() {
  const [page, setPage] = useState(1);
  const [action, setAction] = useState("");
  const [fromDate, setFromDate] = useState("");
  const [toDate, setToDate] = useState("");

  const { data, isLoading, isError, error, refetch } = useQuery({
    queryKey: ["events", page, action, fromDate, toDate],
    queryFn: () =>
      api.listEvents({
        page,
        page_size: PAGE_SIZE,
        action: action || undefined,
        from: fromDate || undefined,
        to: toDate || undefined,
      }),
  });

  const events = data?.events ?? [];
  const total = data?.total ?? 0;
  const pageCount = Math.max(1, Math.ceil(total / PAGE_SIZE));
  const filtered = Boolean(action || fromDate || toDate);

  return (
    <div className="max-w-4xl space-y-6">
      <PageHeader title="Audit Log" />

      <div className="flex flex-wrap items-end gap-2">
        <Input
          value={action}
          onChange={(e) => { setAction(e.target.value); setPage(1); }}
          placeholder="Filter by action…"
          className="w-full max-w-56"
        />
        <input
          type="date"
          value={fromDate}
          onChange={(e) => { setFromDate(e.target.value); setPage(1); }}
          aria-label="From date"
          className="h-10 rounded-md border border-input bg-background px-3 text-sm"
        />
        <input
          type="date"
          value={toDate}
          onChange={(e) => { setToDate(e.target.value); setPage(1); }}
          aria-label="To date"
          className="h-10 rounded-md border border-input bg-background px-3 text-sm"
        />
      </div>

      {isLoading && <PageLoader />}

      {isError && (
        <div className="flex items-center justify-between gap-3 rounded-md border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
          <span>{apiErrorMessage(error, "Couldn't load the audit log")}</span>
          <Button variant="outline" size="sm" onClick={() => refetch()}>Try again</Button>
        </div>
      )}

      {!isLoading && !isError && events.length === 0 && filtered && (
        <EmptyState
          icon={ScrollText}
          title="No events in this range"
          description="Try widening the dates or clearing the action filter."
        />
      )}

      {!isLoading && !isError && events.length > 0 && (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Time</TableHead>
              <TableHead>Action</TableHead>
              <TableHead>Object</TableHead>
              <TableHead>User</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {events.map((e) => (
              <TableRow key={e.id}>
                <TableCell className="text-muted-foreground">
                  {exactTs(Date.parse(e.created_at))}
                </TableCell>
                <TableCell>{formatAction(e.action)}</TableCell>
                <TableCell className="text-muted-foreground">
                  <ObjectCell event={e} />
                </TableCell>
                <TableCell className="text-muted-foreground">
                  {e.user_name ?? (e.user_id !== null ? `User #${e.user_id}` : "—")}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      )}

      {!isError && (
        <div className="flex items-center gap-3">
          <span className="text-sm text-muted-foreground">{total.toLocaleString()} events</span>
          <Button variant="outline" size="sm" disabled={page <= 1} onClick={() => setPage((p) => p - 1)}>← Prev</Button>
          <span className="text-sm">Page {page} of {pageCount}</span>
          <Button variant="outline" size="sm" disabled={page >= pageCount} onClick={() => setPage((p) => p + 1)}>Next →</Button>
        </div>
      )}
    </div>
  );
}
