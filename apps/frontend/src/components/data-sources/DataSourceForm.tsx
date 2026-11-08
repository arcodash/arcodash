import { useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label, RequiredMark, FieldError } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { FileUploadField } from "@/components/data-sources/FileUploadField";
import { fieldKind, validateValue } from "@/lib/formValidation";
import type { DataSourceConfigurationSchema, DataSourceType } from "@/types/api";

export interface DataSourceFormValues {
  name: string;
  type: string;
  options: Record<string, unknown>;
}

interface DataSourceFormProps {
  types: DataSourceType[];
  /** Pre-fill for edit; secrets are never pre-filled (the backend never returns them). */
  initialName?: string;
  initialType?: string;
  initialOptions?: Record<string, unknown>;
  /** Pre-select a type for the create wizard and hide the picker; unlike `initialType`, never
   * triggers edit-mode secret semantics. */
  presetType?: string;
  submitLabel?: string;
  onSubmit: (values: DataSourceFormValues) => void | Promise<void>;
  isSubmitting?: boolean;
  renderExtraActions?: (values: DataSourceFormValues) => React.ReactNode;
}

function defaultOptionsFor(schema: DataSourceConfigurationSchema | undefined): Record<string, unknown> {
  return Object.fromEntries(
    Object.entries(schema?.properties ?? {}).flatMap(([key, field]) =>
      field.default === undefined ? [] : [[key, field.default]],
    ),
  );
}

