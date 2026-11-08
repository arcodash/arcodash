import React, { useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { RefreshCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { api } from "@/lib/api";
import type { ParameterType, QueryParameterDefinition } from "@/types/api";

/** Detects `{{ name }}`/`{{ name.start }}`/`{{ name.end }}` tokens in SQL, deduped by base name.
 * Mirrors the grammar in `apps/backend/src/services/parameters.rs`; the backend validates at
 * render time. */
const TOKEN_RE = /\{\{\s*([A-Za-z0-9_-]+)(?:\.(start|end))?\s*\}\}/g;

export function detectParameterNames(sql: string): string[] {
  return [...new Set(Array.from(sql.matchAll(TOKEN_RE), (m) => m[1]))];
}

/** Date/timestamp values substitute bare (see `render_date`/`render_range` in
 * `apps/backend/src/services/parameters.rs`), so SQL must quote and cast them or Postgres fails
 * with a confusing arithmetic error. */
function dateTokenHint(type: ParameterType, name: string): string | null {
  switch (type) {
    case "date":
      return `'{{ ${name} }}'::date`;
    case "date-range":
    case "datetime-range":
      return `BETWEEN '{{ ${name}.start }}' AND '{{ ${name}.end }}'`;
    default:
      return null;
  }
}

const PARAMETER_TYPES: { value: ParameterType; label: string }[] = [
  { value: "text", label: "Text" },
  { value: "number", label: "Number" },
  { value: "date", label: "Date" },
  { value: "date-range", label: "Date Range" },
  { value: "datetime-range", label: "Date/Time Range" },
  { value: "dropdown", label: "Dropdown" },
  { value: "query", label: "Query-based Dropdown" },
];

interface ParametersBarProps {
  sql: string;
  definitions: QueryParameterDefinition[];
  values: Record<string, unknown>;
  onValuesChange: (values: Record<string, unknown>) => void;
  /** Called when a definition is added/edited; a rejected promise means the save failed and the
   * caller keeps its form open. */
  onDefinitionsChange: (definitions: QueryParameterDefinition[]) => void | Promise<void>;
  /** Saved query id, needed for query-based dropdown options; `null` for never-saved queries, whose
   * options can't resolve until saved. */
  queryId: number | null;
}

export function ParametersBar({
  sql,
  definitions,
  values,
  onValuesChange,
  onDefinitionsChange,
  queryId,
}: ParametersBarProps) {
  const detectedNames = useMemo(() => detectParameterNames(sql), [sql]);

  if (detectedNames.length === 0) {
    return null;
  }

  return (
    <div className="flex flex-wrap items-end gap-3 rounded-md border border-border bg-muted/30 p-3">
      {detectedNames.map((name) => {
        const definition = definitions.find((d) => d.name === name);
        if (!definition) {
          return (
            <NewParameterConfigurator
              key={name}
              name={name}
              onSave={(def) => onDefinitionsChange([...definitions, def])}
            />
          );
        }
        return (
          <ParameterControl
            key={name}
            definition={definition}
            value={values[name]}
            onChange={(v) => onValuesChange({ ...values, [name]: v })}
            queryId={queryId}
            testId={`param-${name}`}
          />
        );
      })}
    </div>
  );
}

function NewParameterConfigurator({
  name,
  onSave,
}: {
  name: string;
  onSave: (definition: QueryParameterDefinition) => void | Promise<void>;
}) {
  const [type, setType] = useState<ParameterType>("text");
  const [title, setTitle] = useState(name);
  const [enumOptions, setEnumOptions] = useState("");
  const [queryId, setQueryId] = useState("");
  const [allowMultiple, setAllowMultiple] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  if (saved) return null;
  const hint = dateTokenHint(type, name);

  async function handleSave() {
    const definition: QueryParameterDefinition = {
      name,
      title: title || name,
      type,
      ...(type === "dropdown" ? { enum_options: enumOptions } : {}),
      ...(type === "query" && queryId ? { query_id: Number(queryId) } : {}),
      ...((type === "dropdown" || type === "query") && allowMultiple
        ? { multi_values_options: { prefix: "'", suffix: "'", separator: "," } }
        : {}),
    };
    setSaving(true);
    setSaveError(null);
    try {
      await onSave(definition);
      setSaved(true);
    } catch {
      // Left mounted with typed inputs intact: the definition never saved server-side, so hiding
      // would lose it.
      setSaving(false);
      setSaveError("Couldn't save this parameter. Try again.");
    }
  }

  return (
    <div
      className="flex flex-col gap-2 rounded-md border border-dashed border-border p-2"
      data-testid={`param-${name}`}
    >
      <p className="text-xs text-muted-foreground">
        New parameter detected: <code>{`{{ ${name} }}`}</code> — configure it
      </p>
      <div className="flex flex-wrap items-end gap-2">
        <div className="flex flex-col gap-1">
          <Label className="text-xs">Title</Label>
          <Input
            className="h-8 w-32"
            value={title}
            onChange={(e) => setTitle(e.target.value)}
          />
        </div>
        <div className="flex flex-col gap-1">
          <Label className="text-xs">Type</Label>
          <Select value={type} onValueChange={(v) => setType(v as ParameterType)}>
            <SelectTrigger className="h-8 w-44">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {PARAMETER_TYPES.map((t) => (
                <SelectItem key={t.value} value={t.value}>
                  {t.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        {type === "dropdown" && (
          <div className="flex flex-col gap-1">
            <Label className="text-xs">Options (one per line)</Label>
            <textarea
              className="h-8 w-full max-w-40 rounded-md border border-input bg-transparent px-2 py-1 text-sm"
              value={enumOptions}
              onChange={(e) => setEnumOptions(e.target.value)}
            />
          </div>
        )}
        {type === "query" && (
          <div className="flex flex-col gap-1">
            <Label className="text-xs">Source query id</Label>
            <Input
              className="h-8 w-24"
              value={queryId}
              onChange={(e) => setQueryId(e.target.value)}
              placeholder="e.g. 12"
            />
          </div>
        )}
        {(type === "dropdown" || type === "query") && (
          <label className="flex items-center gap-1 text-xs text-muted-foreground">
            <input
              type="checkbox"
              checked={allowMultiple}
              onChange={(e) => setAllowMultiple(e.target.checked)}
            />
            Allow multiple values
          </label>
        )}
        <Button size="sm" onClick={handleSave} disabled={saving}>
          {saving ? "Saving…" : "Add parameter"}
        </Button>
      </div>
      {hint && (
        <p className="text-xs text-muted-foreground">
          Substitutes unquoted — wrap it in SQL: <code>{hint}</code>
        </p>
      )}
      {saveError && <p className="text-xs text-destructive">{saveError}</p>}
    </div>
  );
}

export function ParameterControl({
  definition,
  value,
  onChange,
  queryId,
  testId,
}: {
  definition: QueryParameterDefinition;
  value: unknown;
  onChange: (value: unknown) => void;
  queryId: number | null;
  testId?: string;
}) {
  const isMulti = Boolean(definition.multi_values_options);

  let inner: React.ReactNode;
  switch (definition.type) {
    case "text":
    case "date":
      inner = (
        <LabeledInput
          title={definition.title}
          type={definition.type}
          value={typeof value === "string" ? value : ""}
          onChange={onChange}
        />
      );
      break;
    case "number":
      inner = <NumberInput title={definition.title} value={value} onChange={onChange} />;
      break;
    case "date-range":
    case "datetime-range":
      inner = <DateRangeControl definition={definition} value={value} onChange={onChange} />;
      break;
    case "dropdown":
      inner = (
        <DropdownControl
          definition={definition}
          value={value}
          onChange={onChange}
          isMulti={isMulti}
          options={(definition.enum_options ?? "").split("\n").filter((s) => s.length > 0)}
        />
      );
      break;
    case "query":
      inner = (
        <QueryDropdownControl
          definition={definition}
          value={value}
          onChange={onChange}
          isMulti={isMulti}
          queryId={queryId}
        />
      );
      break;
    default:
      return null;
  }

  return <div data-testid={testId}>{inner}</div>;
}

/** A runtime control's caption above its input. */
function Field({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-1">
      <Label className="text-xs text-muted-foreground">{title}</Label>
      {children}
    </div>
  );
}

function LabeledInput({
  title,
  value,
  onChange,
  type,
}: {
  title: string;
  value: string;
  onChange: (value: string) => void;
  type: string;
}) {
  return (
    <Field title={title}>
      <Input className="h-8 w-40" type={type} value={value} onChange={(e) => onChange(e.target.value)} />
    </Field>
  );
}

/** A half-typed non-number ("1e", "-") coerces to `NaN` and would be substituted into the SQL;
 * treat anything non-finite as empty. */
export function parseNumberParam(raw: string): number | "" {
  if (raw === "") return "";
  const n = Number(raw);
  return Number.isFinite(n) ? n : "";
}

/** Keeps the raw text on screen (marked invalid) while typing; the committed value is empty until
 * it parses. */
function NumberInput({
  title,
  value,
  onChange,
}: {
  title: string;
  value: unknown;
  onChange: (value: number | "") => void;
}) {
  const [text, setText] = useState(value == null ? "" : String(value));
  const [invalid, setInvalid] = useState(false);

  useEffect(() => {
    if (invalid) return;
    setText(value == null ? "" : String(value));
  }, [value, invalid]);

  function handleChange(raw: string) {
    setText(raw);
    const parsed = parseNumberParam(raw);
    setInvalid(raw !== "" && parsed === "");
    onChange(parsed);
  }

  return (
    <Field title={title}>
      <Input
        className="h-8 w-40"
        type="number"
        value={text}
        aria-invalid={invalid}
        onChange={(e) => handleChange(e.target.value)}
      />
      {invalid && <p className="text-xs text-destructive">Enter a number</p>}
    </Field>
  );
}

function DateRangeControl({
  definition,
  value,
  onChange,
}: {
  definition: QueryParameterDefinition;
  value: unknown;
  onChange: (value: unknown) => void;
}) {
  const range = (value ?? {}) as { start?: string; end?: string };
  const dateType = definition.type === "datetime-range" ? "datetime-local" : "date";

  return (
    <Field title={definition.title}>
      <div className="flex items-center gap-1">
        <Input
          className="h-8 w-36"
          type={dateType}
          value={range.start ?? ""}
          onChange={(e) => onChange({ ...range, start: e.target.value })}
        />
        <span className="text-xs text-muted-foreground">to</span>
        <Input
          className="h-8 w-36"
          type={dateType}
          value={range.end ?? ""}
          onChange={(e) => onChange({ ...range, end: e.target.value })}
        />
      </div>
    </Field>
  );
}

function DropdownControl({
  definition,
  value,
  onChange,
  isMulti,
  options,
}: {
  definition: QueryParameterDefinition;
  value: unknown;
  onChange: (value: unknown) => void;
  isMulti: boolean;
  options: string[];
}) {
  if (isMulti) {
    const selected = Array.isArray(value) ? (value as string[]) : [];
    return (
      <MultiSelectControl
        title={definition.title}
        options={options.map((o) => ({ name: o, value: o }))}
        selected={selected}
        onChange={onChange}
      />
    );
  }

  return (
    <Field title={definition.title}>
      <Select value={typeof value === "string" ? value : ""} onValueChange={onChange}>
        <SelectTrigger className="h-8 w-40">
          <SelectValue placeholder="Select…" />
        </SelectTrigger>
        <SelectContent>
          {options.map((opt) => (
            <SelectItem key={opt} value={opt}>
              {opt}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </Field>
  );
}

// Options exist only once the source query has a cached result; a query that never finishes must
// not poll forever, so this ladder has a ceiling.
const OPTIONS_POLL_MAX_ATTEMPTS = 9;

/** The options endpoint returns a `job` while the source query is still running. */
function hasPendingJob(data: object | undefined): boolean {
  return data != null && "job" in data && !!data.job;
}

function QueryDropdownControl({
  definition,
  value,
  onChange,
  isMulti,
  queryId,
}: {
  definition: QueryParameterDefinition;
  value: unknown;
  onChange: (value: unknown) => void;
  isMulti: boolean;
  queryId: number | null;
}) {
  // Track poll attempts here (react-query doesn't expose dataUpdateCount on the hook result). Reset
  // during render, not in an effect:
  // an effect would discount the mount fetch and allow one extra attempt past
  // OPTIONS_POLL_MAX_ATTEMPTS.
  const pollAttemptRef = useRef(0);
  const pollKeyRef = useRef<string | null>(null);
  const pollKey = `${queryId}:${definition.name}`;
  if (pollKeyRef.current !== pollKey) {
    pollKeyRef.current = pollKey;
    pollAttemptRef.current = 0;
  }

  const optionsQuery = useQuery({
    queryKey: ["parameter-values", queryId, definition.name],
    queryFn: () => {
      pollAttemptRef.current += 1;
      return api.getParameterValues(queryId!, definition.name);
    },
    enabled: queryId != null,
    // Step the poll interval up the longer the source query runs (1s → 2s → 5s), then stop.
    refetchInterval: (q) => {
      if (!hasPendingJob(q.state.data)) return false;
      const attempt = pollAttemptRef.current;
      if (attempt < 3) return 1000;
      if (attempt < 6) return 2000;
      if (attempt < OPTIONS_POLL_MAX_ATTEMPTS) return 5000;
      return false;
    },
  });

  const pending = hasPendingJob(optionsQuery.data);
  const pollGaveUp = pending && !optionsQuery.isFetching && pollAttemptRef.current >= OPTIONS_POLL_MAX_ATTEMPTS;

  const options = useMemo(() => {
    const data = optionsQuery.data;
    return data && "values" in data && data.values ? data.values : [];
  }, [optionsQuery.data]);

  // Coerce numeric values back to strings for <select>/checkbox value attributes; onChange still
  // uses the option's original `value`.
  const optionsByKey = useMemo(
    () => new Map(options.map((o) => [String(o.value), o.value])),
    [options],
  );

  // A disabled outline button standing in for the select, with a retry action when there is one.
  const placeholder = (text: string, tone: string, onRetry?: () => void) => {
    const stub = (
      <Button type="button" variant="outline" disabled className={`h-8 w-40 justify-start font-normal ${tone}`}>
        {text}
      </Button>
    );
    return (
      <Field title={definition.title}>
        {onRetry ? (
          <div className="flex items-center gap-1">
            {stub}
            <Button
              type="button"
              variant="ghost"
              size="icon-sm"
              disabled={optionsQuery.isFetching}
              onClick={onRetry}
              title="Retry"
            >
              <RefreshCw className="h-3.5 w-3.5" />
            </Button>
          </div>
        ) : (
          stub
        )}
      </Field>
    );
  };

  if (queryId == null) {
    return (
      <Field title={definition.title}>
        <p className="text-xs text-muted-foreground">Save the query to load options</p>
      </Field>
    );
  }
  if (optionsQuery.isLoading || (pending && !pollGaveUp)) return placeholder("Loading…", "text-muted-foreground");
  if (pollGaveUp) {
    return placeholder("Options not ready", "text-muted-foreground", () => {
      pollAttemptRef.current = 0;
      optionsQuery.refetch();
    });
  }
  if (optionsQuery.isError) {
    return placeholder("Couldn't load options", "text-destructive", () => optionsQuery.refetch());
  }

  if (isMulti) {
    const selected = Array.isArray(value) ? (value as unknown[]).map(String) : [];
    return (
      <MultiSelectControl
        title={definition.title}
        options={options.map((o) => ({ name: o.name, value: String(o.value) }))}
        selected={selected}
        onChange={(keys) => onChange((keys as string[]).map((k) => optionsByKey.get(k) ?? k))}
      />
    );
  }

  return (
    <Field title={definition.title}>
      <Select value={value == null ? "" : String(value)} onValueChange={(key) => onChange(optionsByKey.get(key) ?? key)}>
        <SelectTrigger className="h-8 w-40">
          <SelectValue placeholder="Select…" />
        </SelectTrigger>
        <SelectContent>
          {options.map((opt) => (
            <SelectItem key={String(opt.value)} value={String(opt.value)}>
              {opt.name}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </Field>
  );
}

function sameValues(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((v, i) => v === b[i]);
}

/** Checkbox list behind a collapsed trigger; edits stay local while open and commit on close, so
 * multi-select doesn't refetch on every click. */
function MultiSelectControl({
  title,
  options,
  selected,
  onChange,
}: {
  title: string;
  options: { name: string; value: string }[];
  selected: string[];
  onChange: (selected: string[]) => void;
}) {
  const [open, setOpen] = useState(false);
  const [staged, setStaged] = useState(selected);

  function handleOpenChange(next: boolean) {
    if (next) {
      setStaged(selected);
    } else if (!sameValues(staged, selected)) {
      onChange(staged);
    }
    setOpen(next);
  }

  function toggle(value: string) {
    setStaged((prev) => (prev.includes(value) ? prev.filter((v) => v !== value) : [...prev, value]));
  }

  return (
    <Field title={title}>
      <Popover open={open} onOpenChange={handleOpenChange}>
        <PopoverTrigger asChild>
          <Button
            type="button"
            variant="outline"
            className="h-8 w-40 justify-start font-normal"
          >
            {selected.length > 0 ? `${selected.length} selected` : "Select…"}
          </Button>
        </PopoverTrigger>
        <PopoverContent className="w-56 p-2" align="start">
          <div className="flex max-h-56 flex-col gap-1 overflow-auto">
            {options.map((opt) => (
              <label key={opt.value} className="flex items-center gap-1.5 text-xs">
                <input
                  type="checkbox"
                  checked={staged.includes(opt.value)}
                  onChange={() => toggle(opt.value)}
                />
                {opt.name}
              </label>
            ))}
            {options.length === 0 && (
              <span className="text-xs text-muted-foreground">No options</span>
            )}
          </div>
        </PopoverContent>
      </Popover>
    </Field>
  );
}

/** Seeds values from each definition's default `value` for names without a runtime value. */
export function useDefaultParameterValues(
  definitions: QueryParameterDefinition[],
  values: Record<string, unknown>,
  onValuesChange: (values: Record<string, unknown>) => void,
): void {
  useEffect(() => {
    const missing = definitions.filter((d) => d.value !== undefined && !(d.name in values));
    if (missing.length === 0) return;
    onValuesChange({ ...values, ...Object.fromEntries(missing.map((d) => [d.name, d.value])) });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [definitions]);
}
