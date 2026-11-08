import { useEffect, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useLocation } from "react-router";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Loader } from "@/components/ui/loader";
import { api, apiErrorMessage } from "@/lib/api";
import { usePermissions } from "@/lib/permissions";
import { exactTs, timeAgo } from "@/lib/scheduling";
import type {
  AiProfileColumn,
  AiRelationship,
  DataSourceAiProfile,
  DataSourceAiProfileResponse,
  AiProfileTable,
} from "@/types/api";

const SOURCE_LABEL: Record<AiRelationship["source"], string> = {
  fk: "Foreign key",
  mined: "From saved queries",
  name: "Matched by name",
};

function plural(n: number, one: string, many = `${one}s`): string {
  return `${n} ${n === 1 ? one : many}`;
}

function sampleCell(c: AiProfileColumn): string {
  if (c.samples.length > 0) return c.samples.slice(0, 3).join(", ");
  if (c.min != null && c.max != null) return `${c.min} – ${c.max}`;
  return "—";
}

const isUnsampled = (t: AiProfileTable) => !!t.error;

function ProfileBody({ profile, shareSamples }: { profile: DataSourceAiProfile; shareSamples: boolean }) {
  const columns = profile.tables.reduce((n, t) => n + t.columns.length, 0);
  const described = profile.tables.reduce(
    (n, t) => n + t.columns.filter((c) => c.description).length,
    0,
  );
  const unsampled = profile.tables.filter(isUnsampled);
  return (
    <div className="space-y-3">
      {unsampled.length > 0 && (
        <details
          className="rounded-md border border-warning/30 bg-warning/10 px-3 py-2 text-sm text-warning-foreground"
          data-testid="ai-profile-unsampled"
        >
          <summary className="cursor-pointer">
            {unsampled.length} of {plural(profile.tables.length, "table")} couldn&apos;t be sampled. AI knows
            their columns but not their values.
          </summary>
          <ul className="mt-2 space-y-1 text-xs">
            {unsampled.map((t) => (
              <li key={t.name}>
                <span className="font-mono">{t.name}</span> — {t.error}
              </li>
            ))}
          </ul>
        </details>
      )}
      <p className="text-sm">
        {`${plural(profile.tables.length, "table")} · ${plural(columns, "column")} · ${plural(profile.relationships.length, "relationship")} · ${plural(described, "AI column description")}`}
      </p>
      {!shareSamples && (
        <p className="text-sm text-muted-foreground">
          Sample values aren&apos;t shared with AI.{" "}
          <Link to="/organization/ai" className="underline">
            Change in AI Settings
          </Link>
        </p>
      )}
      {profile.descriptions_used_samples && !shareSamples && (
        <p className="text-sm text-muted-foreground">
          AI column descriptions were written from sample values, so they&apos;re held back from AI while
          sample sharing is off.
        </p>
      )}
      {described === 0 && (
        <p className="text-sm text-muted-foreground">
          AI writes column descriptions the first time a question needs them.
        </p>
      )}
      <div className="space-y-1">
        {profile.tables.map((t) => (
          <details key={t.name} className="rounded border border-border px-3 py-2">
            <summary className="cursor-pointer text-sm">
              <span className="font-medium">{t.name}</span>{" "}
              <span className={isUnsampled(t) ? "text-warning-foreground" : "text-muted-foreground"}>
                {isUnsampled(t)
                  ? "Couldn't sample this table"
                  : t.data_profiled_at || t.sampled_rows > 0
                    ? `${t.sampled_rows} rows sampled`
                    : "No sample values"}
              </span>
            </summary>
            {t.columns.length > 0 && (
              <table className="mt-2 w-full text-left text-xs">
                <thead className="text-muted-foreground">
                  <tr>
                    <th scope="col" className="py-1 pr-3 font-medium">Column</th>
                    <th scope="col" className="py-1 pr-3 font-medium">Null %</th>
                    <th scope="col" className="py-1 pr-3 font-medium">Distinct (in sample)</th>
                    {shareSamples && <th scope="col" className="py-1 pr-3 font-medium">Samples</th>}
                    <th scope="col" className="py-1 font-medium">Description</th>
                  </tr>
                </thead>
                <tbody>
                  {t.columns.map((c) => (
                    <tr key={c.name} className="border-t border-border align-top">
                      <td className="py-1 pr-3 font-mono">{c.name}</td>
                      <td className="py-1 pr-3">{Math.round(c.null_frac * 100)}%</td>
                      <td className="py-1 pr-3">{c.distinct_in_sample}</td>
                      {shareSamples && <td className="py-1 pr-3 break-all">{sampleCell(c)}</td>}
                      <td className="py-1">{c.description ?? "—"}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </details>
        ))}
      </div>
      <details className="rounded border border-border px-3 py-2">
        <summary className="cursor-pointer text-sm font-medium">
          Relationships ({profile.relationships.length})
        </summary>
        {profile.relationships.length === 0 ? (
          <p className="mt-2 text-sm text-muted-foreground">No relationships found.</p>
        ) : (
          <ul className="mt-2 space-y-1 text-xs">
            {profile.relationships.map((r) => (
              <li key={`${r.from_table}.${r.from_column}>${r.to_table}.${r.to_column}`}>
                <span className="font-mono">
                  {r.from_table}.{r.from_column} → {r.to_table}.{r.to_column}
                </span>{" "}
                <span className="text-muted-foreground">{SOURCE_LABEL[r.source]}</span>
              </li>
            ))}
          </ul>
        )}
      </details>
    </div>
  );
}

export function DataSourceAiProfileCard({
  dataSourceId,
  typeDisplayName,
  schemaRefreshedAt,
}: {
  dataSourceId: number;
  typeDisplayName: string;
  schemaRefreshedAt: string | null;
}) {
  const { isAdmin, isLoading: permsLoading } = usePermissions();
  const queryClient = useQueryClient();
  const key = ["data-source-ai-profile", dataSourceId];
  const enabled = isAdmin && !permsLoading;

  const profileQuery = useQuery({
    queryKey: key,
    queryFn: () => api.getDataSourceAiProfile(dataSourceId),
    enabled,
    refetchInterval: (q) => (q.state.data?.building ? 3000 : false),
  });
  // Read-only view of the SchemaBrowser's cache entry; the page prop alone goes stale after its
  // first load.
  const schemaQuery = useQuery({
    queryKey: ["data-source-schema", dataSourceId],
    queryFn: () => api.getDataSourceSchema(dataSourceId, false),
    enabled: false,
  });
  const schemaUpdatedAt = schemaQuery.dataUpdatedAt;
  const hasSchema = schemaRefreshedAt != null || (schemaQuery.data?.schema.tables.length ?? 0) > 0;
  const settingsQuery = useQuery({
    queryKey: ["ai-settings"],
    queryFn: () => api.getAiSettings(),
    enabled,
  });

  const seenSchemaAt = useRef<number | null>(null);
  useEffect(() => {
    const prev = seenSchemaAt.current;
    seenSchemaAt.current = schemaUpdatedAt;
    // The first run only records the value; the card's own query already fetches on mount.
    if (prev !== null && prev !== schemaUpdatedAt) void queryClient.invalidateQueries({ queryKey: key });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [schemaUpdatedAt]);

  const [alreadyRunning, setAlreadyRunning] = useState(false);
  const [confirmOpen, setConfirmOpen] = useState(false);
  const data = profileQuery.data;

  useEffect(() => {
    if (data && !data.building) setAlreadyRunning(false);
  }, [data]);

  const { hash } = useLocation();
  const sectionRef = useRef<HTMLElement>(null);
  const scrolled = useRef(false);
  const loaded = data !== undefined;
  useEffect(() => {
    if (hash !== "#ai-profile" || !loaded || scrolled.current) return;
    scrolled.current = true;
    sectionRef.current?.scrollIntoView({ block: "start" });
  }, [hash, loaded]);

  const rebuild = useMutation({
    mutationFn: () => api.rebuildDataSourceAiProfile(dataSourceId),
    onSuccess: (res) => {
      setAlreadyRunning(res.status === "already_queued");
      setConfirmOpen(false);
      queryClient.setQueryData<DataSourceAiProfileResponse>(key, (old) =>
        old ? { ...old, building: true } : old,
      );
      void queryClient.invalidateQueries({ queryKey: key });
    },
  });

  if (!enabled) return null;

  const shareSamples = settingsQuery.data?.include_sample_values !== false;
  const building = data?.building ?? false;
  const supported = data?.supported ?? false;
  const noSchema = supported && !hasSchema;
  const showAction = !!data && supported && !noSchema;

  const metered = data?.metered === true;
  const failed = !building && !!data?.last_error;
  const tables = data?.profile?.tables;
  // Profiles stored before per-table timestamps existed carry only the profile-level build time.
  const samplesBuiltAt =
    tables?.reduce<string | null>(
      (m, t) => (t.data_profiled_at && (!m || t.data_profiled_at > m) ? t.data_profiled_at : m),
      null,
    ) ?? (tables?.some((t) => t.sampled_rows > 0) ? (data?.built_at ?? null) : null);
  const collect = metered && !!data?.profile && !samplesBuiltAt;
  const tableCount = data?.profile?.tables.length ?? schemaQuery.data?.schema.tables.length ?? 0;
  const start = () => {
    setAlreadyRunning(false);
    if (!metered) return rebuild.mutate();
    rebuild.reset();
    setConfirmOpen(true);
  };

  let status: string | null = null;
  let readyTitle: string | undefined;
  if (data && showAction && !failed) {
    if (alreadyRunning) status = "A build is already running.";
    else if (building) status = "Updating profile… This can take a few minutes on large schemas.";
    else if (!data.profile) status = "Not built yet";
    else if (!data.current)
      status = "Out of date · some tables aren't in the profile yet. Rebuild to update it.";
    else if (metered && !samplesBuiltAt) status = "Sample values not collected yet";
    else if (metered && samplesBuiltAt) {
      const ms = Date.parse(samplesBuiltAt);
      status = `Ready · sample values from ${timeAgo(ms, Date.now())}`;
      readyTitle = exactTs(ms);
    } else if (data.built_at) {
      const ms = Date.parse(data.built_at);
      status = `Ready · built ${timeAgo(ms, Date.now())}`;
      readyTitle = exactTs(ms);
    }
  }

  return (
    <section id="ai-profile" ref={sectionRef} aria-labelledby="ai-profile-heading" className="space-y-4 rounded-md border border-border p-4">
      <div className="flex items-start justify-between gap-4">
        <div className="space-y-1">
          <h2 id="ai-profile-heading" className="text-sm font-medium">AI profile</h2>
          <p className="text-sm text-muted-foreground">
            What AI knows about this data beyond table and column names: sampled values, null rates and how
            tables join. Used by Ask with AI and Suggest checks with AI.
          </p>
          {metered && (
            <p className="text-sm text-muted-foreground" data-testid="ai-profile-metered-note">
              Table and column names stay up to date automatically. Sample values refresh only when you click
              Rebuild profile, because {typeDisplayName} bills for every query.
            </p>
          )}
        </div>
        {showAction && (
          <Button
            size="sm"
            variant={!data.profile || !data.current || failed || collect ? "default" : "outline"}
            disabled={building || rebuild.isPending}
            onClick={start}
            data-testid={failed ? "ai-profile-retry" : undefined}
          >
            {building ? (
              <>
                <Loader size={14} /> Updating…
              </>
            ) : failed ? (
              "Retry"
            ) : collect ? (
              "Collect sample values"
            ) : data.profile ? (
              "Rebuild profile"
            ) : (
              "Build profile"
            )}
          </Button>
        )}
      </div>

      <div role="status" aria-live="polite" className="text-sm">
        {profileQuery.isLoading && (
          <span className="flex items-center gap-2">
            <Loader size={14} /> Loading AI profile…
          </span>
        )}
        {profileQuery.isError && (
          <span className="flex items-center gap-2">
            Couldn&apos;t load the AI profile.
            <Button size="sm" variant="outline" onClick={() => void profileQuery.refetch()}>
              Retry
            </Button>
          </span>
        )}
        {failed && data && (
          <div
            className="rounded-md border border-warning/30 bg-warning/10 px-3 py-2 text-sm text-warning-foreground"
            data-testid="ai-profile-failed"
          >
            <p>The last profile update failed: {data.last_error}</p>
            <p className="mt-1">AI still uses table and column names. Check the connection, then retry.</p>
          </div>
        )}
        {status && <span title={readyTitle}>{status}</span>}
      </div>

      {rebuild.isError && !confirmOpen && (
        <p className="text-sm text-destructive">
          {apiErrorMessage(rebuild.error, "Couldn't start the rebuild.")}
        </p>
      )}

      {data && !supported && (
        <p className="text-sm text-muted-foreground">
          AI profiles aren&apos;t available for {typeDisplayName} data sources yet. AI still uses table and
          column names.
        </p>
      )}
      {noSchema && (
        <p className="text-sm text-muted-foreground">
          Load the schema below first. The profile is built from it.
        </p>
      )}
      {showAction && !data.profile && !building && (
        <p className="text-sm text-muted-foreground">
          arcodash samples up to 200 rows per table so AI answers use real values instead of guessing from
          column names.
        </p>
      )}
      {showAction && data.profile && <ProfileBody profile={data.profile} shareSamples={shareSamples} />}
      <ConfirmDialog
        open={confirmOpen}
        onOpenChange={setConfirmOpen}
        title={`Run sample queries on ${typeDisplayName}?`}
        description={`arcodash will run a small query (up to 200 rows) on each of the ${tableCount} tables. ${typeDisplayName} bills these to your account like any other query.`}
        confirmLabel="Run sample queries"
        onConfirm={() => rebuild.mutate()}
        confirmPending={rebuild.isPending}
        error={rebuild.isError ? apiErrorMessage(rebuild.error, "Couldn't start the rebuild.") : null}
        testId="ai-profile-metered-confirm"
      />
    </section>
  );
}
