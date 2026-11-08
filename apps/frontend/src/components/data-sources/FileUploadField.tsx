import { useRef, useState } from "react";
import { CheckCircle2, UploadCloud } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { cn } from "@/lib/utils";
import { ApiError, api } from "@/lib/api";
import type { UploadDataSourceResponse } from "@/types/api";

const MAX_UPLOAD_BYTES = 25 * 1024 * 1024;

function humanizeBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  return `${value.toFixed(1)} ${units[unit]}`;
}

interface FileUploadFieldProps {
  /** Connector type ("csv" | "sqlite") — the `type` field of the multipart upload. */
  connectorType: string;
  /** Schema field title (e.g. "CSV file", "SQLite database"); drives the drop-zone copy. */
  fieldTitle?: string;
  /** Schema field `accept` (e.g. ".csv"); shown in the sub-line and passed to the file input. */
  accept?: string;
  /** CSV delimiter from the sibling form field, sent alongside the upload. */
  delimiter?: string;
  /** Existing upload's filename, from `initialOptions.upload_filename` (edit mode only). */
  currentFilename?: string;
  onUploaded: (result: UploadDataSourceResponse) => void;
}

type View =
  | { kind: "current" }
  | { kind: "zone" }
  | { kind: "uploading"; filename: string; progress: number }
  | { kind: "success"; result: UploadDataSourceResponse };

export function FileUploadField({
  connectorType,
  fieldTitle,
  accept,
  delimiter,
  currentFilename,
  onUploaded,
}: FileUploadFieldProps) {
  const [view, setView] = useState<View>(currentFilename ? { kind: "current" } : { kind: "zone" });
  const [error, setError] = useState<string | null>(null);
  const [dragOver, setDragOver] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  async function handleFile(file: File) {
    setError(null);
    if (file.size > MAX_UPLOAD_BYTES) {
      setError(`"${file.name}" is too large — the limit is 25 MB.`);
      return;
    }
    setView({ kind: "uploading", filename: file.name, progress: 0 });
    try {
      const result = await api.uploadDataSourceFile(
        { type: connectorType, file, delimiter },
        (fraction) =>
          setView((prev) => (prev.kind === "uploading" ? { ...prev, progress: fraction } : prev)),
      );
      setView({ kind: "success", result });
      onUploaded(result);
    } catch (err) {
      setError(err instanceof ApiError ? err.message : "Upload failed");
      setView({ kind: "zone" });
    }
  }

  function openPicker() {
    inputRef.current?.click();
  }

  function replace() {
    setError(null);
    setView({ kind: "zone" });
  }

  return (
    <div className="space-y-3">
      {view.kind === "current" && (
        <div className="flex items-center justify-between rounded-md border border-border p-3 text-sm">
          <span>
            Current: <span className="font-medium">{currentFilename}</span>
          </span>
          <Button type="button" variant="outline" size="sm" onClick={replace}>
            Replace
          </Button>
        </div>
      )}

      {view.kind === "uploading" && (
        <div className="space-y-2 rounded-md border border-border p-3">
          <p className="text-sm">{view.filename}</p>
          <div className="h-2 w-full overflow-hidden rounded-full bg-secondary">
            <div
              className="h-full bg-primary transition-all"
              style={{ width: `${Math.round(view.progress * 100)}%` }}
            />
          </div>
        </div>
      )}

      {view.kind === "success" && (
        <div className="space-y-3">
          <div className="flex items-center justify-between rounded-md border border-border p-3 text-sm">
            <div className="flex items-center gap-2">
              <CheckCircle2 className="h-4 w-4 text-success" aria-hidden="true" />
              <span className="font-medium">{view.result.filename}</span>
              <span className="text-muted-foreground">
                {humanizeBytes(view.result.byte_size)} · {view.result.columns.length} columns
              </span>
            </div>
            <Button type="button" variant="outline" size="sm" onClick={replace}>
              Replace
            </Button>
          </div>
          {view.result.warning && <p className="text-xs text-destructive">{view.result.warning}</p>}
          {view.result.columns.length > 0 && (
            <Table>
              <TableHeader>
                <TableRow>
                  {view.result.columns.map((c) => (
                    <TableHead key={c.name}>{c.name}</TableHead>
                  ))}
                </TableRow>
              </TableHeader>
              <TableBody>
                {view.result.preview_rows.map((row, i) => (
                  <TableRow key={i}>
                    {view.result.columns.map((c) => (
                      <TableCell key={c.name}>{String(row[c.name] ?? "")}</TableCell>
                    ))}
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </div>
      )}

      {view.kind === "zone" && (
        <div
          data-testid="file-upload-dropzone"
          role="button"
          tabIndex={0}
          className={cn(
            "flex cursor-pointer flex-col items-center gap-3 rounded-md border border-dashed p-6 text-center transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
            dragOver ? "border-foreground bg-accent" : "border-border",
          )}
          onClick={openPicker}
          onKeyDown={(e) => {
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              openPicker();
            }
          }}
          onDragOver={(e) => {
            e.preventDefault();
            setDragOver(true);
          }}
          onDragLeave={() => setDragOver(false)}
          onDrop={(e) => {
            e.preventDefault();
            setDragOver(false);
            const file = e.dataTransfer.files?.[0];
            if (file) void handleFile(file);
          }}
        >
          <span className="flex h-10 w-10 items-center justify-center rounded-full bg-secondary">
            <UploadCloud className="h-5 w-5 text-muted-foreground" aria-hidden="true" />
          </span>
          <p className="text-sm">
            Drag &amp; drop your {fieldTitle ?? "file"} here, or <span className="underline">browse</span>
          </p>
          <p className="text-xs text-muted-foreground">
            {accept ?? "any file"} up to 25 MB — validated and previewed before you save
          </p>
          <input
            ref={inputRef}
            data-testid="file-upload-input"
            type="file"
            accept={accept}
            className="hidden"
            onChange={(e) => {
              const file = e.target.files?.[0];
              e.target.value = "";
              if (file) void handleFile(file);
            }}
          />
        </div>
      )}

      {error && <p className="text-sm text-destructive">{error}</p>}
    </div>
  );
}
