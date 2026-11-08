import { useState, useEffect, useRef } from "react";
import { Link, useLocation, useNavigate, useParams, useSearchParams } from "react-router";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { ArrowDown, ArrowUp, Bell, ChevronDown, SearchX, Sparkles, Workflow } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Badge } from "@/components/ui/badge";
import { Switch } from "@/components/ui/switch";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { DropdownMenu, DropdownMenuTrigger, DropdownMenuContent, DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Loader, PageLoader } from "@/components/ui/loader";
import { OwnerChip } from "@/components/sharing/OwnerChip";
import { VisibilityChip } from "@/components/sharing/VisibilityChip";
import { ShareDialog } from "@/components/sharing/ShareDialog";
import { ScheduleSection, MONITOR_FLAVOR } from "@/components/scheduling/ScheduleSection";
import { BackButton } from "@/components/nav/BackButton";
import { StateBadge } from "@/components/common/StateBadge";
import { EmptyState } from "@/components/common/EmptyState";
import { DocsLink } from "@/components/common/DocsLink";
import { ReadonlyField } from "@/components/common/ReadonlyField";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { useTransientMessage } from "@/hooks/useTransientMessage";
import { ApiError, api, apiErrorMessage } from "@/lib/api";
import { RichText } from "@/components/monitors/richText";
import { usePermissions } from "@/lib/permissions";
import { useDocumentTitle } from "@/lib/useDocumentTitle";
import { exactTs, useNow } from "@/lib/scheduling";
import {
  OPS,
  OP_LABELS,
  SENSITIVITIES,
  SENSITIVITY_LABELS,
  opLabel,
  isComparableColumn,
  formatComparisonValue,
  createMonitorFromSql,
  PartialCheckError,
  type PartialCheck,
} from "@/lib/monitors";
import { MonitorKindToggle } from "@/components/monitors/MonitorKindToggle";
import { LatestIncidentCard } from "@/components/monitors/LatestIncidentCard";
import { LearningBadge } from "@/components/monitors/LearningBadge";
import { learningStatus } from "@/lib/monitorLearning";
import { ObservationSparkline } from "@/components/monitors/ObservationSparkline";
import type {
  EscalationStep,
  Monitor,
  MonitorKind,
  MonitorOptions,
  QueryMonitorSuggestion,
  ResultColumn,
  Sensitivity,
} from "@/types/api";

interface AiDraft {
  sql: string;
  data_source_id: number;
  query_name: string;
  suggestion: QueryMonitorSuggestion;
  columns?: ResultColumn[];
}
interface MonitorLocationState {
  createSubscribeError?: string;
  aiSuggestion?: QueryMonitorSuggestion;
  aiDraft?: AiDraft;
}