export function DataSourceForm({
  types,
  initialName,
  initialType,
  initialOptions,
  presetType,
  submitLabel = "Save",
  onSubmit,
  isSubmitting,
  renderExtraActions,
}: DataSourceFormProps) {
  const [name, setName] = useState(initialName ?? "");
  const knownTypes = types.filter((t) => t.name !== "Unknown").sort((a, b) => a.name.localeCompare(b.name));
  // No fallback to `knownTypes[0]`: create mode starts with no type, so connection fields stay
  // hidden until one is picked.
  const [type, setType] = useState(initialType ?? presetType ?? "");
  const [options, setOptions] = useState<Record<string, unknown>>(
    initialOptions ?? defaultOptionsFor(types.find((t) => t.type === (initialType ?? presetType))?.configuration_schema),
  );
  const [errors, setErrors] = useState<Record<string, string>>({});
  const formRef = useRef<HTMLFormElement>(null);
  const values: DataSourceFormValues = { name, type, options };

  const selectedType = types.find((t) => t.type === type);
  const schema = selectedType?.configuration_schema;
  const secretFields = new Set(schema?.secret ?? []);
  const requiredFields = new Set(schema?.required ?? []);

  useEffect(() => {
    if (!initialType) setOptions(defaultOptionsFor(schema));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [type]);

  /** A blank secret on an edit form means "keep the current value"; the backend never echoes
   * secrets back. */
  function isKeepCurrentSecret(key: string, value: unknown): boolean {
    return secretFields.has(key) && initialType != null && (value == null || value === "");
  }

  function validateOptionField(key: string, value: unknown): string | null {
    if (isKeepCurrentSecret(key, value)) return null;
    const field = schema?.properties?.[key];
    return validateValue(field?.title ?? key, requiredFields.has(key), fieldKind(key, field?.type), value);
  }

  function fieldElementId(key: string): string {
    if (key === "name") return "ds-name";
    if (key === "type") return "ds-type";
    return `ds-field-${key}`;
  }

  function runValidation(): Record<string, string> {
    const next: Record<string, string> = {};
    const nameErr = validateValue("Name", true, undefined, name);
    if (nameErr) next.name = nameErr;
    if (presetType == null && !type) next.type = "Type is required.";
    for (const key of Object.keys(schema?.properties ?? {})) {
      const err = validateOptionField(key, options[key]);
      if (err) next[key] = err;
    }
    return next;
  }

  function reValidateIfErroring(key: string, validate: () => string | null) {
    setErrors((prev) => {
      if (!(key in prev)) return prev;
      const next = { ...prev };
      const message = validate();
      if (message) next[key] = message;
      else delete next[key];
      return next;
    });
  }

  function handleNameChange(value: string) {
    setName(value);
    reValidateIfErroring("name", () => validateValue("Name", true, undefined, value));
  }

  function handleTypeChange(value: string) {
    setType(value);
    reValidateIfErroring("type", () => (value ? null : "Type is required."));
  }

  function handleOptionChange(key: string, value: unknown) {
    setOptions((prev) => ({ ...prev, [key]: value }));
    reValidateIfErroring(key, () => validateOptionField(key, value));
  }

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    const next = runValidation();
    setErrors(next);
    const firstInvalidKey = Object.keys(next)[0];
    if (firstInvalidKey) {
      formRef.current
        ?.querySelector<HTMLElement>(`#${CSS.escape(fieldElementId(firstInvalidKey))}`)
        ?.focus();
      return;
    }
    onSubmit(values);
  }

  return (
    <form ref={formRef} className="space-y-6" onSubmit={handleSubmit} noValidate>
      <div className="space-y-2">
        <Label htmlFor="ds-name">
          Name
          <RequiredMark />
        </Label>
        <Input
          id="ds-name"
          value={name}
          onChange={(e) => handleNameChange(e.target.value)}
          placeholder={selectedType ? `My ${selectedType.name} database` : "My database"}
          aria-invalid={errors.name ? true : undefined}
          aria-describedby={errors.name ? "ds-name-error" : undefined}
        />
        <FieldError id="ds-name-error" message={errors.name} />
      </div>

      {presetType == null && (
        <div className="space-y-2">
          <Label htmlFor="ds-type">
            Type
            <RequiredMark />
          </Label>
          <Select
            value={type}
            onValueChange={handleTypeChange}
            disabled={initialType != null}
          >
            <SelectTrigger
              id="ds-type"
              className="w-full"
              aria-invalid={errors.type ? true : undefined}
              aria-describedby={errors.type ? "ds-type-error" : undefined}
            >
              <SelectValue placeholder="Select a data source type" />
            </SelectTrigger>
            <SelectContent>
              {knownTypes.map((t) => (
                <SelectItem key={t.type} value={t.type}>
                  {t.name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <FieldError id="ds-type-error" message={errors.type} />
        </div>
      )}

      {schema?.properties && (
        <div className="space-y-4 rounded-md border border-border p-4">
          <p className="text-sm font-medium text-muted-foreground">Connection settings</p>
          {Object.entries(schema.properties).map(([key, field]) => {
            const isSecret = secretFields.has(key);
            const isNumeric = field.type === "integer" || field.type === "number";
            const fieldId = fieldElementId(key);
            const errorId = `${fieldId}-error`;
            const value = options[key];
            const fieldError = errors[key];

            return (
              <div key={key} className="space-y-2">
                <Label htmlFor={fieldId}>
                  {field.title ?? key}
                  {requiredFields.has(key) && <RequiredMark />}
                </Label>
                {field.type === "file" ? (
                  <FileUploadField
                    connectorType={type}
                    fieldTitle={field.title}
                    accept={field.accept}
                    delimiter={options.delimiter != null ? String(options.delimiter) : undefined}
                    currentFilename={initialOptions?.upload_filename as string | undefined}
                    onUploaded={(result) => handleOptionChange(key, result.upload_id)}
                  />
                ) : field.enum ? (
                  <Select
                    value={value != null ? String(value) : ""}
                    onValueChange={(v) => handleOptionChange(key, v)}
                  >
                    <SelectTrigger id={fieldId} className="w-full">
                      <SelectValue placeholder={`Select ${field.title ?? key}`} />
                    </SelectTrigger>
                    <SelectContent>
                      {field.enum.map((opt) => (
                        <SelectItem key={String(opt)} value={String(opt)}>
                          {String(opt)}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                ) : (
                  <>
                    <Input
                      id={fieldId}
                      type={isSecret ? "password" : isNumeric ? "number" : "text"}
                      autoComplete={isSecret ? "new-password" : "off"}
                      value={value != null ? String(value) : ""}
                      placeholder={isSecret && initialType != null ? "Leave blank to keep current" : undefined}
                      aria-invalid={fieldError ? true : undefined}
                      aria-describedby={fieldError ? errorId : undefined}
                      onChange={(e) => {
                        const raw = e.target.value;
                        handleOptionChange(key, isNumeric ? (raw === "" ? undefined : Number(raw)) : raw);
                      }}
                    />
                    {isSecret && initialType != null && (
                      <p className="text-xs text-muted-foreground">Leave empty to keep the current value.</p>
                    )}
                  </>
                )}
                <FieldError id={errorId} message={fieldError} />
                {field.description && <p className="text-xs text-muted-foreground">{field.description}</p>}
              </div>
            );
          })}
        </div>
      )}

      <div className="flex items-center gap-3">
        <Button type="submit" disabled={isSubmitting}>
          {isSubmitting ? "Saving…" : submitLabel}
        </Button>
        {renderExtraActions?.(values)}
      </div>
    </form>
  );
}
