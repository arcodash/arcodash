import { useState } from "react";
import { Download } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { api, apiErrorMessage } from "@/lib/api";

const FORMAT_LABELS = { csv: "Download CSV", tsv: "Download TSV", xlsx: "Download Excel" };

export function ExportMenu({ resultId }: { resultId: number | null }) {
  const [open, setOpen] = useState(false);
  const [downloading, setDownloading] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function download(ext: string) {
    if (resultId == null) return;
    setOpen(false);
    setError(null);
    setDownloading(ext);
    try {
      const blob = await api.downloadQueryResult(resultId, ext);
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `query-result-${resultId}.${ext}`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (err) {
      setError(apiErrorMessage(err, "Couldn't export this result"));
    } finally {
      setDownloading(null);
    }
  }

  return (
    <div className="flex flex-col items-end gap-1">
      <Popover open={open} onOpenChange={setOpen}>
        <PopoverTrigger asChild>
          <Button variant="outline" size="sm" disabled={resultId == null}>
            <Download className="size-3.5" />
            Export
          </Button>
        </PopoverTrigger>
        <PopoverContent className="w-44 p-1" align="end">
          <div className="flex flex-col">
            {Object.entries(FORMAT_LABELS).map(([ext, label]) => (
              <button
                key={ext}
                onClick={() => download(ext)}
                disabled={downloading != null}
                className="cursor-pointer rounded px-3 py-1.5 text-left text-sm hover:bg-accent disabled:cursor-not-allowed disabled:opacity-50"
              >
                {label}
              </button>
            ))}
          </div>
        </PopoverContent>
      </Popover>
      {error && <p className="text-xs text-destructive">{error}</p>}
    </div>
  );
}