/** Compact absolute time; no relative form, since it would go stale without a live timer. */
function formatSnoozeUntil(iso: string): string {
  return new Date(iso).toLocaleString([], {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

/** Editable-fields snapshot standing in for a version token (monitors have none; mute/snooze/ack/worker ticks touch `updated_at`).
 * `steps` is excluded: escalation saves via its own mutation. */
function editableSnapshot(m: Monitor): string {
  return JSON.stringify({
    name: m.name,
    rearm: m.rearm ?? null,
    kind: m.options.kind ?? "threshold",
    sensitivity: m.options.sensitivity ?? "medium",
    column: m.options.column ?? "",
    op: m.options.op ?? ">",
    value: m.options.value ?? 0,
    custom_subject: m.options.custom_subject ?? "",
    custom_body: m.options.custom_body ?? "",
    max_fresh_wait_secs: m.options.max_fresh_wait_secs ?? null,
  });
}

function DeliveryStatusBadge({ status }: { status: string }) {
  if (status === "sent") return <Badge className="bg-green-100 text-green-800">Sent</Badge>;
  if (status === "failed") return <Badge variant="destructive">Failed</Badge>;
  if (status === "retrying")
    return <Badge className="border-warning/30 bg-warning/10 text-warning-foreground">Retrying</Badge>;
  return <Badge variant="outline">{status}</Badge>;
}

type Feedback = { kind: "ok" | "error"; text: string };

function FeedbackLine({ feedback, as: Tag = "p" }: { feedback: Feedback | null; as?: "p" | "span" }) {
  if (!feedback) return null;
  return (
    <Tag className={feedback.kind === "error" ? "text-sm text-destructive" : "text-sm text-muted-foreground"}>
      {feedback.text}
    </Tag>
  );
}

/** Local editor draft; `destination_id: null` = an unpicked new row (Save stays disabled until each
 * has one). */
interface EscalationStepDraft {
  delay_minutes: number;
  destination_id: number | null;
}

export function MonitorPage() {
  const { id } = useParams<{ id: string }>();
  const isNew = !id;
  const monitorId = Number(id);
  // A non-numeric route param must never reach the API (the backend 400s with a raw error).
  const idValid = !isNew && Number.isFinite(monitorId);
  const navigate = useNavigate();
  const location = useLocation();
  const queryClient = useQueryClient();
  const { has } = usePermissions();
  const [searchParams] = useSearchParams();
  const [shareOpen, setShareOpen] = useState(false);
  const [deleteConfirmOpen, setDeleteConfirmOpen] = useState(false);

  const [name, setName] = useState("");
  // Pre-selects the query when arriving from its "Add monitor" link; a loaded monitor overwrites
  // it.
  const [queryId, setQueryId] = useState<string>(() => searchParams.get("query_id") ?? "");
  const [kind, setKind] = useState<MonitorKind>("threshold");
  const [sensitivity, setSensitivity] = useState<Sensitivity>("medium");
  const [column, setColumn] = useState("");
  const [op, setOp] = useState<string>(">");
  const [value, setValue] = useState<string>("0");
  const [customSubject, setCustomSubject] = useState("");
  const [customBody, setCustomBody] = useState("");
  const [rearmMode, setRearmMode] = useState<"always" | "throttled">("always");
  const [rearmMinutes, setRearmMinutes] = useState<string>("60");
  const [maxFreshWaitSecs, setMaxFreshWaitSecs] = useState<number | null>(null);
  const [scheduleExpanded, setScheduleExpanded] = useState(true);
  // Save-flow errors only; delete/mute/subscribe/unsubscribe have their own state.
  const [saveError, setSaveError] = useState<string | null>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const [muteFeedback, setMuteFeedback] = useTransientMessage<string>();
  const [subscriptionFeedback, setSubscriptionFeedback] = useTransientMessage<string>();
  const [conflict, setConflict] = useState(false);
  // Freshness check standing in for a version token.
  const seededEditableRef = useRef<string | null>(null);

  // Create navigates here without remounting, carrying a subscribe failure in navigation state;
  // surface it, then clear the state so back/forward doesn't resurface it.
  useEffect(() => {
    const state = location.state as MonitorLocationState | null;
    if (state?.createSubscribeError) {
      setSubscriptionFeedback(state.createSubscribeError);
      navigate(`${location.pathname}${location.search}`, { replace: true, state: null });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [location.state]);
  // Captured once per navigation: the effect above clears route state but the banner and draft must
  // outlive it.
  const [aiDraft, setAiDraft] = useState<AiDraft | null>(null);
  const [aiSuggestion, setAiSuggestion] = useState<QueryMonitorSuggestion | null>(null);
  const [draftQueryName, setDraftQueryName] = useState("");
  const [draftPartial, setDraftPartial] = useState<PartialCheck | null>(null);
  const aiMode = isNew ? (aiDraft ? "draft" : aiSuggestion ? "suggestion" : null) : null;
  const aiShown = aiDraft?.suggestion ?? aiSuggestion;

  useEffect(() => {
    if (!isNew) return;
    const state = location.state as MonitorLocationState | null;
    const draft = state?.aiDraft ?? null;
    const s = draft?.suggestion ?? state?.aiSuggestion;
    if (!s) return;
    setAiDraft(draft);
    setAiSuggestion(draft ? null : s);
    setDraftQueryName(draft?.query_name ?? "");
    setDraftPartial(null);
    setName(s.name);
    setKind(s.kind);
    setColumn(s.column);
    if (s.kind === "baseline") {
      setSensitivity(s.sensitivity ?? "medium");
    } else {
      setOp(s.op ?? ">");
      setValue(String(s.value ?? 0));
    }
    // Keyed on the navigation entry so in-place state clearing never re-seeds edits.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [location.key]);

  const dataSourcesQuery = useQuery({
    queryKey: ["data-sources"],
    queryFn: () => api.listDataSources(),
    enabled: aiDraft != null,
  });
  const draftDataSourceName = dataSourcesQuery.data?.data_sources.find(
    (d) => d.id === aiDraft?.data_source_id,
  )?.name;
  const [destChoice, setDestChoice] = useState<string>("email");
  const chosenDestinationId = destChoice === "email" ? null : Number(destChoice);
  // Create-mode only: also subscribe the creator (on by default).
  const [notifyOnCreate, setNotifyOnCreate] = useState(true);
  const [steps, setSteps] = useState<EscalationStepDraft[]>([]);
  const [escalationFeedback, setEscalationFeedback] = useTransientMessage<Feedback>();

  const monitorQuery = useQuery({
    queryKey: ["monitor", id],
    queryFn: () => api.getMonitor(Number(id)),
    enabled: idValid,
  });

  useDocumentTitle(
    isNew
      ? "New Monitor · arcodash"
      : monitorQuery.data
        ? `${monitorQuery.data.name || "Untitled monitor"} · arcodash`
        : "Monitor · arcodash",
  );

  const queriesQuery = useQuery({
    queryKey: ["queries"],
    queryFn: () => api.listQueries(),
  });

  // Whether the query has ever produced a result, so we skip a column request that would 404
  // (unknown for archived queries).
  const selectedQuery = queriesQuery.data?.find((q) => String(q.id) === queryId);
  const knownNoResult = selectedQuery != null && selectedQuery.latest_query_data_id == null;

  // Real result columns for the picker: a mistyped free-text column would make a monitor that
  // silently never fires.
  const columnsQuery = useQuery({
    queryKey: ["query-result-columns", queryId],
    queryFn: () => api.getLatestQueryResult(Number(queryId)),
    enabled: aiDraft == null && queryId !== "" && Number.isFinite(Number(queryId)) && !knownNoResult,
    retry: false,
  });
  // A 404 just means the query has never run; only other errors are worth a Retry.
  const columnsLoadFailed =
    columnsQuery.error != null && !(columnsQuery.error instanceof ApiError && columnsQuery.error.status === 404);
  // A draft query has no saved result, so its columns come from the AI preview.
  const availableColumns: ResultColumn[] = aiDraft
    ? (aiDraft.columns ?? [{ name: aiDraft.suggestion.column, friendly_type: "" }])
    : (columnsQuery.data?.query_result.data.columns ?? []);
  // Only numeric-comparable columns: a string/date column would save silently and never fire.
  const comparableColumns = availableColumns.filter(isComparableColumn);
  const selectedColumnMeta = availableColumns.find((c) => c.name === column);
  const columnTypeValid = selectedColumnMeta == null || isComparableColumn(selectedColumnMeta);

  const observationsQuery = useQuery({
    queryKey: ["monitor-observations", id, monitorQuery.dataUpdatedAt],
    queryFn: () => api.getMonitorObservations(Number(id), 100),
    enabled: idValid,
  });

  const latestIncident = [...(observationsQuery.data ?? [])]
    .reverse()
    .find((o): o is typeof o & { explanation: string } => o.explanation != null);
  const [focusRunId, setFocusRunId] = useState<number | null>(null);
  const focusSchedule = () => {
    setScheduleExpanded(true);
    document.getElementById("monitor-schedule")?.scrollIntoView({ behavior: "smooth", block: "start" });
  };
  const viewRun = (runId: number) => {
    setFocusRunId(null);
    setTimeout(() => setFocusRunId(runId), 0);
    focusSchedule();
  };

  const scheduleQuery = useQuery({
    queryKey: ["schedules", "monitor", monitorId],
    queryFn: () => api.getSchedule("monitor", monitorId),
    enabled: idValid && monitorQuery.data?.learning != null,
  });

  const subsQuery = useQuery({
    queryKey: ["monitor-subs", id],
    queryFn: () => api.listMonitorSubscriptions(Number(id)),
    enabled: idValid,
  });

  const destinationsQuery = useQuery({
    queryKey: ["destinations"],
    queryFn: () => api.listDestinations(),
  });

  const deliveriesQuery = useQuery({
    queryKey: ["monitor-deliveries", id],
    queryFn: () => api.listMonitorDeliveries(Number(id)),
    enabled: idValid,
  });

  function hydrateFrom(a: Monitor) {
    setName(a.name);
    setQueryId(String(a.query.id));
    setKind(a.options.kind ?? a.kind ?? "threshold");
    setSensitivity(a.options.sensitivity ?? "medium");
    setColumn(a.options.column ?? "");
    setOp(a.options.op ?? ">");
    setValue(String(a.options.value ?? 0));
    setCustomSubject(a.options.custom_subject ?? "");
    setCustomBody(a.options.custom_body ?? "");
    if (a.rearm == null || a.rearm === 0) {
      setRearmMode("always");
    } else {
      setRearmMode("throttled");
      setRearmMinutes(String(a.rearm / 60));
    }
    setMaxFreshWaitSecs(a.options.max_fresh_wait_secs ?? null);
    setSteps((a.escalation ?? []).map((s) => ({ delay_minutes: s.delay_minutes, destination_id: s.destination_id })));
    seededEditableRef.current = editableSnapshot(a);
  }

  useEffect(() => {
    if (monitorQuery.data) hydrateFrom(monitorQuery.data);
    // Refetches after mute/snooze/ack/save return a new object; key on id so they don't re-seed
    // in-progress edits.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [monitorQuery.data?.id]);

  async function handleReload() {
    const { data, error: refetchError } = await monitorQuery.refetch();
    if (refetchError || !data) {
      setSaveError("Couldn't reload this monitor. Check your connection and try again.");
      return;
    }
    hydrateFrom(data);
    setConflict(false);
    setSaveError(null);
  }

  const saveMutation = useMutation({
    mutationFn: async () => {
      const isBaseline = kind === "baseline";
      const options: MonitorOptions = {
        column,
        kind,
        ...(isBaseline
          ? { sensitivity, op: null, value: null }
          : { op, value: Number(value), sensitivity: null }),
        custom_subject: customSubject.trim() || undefined,
        custom_body: customBody.trim() || undefined,
        // Omit rather than send an explicit null; same "always wait" default to the backend.
        max_fresh_wait_secs: maxFreshWaitSecs ?? undefined,
      };
      const rearm = rearmMode === "always" ? null : Number(rearmMinutes) * 60;
      if (isNew && aiDraft) {
        try {
          const r = await createMonitorFromSql(
            {
              dataSourceId: aiDraft.data_source_id,
              queryName: draftQueryName.trim(),
              sql: aiDraft.sql,
              monitorName: name,
              options,
              rearm: rearm ?? undefined,
              subscribe: notifyOnCreate ? { destinationId: chosenDestinationId } : false,
            },
            draftPartial ?? {},
          );
          return { id: r.monitorId, draft: true as const, subscribeFailed: notifyOnCreate && !r.subscribed };
        } catch (err) {
          if (err instanceof PartialCheckError) setDraftPartial(err.partial);
          throw err;
        }
      }
      if (isNew) {
        return api.createMonitor({ query_id: Number(queryId), name, options, rearm: rearm ?? undefined });
      }
      // The backend merges `options` (null deletes a key), so merge onto the current copy to keep
      // mute/snooze state.
      const current = await api.getMonitor(Number(id));
      if (seededEditableRef.current != null && editableSnapshot(current) !== seededEditableRef.current) {
        throw new ApiError(
          409,
          "This monitor changed since you opened it. Reload to see the latest before saving.",
          "conflict",
        );
      }
      return api.updateMonitor(Number(id), { name, options: { ...current.options, ...options }, rearm });
    },
    onSuccess: async (a) => {
      if ("draft" in a) {
        queryClient.invalidateQueries({ queryKey: ["monitors"] });
        queryClient.invalidateQueries({ queryKey: ["queries"] });
        navigate(
          `/monitors/${a.id}`,
          a.subscribeFailed ? { state: { createSubscribeError: "Monitor created, but subscribing you to it failed." } } : undefined,
        );
        return;
      }
      seededEditableRef.current = editableSnapshot(a);
      setConflict(false);
      queryClient.invalidateQueries({ queryKey: ["monitors"] });
      if (isNew) {
        let subscribeError: string | null = null;
        if (notifyOnCreate) {
          try {
            await api.createMonitorSubscription(a.id, { destination_id: chosenDestinationId });
          } catch (err) {
            // Monitor already exists; carry the failure into navigation so the creator sees it now.
            subscribeError = apiErrorMessage(err, "Monitor created, but subscribing you to it failed.");
          }
        }
        navigate(`/monitors/${a.id}`, subscribeError ? { state: { createSubscribeError: subscribeError } } : undefined);
      } else {
        queryClient.invalidateQueries({ queryKey: ["monitor", id] });
      }
    },
    onError: (err) => {
      if (err instanceof ApiError && err.status === 409 && err.errorCode === "conflict") {
        setConflict(true);
        return;
      }
      setSaveError(apiErrorMessage(err, "Save failed"));
    },
  });

  const invalidateMonitor = () => {
    queryClient.invalidateQueries({ queryKey: ["monitor", id] });
    queryClient.invalidateQueries({ queryKey: ["monitors"] });
  };

  const deleteMutation = useMutation({
    mutationFn: () => api.deleteMonitor(Number(id)),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["monitors"] });
      navigate("/monitors");
    },
    onError: (err) => setDeleteError(apiErrorMessage(err, "Delete failed")),
  });

  const muteMutation = useMutation({
    mutationFn: (nextMuted: boolean) =>
      nextMuted ? api.muteMonitor(Number(id)) : api.unmuteMonitor(Number(id)),
    onSuccess: invalidateMonitor,
    onError: (err) => setMuteFeedback(apiErrorMessage(err, "Couldn't update mute state.")),
  });

  const [snoozeFeedback, setSnoozeFeedback] = useTransientMessage<Feedback>();

  const snoozeMutation = useMutation({
    mutationFn: (minutes: number) => api.snoozeMonitor(Number(id), minutes),
    onSuccess: invalidateMonitor,
    onError: (err) => {
      const text =
        err instanceof ApiError && err.message ? err.message : "Couldn't update snooze. Please try again.";
      setSnoozeFeedback({ kind: "error", text });
    },
  });

  const subscribeMutation = useMutation({
    mutationFn: () => api.createMonitorSubscription(Number(id), { destination_id: chosenDestinationId }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["monitor-subs", id] });
      // A triggered monitor may have auto-notified this subscriber; refresh Delivery history.
      queryClient.invalidateQueries({ queryKey: ["monitor-deliveries", id] });
      setDestChoice("email");
    },
    onError: (err) => {
      // Already subscribed: treat as a no-op success.
      if (err instanceof ApiError && err.status === 409) {
        queryClient.invalidateQueries({ queryKey: ["monitor-subs", id] });
        setDestChoice("email");
        return;
      }
      setSubscriptionFeedback(apiErrorMessage(err, "Subscribe failed"));
    },
  });

  const unsubMutation = useMutation({
    mutationFn: (subId: number) => api.deleteMonitorSubscription(subId),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["monitor-subs", id] }),
    onError: (err) => setSubscriptionFeedback(apiErrorMessage(err, "Couldn't remove subscription.")),
  });

  const [notifyFeedback, setNotifyFeedback] = useTransientMessage<Feedback>();

  const notifyMutation = useMutation({
    mutationFn: (subId: number) => api.notifyMonitorSubscription(Number(id), subId),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["monitor-deliveries", id] });
      setNotifyFeedback({
        kind: "ok",
        text: "Notification queued — the result appears in Delivery history below.",
      });
    },
    onError: (err) => {
      const text =
        err instanceof ApiError && err.status === 404
          ? "This monitor no longer exists."
          : err instanceof ApiError && err.message
            ? err.message
            : "Couldn't send the notification. Please try again.";
      setNotifyFeedback({ kind: "error", text });
    },
  });

  const saveEscalation = useMutation({
    mutationFn: (payload: EscalationStep[]) => api.putMonitorEscalation(Number(id), payload),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["monitor", id] });
      setEscalationFeedback({ kind: "ok", text: "Escalation steps saved." });
    },
    onError: (err) => {
      const text =
        err instanceof ApiError && err.message
          ? err.message
          : "Couldn't save escalation steps. Please try again.";
      setEscalationFeedback({ kind: "error", text });
    },
  });

  const escalationValid = steps.every((s) => Number.isInteger(s.delay_minutes) && s.delay_minutes >= 1 && s.destination_id != null);

  function updateStep(i: number, patch: Partial<EscalationStepDraft>) {
    setSteps((prev) => prev.map((s, idx) => (idx === i ? { ...s, ...patch } : s)));
  }

  function moveStep(i: number, dir: -1 | 1) {
    setSteps((prev) => {
      const next = [...prev];
      const j = i + dir;
      if (j < 0 || j >= next.length) return prev;
      [next[i], next[j]] = [next[j], next[i]];
      return next;
    });
  }

  function removeStep(i: number) {
    setSteps((prev) => prev.filter((_, idx) => idx !== i));
  }

  function addStep() {
    setSteps((prev) => [...prev, { delay_minutes: 30, destination_id: null }]);
  }

  const [ackFeedback, setAckFeedback] = useTransientMessage<Feedback>();

  const ackMutation = useMutation({
    mutationFn: () => api.acknowledgeMonitor(Number(id)),
    onSuccess: invalidateMonitor,
    onError: (err) => {
      const text =
        err instanceof ApiError && err.status === 409
          ? "This monitor is no longer alerting."
          : err instanceof ApiError && err.message
            ? err.message
            : "Couldn't acknowledge. Please try again.";
      setAckFeedback({ kind: "error", text });
    },
  });

  // Number("") is 0, not NaN — an empty field must be rejected explicitly, not just non-numeric
  // input.
  const valueValid = value.trim() !== "" && Number.isFinite(Number(value));
  const rearmMinutesValid = rearmMode !== "throttled" || Number(rearmMinutes) >= 1;

  // Names the condition(s) blocking Save, for the disabled button's tooltip.
  const saveDisabledReasons: string[] = [];
  if (!name) saveDisabledReasons.push("a name");
  if (aiDraft) {
    if (!draftQueryName.trim()) saveDisabledReasons.push("a query name");
  } else if (!queryId) saveDisabledReasons.push("a query");
  if (!column) saveDisabledReasons.push("a column");
  else if (!columnTypeValid) saveDisabledReasons.push("a numeric column (this one can't be compared)");
  if (kind === "threshold" && !valueValid) saveDisabledReasons.push("a valid comparison value");
  if (!rearmMinutesValid) saveDisabledReasons.push("a valid throttle interval");

  const canSave = isNew ? has("create_monitor") : (monitorQuery.data?.caller.can_edit ?? false);
  const canDelete = monitorQuery.data?.caller.can_edit ?? false;
  // Sharing is owner-tier on top of edit-tier (like `can_delete`), so losing edit_monitor loses it.
  const canManageSharing = canDelete && monitorQuery.data?.caller.object_role === "owner";

  const isMuted = monitorQuery.data?.options.muted ?? false;
  const mutedUntil = monitorQuery.data?.options.muted_until;
  // Snooze is measured in hours, so the 30s idle tick of useNow(false) flips this back on lapse.
  const now = useNow(false);
  const learning =
    monitorQuery.data?.learning
      ? learningStatus(monitorQuery.data.learning, scheduleQuery.isLoading || scheduleQuery.isError ? undefined : (scheduleQuery.data ?? null), now)
      : null;
  const isSnoozed = !!mutedUntil && new Date(mutedUntil).getTime() > now;
  const isSilenced = isMuted || isSnoozed;

  // null destination_id -> "Email"; found -> "type: name"; not found -> stub.
  function resolveChannel(destinationId: number | null): string {
    if (destinationId == null) return "Email";
    const dest = destinationsQuery.data?.find((d) => d.id === destinationId);
    if (dest) return `${dest.type}: ${dest.name}`;
    return `Destination #${destinationId}`;
  }

  const destinationOptions = destinationsQuery.data?.map((d) => (
    <SelectItem key={d.id} value={String(d.id)}>
      {d.type}: {d.name}
    </SelectItem>
  ));

  const lastSent = deliveriesQuery.data?.find((d) => d.status === "sent");

  // An unresolvable route id (deleted, wrong, no access) must not fall through to a blank edit form
  // with a live Subscribe button.
  if (!isNew && !monitorQuery.isLoading && !monitorQuery.data) {
    const isNotFound =
      !idValid || (monitorQuery.error instanceof ApiError && monitorQuery.error.status === 404);
    return (
      <div className="max-w-2xl">
        <EmptyState
          icon={SearchX}
          iconClassName="bg-destructive/10 text-destructive"
          title={isNotFound ? "Monitor not found" : "Couldn't load this monitor"}
          description={
            isNotFound
              ? "This monitor doesn't exist or you don't have access to it."
              : apiErrorMessage(monitorQuery.error, "Something went wrong loading this monitor.")
          }
          action={
            <Button asChild variant="outline">
              <Link to="/monitors">Back to monitors</Link>
            </Button>
          }
        />
      </div>
    );
  }

  const monitorControls = (
    <>
      <div>
        <label htmlFor="monitor-rearm-mode" className="text-sm font-medium">
          Notify — how often you're told while this stays alerting
        </label>
        <div className="mt-1 grid grid-cols-1 sm:grid-cols-2 gap-3">
          <Select value={rearmMode} onValueChange={(v) => setRearmMode(v as "always" | "throttled")} disabled={!canSave}>
            <SelectTrigger id="monitor-rearm-mode" data-testid="monitor-rearm-mode-select"><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value="always">Each time it changes</SelectItem>
              <SelectItem value="throttled">At most every N minutes</SelectItem>
            </SelectContent>
          </Select>
          {rearmMode === "throttled" && (
            <div>
              <Input
                id="monitor-rearm-minutes"
                type="number"
                min={1}
                value={rearmMinutes}
                disabled={!canSave}
                onChange={(e) => setRearmMinutes(e.target.value)}
              />
              {!rearmMinutesValid && <p className="mt-1 text-xs text-destructive">Enter a number</p>}
            </div>
          )}
        </div>
      </div>
      <div>
        <label htmlFor="monitor-max-fresh-wait" className="text-sm font-medium">
          When a refresh is already running
        </label>
        <Select
          value={maxFreshWaitSecs == null ? "always" : String(maxFreshWaitSecs)}
          onValueChange={(v) => setMaxFreshWaitSecs(v === "always" ? null : Number(v))}
          disabled={!canSave}
        >
          <SelectTrigger id="monitor-max-fresh-wait" className="w-64" data-testid="monitor-max-wait-select"><SelectValue /></SelectTrigger>
          <SelectContent>
            <SelectItem value="always">Always wait for fresh data</SelectItem>
            <SelectItem value="300">Up to 5 min, then check anyway</SelectItem>
            <SelectItem value="600">Up to 10 min, then check anyway</SelectItem>
            <SelectItem value="1800">Up to 30 min, then check anyway</SelectItem>
            <SelectItem value="3600">Up to 1 hour, then check anyway</SelectItem>
          </SelectContent>
        </Select>
        <p className="mt-1 text-xs text-muted-foreground">
          If your data is still refreshing, we wait for it before checking — so the alert reflects
          the latest numbers. Set a ceiling to check against the last result instead of waiting
          indefinitely.
        </p>
      </div>
    </>
  );

  return (
    <div className="max-w-2xl space-y-6">
      <BackButton to="/monitors" label="Back to monitors" />
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="break-words text-2xl font-bold text-foreground">
              {isNew ? "New Monitor" : (monitorQuery.data?.name ?? "Monitor")}
            </h1>
            {!isNew && monitorQuery.data && (
              <>
                <OwnerChip name={monitorQuery.data.user.name} showName />
                <VisibilityChip visibility={monitorQuery.data.visibility} objectType="monitor" />
                <StateBadge state={monitorQuery.data.state} />
                {learning && (
                  <LearningBadge status={learning} nowMs={now} canEdit={monitorQuery.data?.caller.can_edit ?? false} onChangeSchedule={focusSchedule} />
                )}
              </>
            )}
          </div>
        </div>
        {!isNew && monitorQuery.data && (
          <div className="flex flex-wrap shrink-0 items-center gap-2">
            {canManageSharing && (
              <Button variant="outline" size="sm" onClick={() => setShareOpen(true)}>
                Share
              </Button>
            )}
            {/* Ungated beyond visibility: ack is a "teammate has seen this" signal open to any org member who can view. */}
            {monitorQuery.data.state === "triggered" && !monitorQuery.data.acked_at && (
              <Button size="sm" onClick={() => ackMutation.mutate()} disabled={ackMutation.isPending}>
                {ackMutation.isPending ? "Acknowledging…" : "Acknowledge"}
              </Button>
            )}
            {canDelete && !isMuted &&
              (isSnoozed ? (
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => snoozeMutation.mutate(0)}
                  disabled={snoozeMutation.isPending}
                >
                  {snoozeMutation.isPending ? "Cancelling…" : "Cancel snooze"}
                </Button>
              ) : (
                <DropdownMenu>
                  <DropdownMenuTrigger asChild>
                    <Button variant="outline" size="sm" disabled={snoozeMutation.isPending}>
                      Snooze
                      <ChevronDown size={14} />
                    </Button>
                  </DropdownMenuTrigger>
                  <DropdownMenuContent align="end">
                    <DropdownMenuItem onClick={() => snoozeMutation.mutate(60)}>1 hour</DropdownMenuItem>
                    <DropdownMenuItem onClick={() => snoozeMutation.mutate(240)}>4 hours</DropdownMenuItem>
                    <DropdownMenuItem onClick={() => snoozeMutation.mutate(1440)}>1 day</DropdownMenuItem>
                  </DropdownMenuContent>
                </DropdownMenu>
              ))}
            {canDelete && (
              <Button
                variant="outline"
                size="sm"
                onClick={() => muteMutation.mutate(!isMuted)}
                disabled={muteMutation.isPending}
              >
                {isMuted ? "Unmute" : "Mute"}
              </Button>
            )}
          </div>
        )}
      </div>

      {!isNew && monitorQuery.data && (
        <ShareDialog
          objectType="monitor"
          objectId={Number(id)}
          ownerId={monitorQuery.data.user.id}
          visibility={monitorQuery.data.visibility}
          open={shareOpen}
          canManage={canManageSharing}
          onClose={() => setShareOpen(false)}
          onChanged={() => monitorQuery.refetch()}
        />
      )}

      {!isNew && monitorQuery.data?.state === "unevaluable" && monitorQuery.data.unevaluable_reason && (
        <p className="text-sm text-destructive">
          Can&apos;t evaluate: {monitorQuery.data.unevaluable_reason}
        </p>
      )}

      {!isNew && subsQuery.data && subsQuery.data.length === 0 && (
        <div className="rounded-md border border-warning/30 bg-warning/10 px-3 py-2 text-sm text-warning-foreground">
          No one is subscribed — this monitor won&apos;t notify anyone when it triggers.
        </div>
      )}

      {!isNew && (
        <p className="text-sm text-muted-foreground">
          {lastSent ? `Last notified ${exactTs(Date.parse(lastSent.created_at))}` : "Not notified yet"}
        </p>
      )}

      {!isNew && monitorQuery.data?.acked_at && (
        <p className="text-sm text-muted-foreground">
          Acknowledged by {monitorQuery.data.acked_by_email ?? "someone"} on{" "}
          {exactTs(Date.parse(monitorQuery.data.acked_at))} — escalation stopped.
        </p>
      )}

      {!isNew &&
        monitorQuery.data?.state === "triggered" &&
        (monitorQuery.data.escalation?.length ?? 0) > 0 &&
        !monitorQuery.data.acked_at && (
          <p className="text-sm text-muted-foreground">
            Escalation:{" "}
            {monitorQuery.data.escalation_step === 0
              ? `no steps notified yet, ${monitorQuery.data.escalation.length} configured`
              : `step ${monitorQuery.data.escalation_step} of ${monitorQuery.data.escalation.length} notified`}
          </p>
        )}

      {isMuted && (
        <p className="text-sm text-muted-foreground">Muted — notifications paused.</p>
      )}

      {!isMuted && isSnoozed && mutedUntil && (
        <p className="text-sm text-muted-foreground">
          Snoozed until {formatSnoozeUntil(mutedUntil)} — notifications paused.
        </p>
      )}

      {muteFeedback && <p className="text-sm text-destructive">{muteFeedback}</p>}

      <FeedbackLine feedback={snoozeFeedback} />
      <FeedbackLine feedback={ackFeedback} />

      {!isNew && latestIncident && <LatestIncidentCard observation={latestIncident} onViewRun={viewRun} />}

      {monitorQuery.isLoading && <PageLoader />}

      <div className="space-y-4">
        {(isNew || monitorQuery.data) && !canSave && (
          <p className="rounded-md bg-muted px-3 py-2 text-sm text-muted-foreground">
            {isNew
              ? "You don't have permission to create monitors. Viewing only."
              : "You don't have permission to edit this monitor. Viewing only."}
          </p>
        )}
        {aiMode && aiShown && (
          <div
            className="rounded-md border border-border bg-muted/40 p-3 text-sm space-y-1"
            data-testid="monitor-ai-banner"
          >
            <p>
              <Sparkles size={14} className="mr-1 inline" aria-hidden />
              {aiMode === "draft" ? (
                <>
                  <span className="font-medium">AI draft</span> — nothing is saved until you click
                  Save. Saving also creates and publishes the query below.
                </>
              ) : (
                <>
                  <span className="font-medium">AI suggestion</span> — review the settings below,
                  then save.
                </>
              )}
            </p>
            <p className="text-muted-foreground">Why this matters: <RichText text={aiShown.why} /></p>
          </div>
        )}
        <div>
          <label htmlFor="monitor-name" className="text-sm font-medium">
            Name<span className="text-destructive"> *</span>
          </label>
          <Input
            id="monitor-name"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="Monitor name"
            disabled={!canSave}
          />
        </div>

        {aiDraft ? (
          <div className="space-y-3">
            <div>
              <label htmlFor="monitor-draft-query-name" className="text-sm font-medium">
                Query name<span className="text-destructive"> *</span>
              </label>
              <Input
                id="monitor-draft-query-name"
                data-testid="monitor-draft-query-name"
                value={draftQueryName}
                onChange={(e) => setDraftQueryName(e.target.value)}
                disabled={!canSave || draftPartial?.queryId != null}
              />
            </div>
            <div>
              <p className="text-sm font-medium">SQL (written by AI)</p>
              <pre
                className="max-h-48 overflow-auto rounded-md bg-muted/40 p-3 text-xs font-mono"
                data-testid="monitor-draft-sql"
              >
                {aiDraft.sql}
              </pre>
              {draftDataSourceName && (
                <p className="mt-1 text-xs text-muted-foreground">Runs on {draftDataSourceName}.</p>
              )}
            </div>
          </div>
        ) : (
          <div>
            {monitorQuery.data && !queriesQuery.data?.some((q) => String(q.id) === queryId) ? (
              // listQueries() excludes archived queries; fall back to the name the monitor response
              // carries.
              <ReadonlyField id="monitor-query" label="Query" value={monitorQuery.data.query.name} />
            ) : (
              <>
                <label htmlFor="monitor-query" className="text-sm font-medium">
                  Query<span className="text-destructive"> *</span>
                </label>
                {queriesQuery.data && (
                  <Select value={queryId} onValueChange={setQueryId} disabled={!isNew || !canSave}>
                    <SelectTrigger id="monitor-query" data-testid="monitor-query-select">
                      <SelectValue placeholder="Select a query" />
                    </SelectTrigger>
                    <SelectContent>
                      {queriesQuery.data.map((q) => (
                        <SelectItem key={q.id} value={String(q.id)}>
                          {q.name}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                )}
              </>
            )}
          </div>
        )}

        <div>
          <p id="monitor-kind-label" className="text-sm font-medium">Alert type</p>
          <MonitorKindToggle value={kind} onChange={setKind} disabled={!canSave} labelId="monitor-kind-label" />
          <p className="mt-1 text-xs text-muted-foreground">
            {kind === "baseline"
              ? "Alerts when the value moves well outside its recent pattern. Needs 7 checks before it can alert."
              : "Alerts when a value crosses a number you set."}
          </p>
        </div>

        <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">
          <div>
            <label htmlFor="monitor-column" className="text-sm font-medium">
              Column<span className="text-destructive"> *</span>
            </label>
            {availableColumns.length > 0 ? (
              <>
                <Select value={column} onValueChange={setColumn} disabled={!canSave}>
                  <SelectTrigger id="monitor-column" data-testid="monitor-column-select">
                    <SelectValue placeholder="Select a column" />
                  </SelectTrigger>
                  <SelectContent>
                    {comparableColumns.map((c) => (
                      <SelectItem key={c.name} value={c.name}>
                        {c.name}
                      </SelectItem>
                    ))}
                    {column && !comparableColumns.some((c) => c.name === column) && (
                      <SelectItem value={column}>
                        {column} {selectedColumnMeta ? "(not numeric)" : "(not in latest result)"}
                      </SelectItem>
                    )}
                  </SelectContent>
                </Select>
                {comparableColumns.length === 0 && (
                  <p className="mt-1 text-xs text-muted-foreground">
                    None of this query's columns look numeric — a monitor compares a number, so
                    the check won't fire.
                  </p>
                )}
                {!columnTypeValid && comparableColumns.length > 0 && (
                  <p className="mt-1 text-xs text-destructive">
                    &quot;{column}&quot; isn&apos;t a number column — this comparison will never fire.
                  </p>
                )}
              </>
            ) : (
              <>
                <Input
                  id="monitor-column"
                  value={column}
                  disabled={!canSave || columnsQuery.isLoading || knownNoResult}
                  onChange={(e) => setColumn(e.target.value)}
                  placeholder={columnsQuery.isLoading ? "Loading columns…" : "e.g. count"}
                />
                {!columnsQuery.isLoading &&
                  (columnsLoadFailed ? (
                    <p className="mt-1 text-xs text-destructive">
                      Couldn't load result columns.{" "}
                      <button
                        type="button"
                        className="cursor-pointer underline underline-offset-2"
                        onClick={() => columnsQuery.refetch()}
                      >
                        Retry
                      </button>
                    </p>
                  ) : (
                    <p className="mt-1 text-xs text-muted-foreground">
                      Run the query at least once to pick from its result columns.
                    </p>
                  ))}
              </>
            )}
          </div>
          {kind === "baseline" ? (
            <div>
              <label htmlFor="monitor-sensitivity" className="text-sm font-medium">Sensitivity</label>
              <Select value={sensitivity} onValueChange={(v) => setSensitivity(v as Sensitivity)} disabled={!canSave}>
                <SelectTrigger id="monitor-sensitivity" data-testid="monitor-sensitivity-select"><SelectValue /></SelectTrigger>
                <SelectContent>
                  {SENSITIVITIES.map((s) => (
                    <SelectItem key={s} value={s}>
                      {SENSITIVITY_LABELS[s].label}
                      <span className="ml-2 text-muted-foreground">{SENSITIVITY_LABELS[s].hint}</span>
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <p className="mt-1 text-xs text-muted-foreground">
                On a value that barely changes, alerts at about a {SENSITIVITY_LABELS[sensitivity].flatPct}% move.
              </p>
            </div>
          ) : (
            <>
              <div>
                <label htmlFor="monitor-operator" className="text-sm font-medium">Comparison</label>
                <Select value={op} onValueChange={setOp} disabled={!canSave}>
                  <SelectTrigger id="monitor-operator" data-testid="monitor-operator-select"><SelectValue /></SelectTrigger>
                  <SelectContent>
                    {OPS.map((o) => (
                      <SelectItem key={o} value={o}>
                        {OP_LABELS[o].phrase}
                        <span className="ml-2 text-muted-foreground">{OP_LABELS[o].symbol}</span>
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
              <div>
                <label htmlFor="monitor-value" className="text-sm font-medium">
                  Value<span className="text-destructive"> *</span>
                </label>
                <Input
                  id="monitor-value"
                  type="number"
                  value={value}
                  onChange={(e) => setValue(e.target.value)}
                  disabled={!canSave}
                />
                {!valueValid && <p className="mt-1 text-xs text-destructive">Enter a number</p>}
                {aiShown?.kind === "threshold" && aiShown.confidence !== "high" && (
                  <p className="mt-1 text-xs text-muted-foreground" data-testid="monitor-guessed-hint">
                    AI picked this number — adjust it to fit your data.
                  </p>
                )}
              </div>
            </>
          )}
        </div>

        {column && (kind === "baseline" || valueValid) && (
          <div>
            {kind === "baseline" ? (
              <p className="mt-2 text-sm text-muted-foreground">
                Alerts when <span className="font-medium text-foreground">{column}</span> in the first row
                moves well outside its last 30 checks.
              </p>
            ) : (
              <p className="mt-2 text-sm text-muted-foreground">
                Alerts when <span className="font-medium text-foreground">{column}</span>{" "}
                in the first row <span className="font-medium text-foreground">{opLabel(op).phrase}</span>{" "}
                <span className="font-medium text-foreground">{formatComparisonValue(value)}</span>.
              </p>
            )}
            <p className="mt-1 text-xs text-muted-foreground">
              Only the first row of the result is checked. Order your SQL so the row you care
              about comes first. <DocsLink topic="monitorEvaluation" label="how a check is evaluated" />
            </p>
          </div>
        )}

        {!isNew && (
          <div>
            <p className="text-sm font-medium">Recent checks</p>
            <ObservationSparkline
              observations={observationsQuery.data}
              kind={monitorQuery.data?.options.kind ?? kind}
              threshold={monitorQuery.data?.options.value ?? undefined}
              learning={monitorQuery.data?.learning}
              learningStatus={learning}
              nowMs={now}
              column={column}
              isLoading={observationsQuery.isLoading}
              isError={observationsQuery.isError}
              onRetry={() => observationsQuery.refetch()}
            />
          </div>
        )}

        <div>
          <label htmlFor="monitor-custom-subject" className="text-sm font-medium">Custom subject (optional)</label>
          <Input
            id="monitor-custom-subject"
            value={customSubject}
            onChange={(e) => setCustomSubject(e.target.value)}
            placeholder="e.g. {{name}} is {{state}}"
            disabled={!canSave}
          />
          <p className="mt-1 text-xs text-muted-foreground">
            Used in email and chat alerts (not webhooks, PagerDuty or Datadog). Leave blank to use the default subject. Use {"{{name}}"} and {"{{state}}"} as
            placeholders.
          </p>
        </div>

        <div>
          <label htmlFor="monitor-custom-body" className="text-sm font-medium">Custom body (optional)</label>
          <Textarea
            id="monitor-custom-body"
            value={customBody}
            onChange={(e) => setCustomBody(e.target.value)}
            placeholder="e.g. {{name}} transitioned to {{state}}."
            disabled={!canSave}
          />
          <p className="mt-1 text-xs text-muted-foreground">
            Used in email and chat alerts (not webhooks, PagerDuty or Datadog). Leave blank to use the default body. Use {"{{name}}"} and {"{{state}}"} as
            placeholders.
          </p>
        </div>

        {isNew && (
          <div>
            <label htmlFor="monitor-notify-toggle" className="text-sm font-medium">Notify</label>
            <div className="mt-1 flex items-center gap-3">
              <Switch
                id="monitor-notify-toggle"
                checked={notifyOnCreate}
                onCheckedChange={setNotifyOnCreate}
                disabled={!canSave}
              />
              <span className="text-sm text-muted-foreground">Subscribe me when this monitor triggers</span>
            </div>
            {notifyOnCreate && (
              <Select value={destChoice} onValueChange={setDestChoice} disabled={!canSave}>
                <SelectTrigger className="mt-2 w-[240px]" data-testid="monitor-create-destination-select">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="email">Email (just me)</SelectItem>
                  {destinationOptions}
                </SelectContent>
              </Select>
            )}
          </div>
        )}

        {conflict && (
          <div
            role="status"
            data-testid="monitor-conflict-banner"
            className="flex items-center justify-between gap-3 rounded-md border border-border bg-muted/50 px-3 py-2 text-sm"
          >
            <span>
              This monitor's settings changed since you opened it. Reload to get the latest — your
              unsaved edits here will be replaced.
            </span>
            <Button variant="outline" size="sm" onClick={handleReload}>
              Reload
            </Button>
          </div>
        )}

        {saveError && (
          <p className="text-sm text-destructive">
            {saveError}
            {draftPartial?.queryId != null &&
              ` The query "${draftQueryName.trim()}" was created — saving again picks up where it stopped.`}
          </p>
        )}

        <div className="flex gap-2">
          {canSave && (
            <Button
              onClick={() => { setSaveError(null); saveMutation.mutate(); }}
              disabled={saveMutation.isPending || saveDisabledReasons.length > 0}
              title={saveDisabledReasons.length > 0 ? `Enter ${saveDisabledReasons.join(", ")} to save.` : undefined}
            >
              {saveMutation.isPending ? "Saving…" : "Save"}
            </Button>
          )}
        </div>
      </div>

      <ConfirmDialog
        open={deleteConfirmOpen}
        onOpenChange={(open) => {
          setDeleteConfirmOpen(open);
          if (!open) setDeleteError(null);
        }}
        title="Delete this monitor?"
        confirmLabel="Delete"
        destructive
        confirmPending={deleteMutation.isPending}
        onConfirm={() => deleteMutation.mutate()}
        error={deleteError}
      />

      {!isNew && monitorQuery.data && (
        <div id="monitor-schedule">
          <ScheduleSection
            entityId={Number(id)}
            flavor={MONITOR_FLAVOR}
            canEdit={monitorQuery.data.caller.can_edit}
            expanded={scheduleExpanded}
            onExpandedChange={setScheduleExpanded}
            focusRunId={focusRunId}
            readsQuery={monitorQuery.data.query}
            monitorControls={monitorControls}
            maxFreshWaitSecs={maxFreshWaitSecs}
          />
        </div>
      )}

      {!isNew && (
        <div className="space-y-3 pt-4 border-t">
          <div className="flex items-center justify-between">
            <h2 className="text-lg font-medium">Subscriptions</h2>
            <div className="flex items-center gap-2">
              <Select value={destChoice} onValueChange={setDestChoice}>
                <SelectTrigger className="w-[200px]" data-testid="monitor-subscribe-destination-select"><SelectValue /></SelectTrigger>
                <SelectContent>
                  <SelectItem value="email">Email (just me)</SelectItem>
                  {destinationOptions}
                </SelectContent>
              </Select>
              <Button
                size="sm"
                onClick={() => subscribeMutation.mutate()}
                disabled={subscribeMutation.isPending}
              >
                {subscribeMutation.isPending ? (
                  <>
                    <Loader size={16} />
                    Subscribing…
                  </>
                ) : (
                  "Subscribe"
                )}
              </Button>
            </div>
          </div>
          {subscriptionFeedback && <p className="text-sm text-destructive">{subscriptionFeedback}</p>}
          {monitorQuery.data?.state === "triggered" && !isSilenced && (
            <div className="rounded-md border border-warning/30 bg-warning/10 px-3 py-2 text-sm text-warning-foreground">
              This monitor is currently alerting. Anyone you subscribe is notified right away.
            </div>
          )}
          {destinationsQuery.data?.length === 0 && (
            <p className="text-sm text-muted-foreground">
              Want Slack, webhook, or PagerDuty?{" "}
              <Link to="/destinations" className="underline">
                Add a destination
              </Link>
            </p>
          )}
          {subsQuery.isLoading && <Loader size={16} announce />}
          {subsQuery.data && subsQuery.data.length === 0 && (
            <EmptyState
              icon={Bell}
              iconClassName="bg-warning/10 text-warning"
              title="No subscriptions yet"
              description="Subscribe a person or destination above to get notified when this monitor triggers."
            />
          )}
          {subsQuery.data && subsQuery.data.length > 0 && (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>User</TableHead>
                  <TableHead>Channel</TableHead>
                  <TableHead></TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {subsQuery.data.map((sub) => (
                  <TableRow key={sub.id}>
                    <TableCell>{sub.user.email}</TableCell>
                    <TableCell>{sub.destination ? `${sub.destination.type}: ${sub.destination.name}` : "Email"}</TableCell>
                    <TableCell>
                      <div className="flex items-center gap-2">
                        <Button
                          size="sm"
                          variant="ghost"
                          title="Send this subscription a notification now"
                          disabled={notifyMutation.isPending && notifyMutation.variables === sub.id}
                          onClick={() => notifyMutation.mutate(sub.id)}
                        >
                          {notifyMutation.isPending && notifyMutation.variables === sub.id
                            ? "Sending…"
                            : "Send now"}
                        </Button>
                        <Button
                          size="sm"
                          variant="ghost"
                          disabled={unsubMutation.isPending && unsubMutation.variables === sub.id}
                          onClick={() => unsubMutation.mutate(sub.id)}
                        >
                          {unsubMutation.isPending && unsubMutation.variables === sub.id ? "Removing…" : "Remove"}
                        </Button>
                      </div>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
          <FeedbackLine feedback={notifyFeedback} />
        </div>
      )}

      {!isNew && (
        <div className="space-y-3 pt-4 border-t">
          <h2 className="text-lg font-medium">Escalation</h2>
          {steps.length === 0 && (
            <EmptyState
              icon={Workflow}
              iconClassName="bg-warning/10 text-warning"
              title="No escalation steps"
              description="Add a step to notify someone else if this monitor stays alerting and no one acknowledges it."
            />
          )}
          {steps.map((step, i) => (
            <div key={i} className="flex flex-wrap items-center gap-2 rounded-md border px-3 py-2">
              <span className="text-sm text-muted-foreground">After</span>
              <Input
                type="number"
                min={1}
                className="w-20"
                value={step.delay_minutes}
                onChange={(e) => updateStep(i, { delay_minutes: Number(e.target.value) })}
                disabled={!canSave}
              />
              <span className="text-sm text-muted-foreground">
                {i === 0 ? "min from when this monitor triggers, notify" : "min from the previous step, notify"}
              </span>
              <Select
                value={step.destination_id ? String(step.destination_id) : ""}
                onValueChange={(v) => updateStep(i, { destination_id: Number(v) })}
                disabled={!canSave}
              >
                <SelectTrigger className="w-[200px]" data-testid={`monitor-escalation-destination-select-${i}`}>
                  <SelectValue placeholder="Select a destination" />
                </SelectTrigger>
                <SelectContent>
                  {destinationOptions}
                </SelectContent>
              </Select>
              <div className="ml-auto flex items-center gap-1">
                <Button
                  size="sm"
                  variant="ghost"
                  aria-label="Move step up"
                  disabled={i === 0 || !canSave}
                  onClick={() => moveStep(i, -1)}
                >
                  <ArrowUp size={16} />
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  aria-label="Move step down"
                  disabled={i === steps.length - 1 || !canSave}
                  onClick={() => moveStep(i, 1)}
                >
                  <ArrowDown size={16} />
                </Button>
                <Button size="sm" variant="ghost" disabled={!canSave} onClick={() => removeStep(i)}>
                  Remove
                </Button>
              </div>
            </div>
          ))}
          <Button
            size="sm"
            variant="outline"
            onClick={addStep}
            disabled={!canSave || destinationsQuery.data?.length === 0}
            title={
              destinationsQuery.data?.length === 0
                ? "Add a destination first"
                : !canSave
                  ? "You don't have permission to edit this monitor"
                  : undefined
            }
          >
            + Add step
          </Button>
          <div className="flex items-center gap-2">
            {canSave && (
              <Button
                size="sm"
                onClick={() =>
                  saveEscalation.mutate(
                    steps.map((s) => ({ delay_minutes: s.delay_minutes, destination_id: s.destination_id as number })),
                  )
                }
                disabled={saveEscalation.isPending || !escalationValid}
              >
                {saveEscalation.isPending ? "Saving…" : "Save escalation steps"}
              </Button>
            )}
            <FeedbackLine feedback={escalationFeedback} as="span" />
          </div>
        </div>
      )}

      {!isNew && (
        <div className="space-y-3 pt-4 border-t">
          <h2 className="text-lg font-medium">Delivery history</h2>
          {deliveriesQuery.isLoading && <Loader size={16} announce />}
          {deliveriesQuery.isError && (
            <p className="text-sm text-destructive">Couldn't load delivery history.</p>
          )}
          {deliveriesQuery.data && deliveriesQuery.data.length === 0 && (
            <p className="text-sm text-muted-foreground">No notifications sent yet.</p>
          )}
          {deliveriesQuery.data && deliveriesQuery.data.length > 0 && (
            <div className="max-h-96 overflow-y-auto rounded-md border">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Time</TableHead>
                    <TableHead>State</TableHead>
                    <TableHead>Channel</TableHead>
                    <TableHead>Delivery</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {deliveriesQuery.data.map((d) => (
                    <TableRow key={d.id}>
                      <TableCell className="text-muted-foreground">
                        {exactTs(Date.parse(d.created_at))}
                      </TableCell>
                      <TableCell>
                        <StateBadge state={d.state} />
                      </TableCell>
                      <TableCell>{resolveChannel(d.destination_id)}</TableCell>
                      <TableCell>
                        <DeliveryStatusBadge status={d.status} />
                        {d.attempt_number > 1 && (
                          <span className="ml-2 text-xs text-muted-foreground">
                            attempt {d.attempt_number}
                          </span>
                        )}
                        {d.error && (
                          <p
                            className="mt-1 max-w-[16rem] truncate text-xs text-destructive"
                            title={d.error}
                          >
                            {d.error}
                          </p>
                        )}
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </div>
          )}
        </div>
      )}

      {!isNew && canDelete && (
        <div className="space-y-3 pt-4 border-t">
          <h2 className="text-lg font-medium text-destructive">Danger zone</h2>
          <p className="text-sm text-muted-foreground">
            Deleting a monitor removes its subscriptions, escalation steps, and delivery history.
            This can&apos;t be undone.
          </p>
          <Button
            variant="destructive"
            onClick={() => setDeleteConfirmOpen(true)}
            disabled={deleteMutation.isPending}
          >
            Delete monitor
          </Button>
        </div>
      )}
    </div>
  );
}
