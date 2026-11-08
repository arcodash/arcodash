import { useRef, useState } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { CheckCircle, Edit, Eye, EyeOff, Send, Trash2, XCircle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { RequiredMark, FieldError } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { PageLoader, Loader } from "@/components/ui/loader";
import { PageHeader } from "@/components/common/PageHeader";
import { ReadonlyField } from "@/components/common/ReadonlyField";
import { EmptyState } from "@/components/common/EmptyState";
import { MonitorsTabs } from "@/components/monitors/MonitorsTabs";
import { DestinationTestButton } from "@/components/destinations/DestinationTestButton";
import { apiErrorMessage, api } from "@/lib/api";
import { fieldKind, validateValue } from "@/lib/formValidation";
import { usePermissions } from "@/lib/permissions";
import { useDocumentTitle } from "@/lib/useDocumentTitle";
import type { Destination, DestinationType } from "@/types/api";

const nameError = (value: string) => validateValue("Name", true, undefined, value);

function fieldError(type: DestinationType | undefined, field: string, value: string) {
  const schema = type?.configuration_schema[field];
  return validateValue(
    schema?.title ?? field,
    (type?.required ?? []).includes(field),
    fieldKind(field, schema?.type),
    value,
  );
}

/** Validate-on-submit for the New and Edit dialogs; blank secrets on edit ("leave blank to keep
 * current") are never flagged. */
function validateDestinationFields(
  name: string,
  type: DestinationType | undefined,
  opts: Record<string, string>,
  isEdit: boolean,
): Record<string, string> {
  const errors: Record<string, string> = {};
  const nameErr = nameError(name);
  if (nameErr) errors.name = nameErr;
  if (!type) return errors;

  const secret = new Set(type.secret ?? []);
  for (const key of Object.keys(type.configuration_schema)) {
    const value = opts[key] ?? "";
    if (isEdit && secret.has(key) && value === "") continue; // keep-current, not empty
    const err = fieldError(type, key, value);
    if (err) errors[key] = err;
  }
  return errors;
}

/** Fix-as-you-type: once submit has flagged `key`, keep its message in sync with the new value. */
function revalidate(prev: Record<string, string>, key: string, message: string | null | undefined) {
  if (!(key in prev)) return prev;
  const next = { ...prev };
  if (message) next[key] = message;
  else delete next[key];
  return next;
}

/** Focuses the first flagged field; returns whether there was one. */
function focusFirstError(form: HTMLFormElement | null, errors: Record<string, string>, nameId: string) {
  const firstKey = Object.keys(errors)[0];
  if (!firstKey) return false;
  const id = firstKey === "name" ? nameId : `dest-field-${firstKey}`;
  form?.querySelector<HTMLElement>(`#${CSS.escape(id)}`)?.focus();
  return true;
}

function DestinationFields({
  type,
  opts,
  errors,
  onChange,
  isEdit,
}: {
  type: DestinationType;
  opts: Record<string, string>;
  errors: Record<string, string>;
  onChange: (field: string, value: string) => void;
  isEdit: boolean;
}) {
  const required = new Set(type.required ?? []);
  const secret = new Set(type.secret ?? []);
  // Stored secrets never round-trip to this form, so a secret in `opts` is only what the user
  // typed; safe to reveal.
  const [revealed, setRevealed] = useState<Set<string>>(new Set());
  return (
    <>
      {Object.entries(type.configuration_schema).map(([field, schema]) => {
        const isSecret = secret.has(field);
        const isRevealed = revealed.has(field);
        const fieldId = `dest-field-${field}`;
        const errorId = `${fieldId}-error`;
        return (
          <div key={field}>
            <label className="text-sm font-medium" htmlFor={fieldId}>
              {schema.title ?? field}
              {required.has(field) && <RequiredMark />}
            </label>
            <div className="relative">
              <Input
                id={fieldId}
                type={isSecret && !isRevealed ? "password" : "text"}
                autoComplete={isSecret ? "new-password" : undefined}
                placeholder={isSecret && isEdit ? "Leave blank to keep current" : undefined}
                value={opts[field] ?? ""}
                aria-invalid={errors[field] ? true : undefined}
                aria-describedby={errors[field] ? errorId : undefined}
                onChange={(e) => onChange(field, e.target.value)}
                className={isSecret ? "pr-9" : undefined}
              />
              {isSecret && (
                <button
                  type="button"
                  className="absolute inset-y-0 right-2 flex items-center text-muted-foreground hover:text-foreground"
                  aria-label={isRevealed ? `Hide ${schema.title ?? field}` : `Show ${schema.title ?? field}`}
                  onClick={() =>
                    setRevealed((prev) => {
                      const next = new Set(prev);
                      if (next.has(field)) next.delete(field);
                      else next.add(field);
                      return next;
                    })
                  }
                >
                  {isRevealed ? <EyeOff className="size-4" /> : <Eye className="size-4" />}
                </button>
              )}
            </div>
            {isSecret && isEdit && (
              <p className="mt-1 text-xs text-muted-foreground">Leave empty to keep the current value.</p>
            )}
            <FieldError id={errorId} message={errors[field]} />
          </div>
        );
      })}
    </>
  );
}

function EditDestinationDialog({
  destination,
  types,
  onClose,
}: {
  destination: Destination;
  types: DestinationType[];
  onClose: () => void;
}) {
  const queryClient = useQueryClient();
  const type = types.find((t) => t.type === destination.type);
  const secretFields = new Set(type?.secret ?? []);
  const formRef = useRef<HTMLFormElement>(null);

  const [name, setName] = useState(destination.name);
  const [opts, setOpts] = useState<Record<string, string>>(() => {
    const initial: Record<string, string> = {};
    for (const field of Object.keys(type?.configuration_schema ?? {})) {
      if (secretFields.has(field)) continue; // never prefill secrets
      const value = destination.options[field];
      if (value !== undefined && value !== null) initial[field] = String(value);
    }
    return initial;
  });
  const [touched, setTouched] = useState(false);
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);

  const updateMutation = useMutation({
    mutationFn: () => {
      const body: { name: string; options?: Record<string, unknown> } = { name };
      if (touched) {
        // Blank values are omitted: the backend merges by key, so an omitted field keeps its value
        // (a non-secret cannot be cleared via this form).
        body.options = Object.fromEntries(
          Object.entries(opts).filter(([, v]) => v !== ""),
        );
      }
      return api.updateDestination(destination.id, body);
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["destinations"] });
      onClose();
    },
    onError: (err) => setError(apiErrorMessage(err, "Failed to save")),
  });

  function handleFieldChange(field: string, value: string) {
    setTouched(true);
    setOpts((prev) => ({ ...prev, [field]: value }));
    setErrors((prev) =>
      revalidate(prev, field, secretFields.has(field) && value === "" ? null : fieldError(type, field, value)),
    );
  }

  function handleSubmit() {
    const next = validateDestinationFields(name, type, opts, true);
    setErrors(next);
    if (focusFirstError(formRef.current, next, "dest-edit-name")) return;
    setError(null);
    updateMutation.mutate();
  }

  return (
    <Dialog open onOpenChange={onClose}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Edit Destination</DialogTitle>
        </DialogHeader>
        <form
          id="edit-destination-form"
          ref={formRef}
          noValidate
          className="space-y-4 pt-2"
          onSubmit={(e) => {
            e.preventDefault();
            handleSubmit();
          }}
        >
          <div>
            <label className="text-sm font-medium" htmlFor="dest-edit-name">
              Name
              <RequiredMark />
            </label>
            <Input
              id="dest-edit-name"
              value={name}
              onChange={(e) => {
                setName(e.target.value);
                setErrors((prev) => revalidate(prev, "name", nameError(e.target.value)));
              }}
              aria-invalid={errors.name ? true : undefined}
              aria-describedby={errors.name ? "dest-edit-name-error" : undefined}
            />
            <FieldError id="dest-edit-name-error" message={errors.name} />
          </div>
          <ReadonlyField label="Type" value={type?.name ?? destination.type} />
          {type && (
            <DestinationFields type={type} opts={opts} errors={errors} onChange={handleFieldChange} isEdit />
          )}
          {error && <p className="text-sm text-destructive">{error}</p>}
        </form>
        <DialogFooter>
          <div className="mr-auto space-y-1">
            {type && (
              <DestinationTestButton target={{ id: destination.id }} destinationType={type.type} />
            )}
            {/* The stored config, not any unsaved edits above — save first to test a change. */}
            <p className="text-xs text-muted-foreground">Tests the saved configuration.</p>
          </div>
          <Button type="button" variant="outline" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" form="edit-destination-form" disabled={updateMutation.isPending}>
            {updateMutation.isPending ? "Saving…" : "Save"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function RowTestAction({ destination }: { destination: Destination }) {
  const [state, setState] = useState<
    { status: "idle" } | { status: "testing" } | { status: "success" } | { status: "error"; message: string }
  >({ status: "idle" });

  async function handleTest() {
    setState({ status: "testing" });
    try {
      await api.testDestination({ id: destination.id });
      setState({ status: "success" });
    } catch (err) {
      setState({ status: "error", message: apiErrorMessage(err, "Failed to send test message") });
    }
  }

  return (
    <span className="inline-flex items-center gap-1.5">
      <Button
        variant="ghost"
        size="sm"
        aria-label="Send test message"
        title="Send test message"
        onClick={handleTest}
        disabled={state.status === "testing"}
      >
        {state.status === "testing" ? <Loader size={14} /> : <Send className="w-3.5 h-3.5" />}
      </Button>
      {state.status === "success" && (
        <span className="flex items-center gap-1 text-xs text-success">
          <CheckCircle className="size-3" aria-hidden="true" /> Sent
        </span>
      )}
      {state.status === "error" && (
        <span className="flex items-center gap-1 text-xs text-destructive" title={state.message}>
          <XCircle className="size-3" aria-hidden="true" /> Failed
        </span>
      )}
    </span>
  );
}

export function DestinationsPage() {
  useDocumentTitle("Destinations · arcodash");
  const queryClient = useQueryClient();
  const { has, isLoading: permissionsLoading } = usePermissions();
  // Write controls must stay hidden until the session resolves, not flash then vanish.
  const canManage = !permissionsLoading && has("manage_destinations");
  const [open, setOpen] = useState(false);
  const [typeName, setTypeName] = useState("");
  const [destName, setDestName] = useState("");
  const [opts, setOpts] = useState<Record<string, string>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<Destination | null>(null);
  const [pendingDelete, setPendingDelete] = useState<Destination | null>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const createFormRef = useRef<HTMLFormElement>(null);

  function resetCreateForm() {
    setTypeName("");
    setDestName("");
    setOpts({});
    setErrors({});
    setError(null);
  }

  const destsQuery = useQuery({ queryKey: ["destinations"], queryFn: () => api.listDestinations() });
  const typesQuery = useQuery({ queryKey: ["dest-types"], queryFn: () => api.listDestinationTypes() });

  const selectedType = typesQuery.data?.find((t) => t.type === typeName);

  const createMutation = useMutation({
    mutationFn: () =>
      api.createDestination({ name: destName, type: typeName, options: opts }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["destinations"] });
      setOpen(false);
      resetCreateForm();
    },
    onError: (err) => setError(apiErrorMessage(err, "Failed to create")),
  });

  const deleteMutation = useMutation({
    mutationFn: (id: number) => api.deleteDestination(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["destinations"] });
      setPendingDelete(null);
    },
    onError: (err) => setDeleteError(apiErrorMessage(err, "Failed to delete")),
  });

  function handleCreateFieldChange(field: string, value: string) {
    setOpts((prev) => ({ ...prev, [field]: value }));
    setErrors((prev) => revalidate(prev, field, fieldError(selectedType, field, value)));
  }

  function handleCreateSubmit() {
    const next = validateDestinationFields(destName, selectedType, opts, false);
    setErrors(next);
    if (focusFirstError(createFormRef.current, next, "dest-new-name")) return;
    setError(null);
    createMutation.mutate();
  }

  return (
    <div className="space-y-6">
      <PageHeader
        title="Notification destinations"
        description="Shared across your organization — any monitor can send alerts to these channels."
        action={
          canManage && (
          <Dialog
            open={open}
            onOpenChange={(o) => {
              setOpen(o);
              if (!o) resetCreateForm();
            }}
          >
            <DialogTrigger asChild>
              <Button>+ New Destination</Button>
            </DialogTrigger>
            <DialogContent>
              <DialogHeader>
                <DialogTitle>New Notification Destination</DialogTitle>
              </DialogHeader>
              <form
                id="create-destination-form"
                ref={createFormRef}
                noValidate
                className="space-y-4 pt-2"
                onSubmit={(e) => {
                  e.preventDefault();
                  handleCreateSubmit();
                }}
              >
                <div>
                  <label className="text-sm font-medium" htmlFor="dest-new-name">
                    Name
                    <RequiredMark />
                  </label>
                  <Input
                    id="dest-new-name"
                    value={destName}
                    placeholder="My Slack channel"
                    onChange={(e) => {
                      setDestName(e.target.value);
                      setErrors((prev) => revalidate(prev, "name", nameError(e.target.value)));
                    }}
                    aria-invalid={errors.name ? true : undefined}
                    aria-describedby={errors.name ? "dest-new-name-error" : undefined}
                  />
                  <FieldError id="dest-new-name-error" message={errors.name} />
                </div>
                <div>
                  <label className="text-sm font-medium" htmlFor="dest-new-type">Type</label>
                  {typesQuery.data && (
                    <Select value={typeName} onValueChange={(v) => { setTypeName(v); setOpts({}); setErrors({}); }}>
                      <SelectTrigger id="dest-new-type"><SelectValue placeholder="Select type" /></SelectTrigger>
                      <SelectContent>
                        {typesQuery.data.map((t) => (
                          <SelectItem key={t.type} value={t.type}>{t.name}</SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                  )}
                </div>
                {selectedType && (
                  <DestinationFields
                    type={selectedType}
                    opts={opts}
                    errors={errors}
                    onChange={handleCreateFieldChange}
                    isEdit={false}
                  />
                )}
                {error && <p className="text-sm text-destructive">{error}</p>}
              </form>
              <DialogFooter>
                <div className="mr-auto">
                  {selectedType && (
                    <DestinationTestButton
                      target={{ type: typeName, options: opts }}
                      disabled={!typeName}
                    />
                  )}
                </div>
                <Button
                  type="submit"
                  form="create-destination-form"
                  disabled={createMutation.isPending}
                >
                  {createMutation.isPending ? "Creating…" : "Create"}
                </Button>
              </DialogFooter>
            </DialogContent>
          </Dialog>
          )
        }
      />
      <MonitorsTabs />

      {destsQuery.isLoading && <PageLoader />}
      {destsQuery.isError && (
        <p className="text-destructive">{apiErrorMessage(destsQuery.error, "Failed to load")}</p>
      )}
      {destsQuery.data && destsQuery.data.length === 0 && canManage && (
        <EmptyState
          icon={Send}
          iconClassName="bg-emerald-50 text-emerald-600"
          title="Add your first destination"
          description="Choose where monitor alerts get delivered so the right people hear about problems fast."
          action={<Button onClick={() => setOpen(true)}>+ New Destination</Button>}
        />
      )}
      {destsQuery.data && destsQuery.data.length === 0 && !canManage && (
        <p className="text-sm text-muted-foreground">No notification destinations have been set up yet.</p>
      )}
      {destsQuery.data && destsQuery.data.length > 0 && (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Name</TableHead>
              <TableHead>Type</TableHead>
              <TableHead>Target</TableHead>
              {canManage && <TableHead>Actions</TableHead>}
            </TableRow>
          </TableHeader>
          <TableBody>
            {destsQuery.data.map((d) => {
              const typeName = typesQuery.data?.find((t) => t.type === d.type)?.name ?? d.type;
              return (
                <TableRow key={d.id}>
                  <TableCell>{d.name}</TableCell>
                  <TableCell>{typeName}</TableCell>
                  <TableCell className="text-muted-foreground truncate" title={d.target ?? undefined}>
                    {d.target ?? "—"}
                  </TableCell>
                  {canManage && (
                    <TableCell>
                      <div className="flex items-center gap-1">
                        <RowTestAction destination={d} />
                        <Button
                          variant="ghost"
                          size="sm"
                          aria-label={`Edit ${d.name}`}
                          onClick={() => setEditing(d)}
                        >
                          <Edit className="h-4 w-4" />
                        </Button>
                        <Button
                          variant="ghost"
                          size="sm"
                          className="text-destructive"
                          aria-label={`Delete ${d.name}`}
                          onClick={() => { setDeleteError(null); setPendingDelete(d); }}
                        >
                          <Trash2 className="h-4 w-4" />
                        </Button>
                      </div>
                    </TableCell>
                  )}
                </TableRow>
              );
            })}
          </TableBody>
        </Table>
      )}

      {editing && (
        <EditDestinationDialog
          key={editing.id}
          destination={editing}
          types={typesQuery.data ?? []}
          onClose={() => setEditing(null)}
        />
      )}

      <ConfirmDialog
        open={pendingDelete !== null}
        onOpenChange={(open) => !open && setPendingDelete(null)}
        title="Delete destination?"
        body={
          <p className="text-sm text-muted-foreground">
            This will permanently delete "{pendingDelete?.name}". Monitors that notify this
            destination will stop doing so — their subscriptions to it are removed.
          </p>
        }
        error={deleteError}
        destructive
        confirmLabel="Delete"
        pendingLabel="Deleting…"
        confirmPending={deleteMutation.isPending}
        allowCancelWhilePending
        onConfirm={() => pendingDelete && deleteMutation.mutate(pendingDelete.id)}
      />
    </div>
  );
}
