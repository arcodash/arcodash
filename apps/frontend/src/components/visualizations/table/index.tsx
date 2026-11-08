import { useEffect, useMemo, useState } from "react";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import type { Column, Row, VizEditorProps, VizRendererProps } from "../registry";
import {
  ResultsTable,
  DEFAULT_ITEMS_PER_PAGE,
  type ResultsTableColumnOverride as ColOverride,
} from "@/components/query-editor/ResultsTable";
import type { ResultData } from "@/types/api";

const NO_OVERRIDES: ColOverride[] = [];

/** Back to the shared ResultsTable's {name, friendly_type} column shape. */
export function toResultData(columns: Column[], rows: Row[]): ResultData {
  return { columns: columns.map((c) => ({ name: c.name, friendly_type: c.type })), rows };
}

function readOptions(options: Record<string, unknown>) {
  return {
    overrides: (options.columns as ColOverride[] | undefined) ?? NO_OVERRIDES,
    itemsPerPage: (options.itemsPerPage as number | undefined) ?? DEFAULT_ITEMS_PER_PAGE,
  };
}

export function TableRenderer({ data, options }: VizRendererProps) {
  const { overrides, itemsPerPage } = readOptions(options);

  // Fold the viz's own friendly_name (data-source default) and column overrides (explicit
  // rename/visibility) into one override list so ResultsTable only needs its own logic.
  const resultData = useMemo(() => toResultData(data.columns, data.rows), [data.columns, data.rows]);
  const resolvedOverrides: ColOverride[] = useMemo(
    () =>
      data.columns.map((c) => {
        const ov = overrides.find((o) => o.name === c.name);
        return { name: c.name, visible: ov?.visible, friendly_name: ov?.friendly_name ?? c.friendly_name };
      }),
    [data.columns, overrides],
  );

  return (
    <ResultsTable
      data={resultData}
      columnOverrides={resolvedOverrides}
      itemsPerPage={itemsPerPage}
      showTypeBadges={false}
    />
  );
}

export function TableEditor({ data, options, onChange }: VizEditorProps) {
  const { overrides, itemsPerPage } = readOptions(options);
  const [itemsPerPageDraft, setItemsPerPageDraft] = useState(String(itemsPerPage));

  useEffect(() => {
    setItemsPerPageDraft(String(itemsPerPage));
  }, [itemsPerPage]);

  function getOverride(name: string): ColOverride {
    return overrides.find((o) => o.name === name) ?? { name, visible: true };
  }

  function setOverride(name: string, patch: Partial<ColOverride>) {
    onChange({
      ...options,
      columns: [...overrides.filter((o) => o.name !== name), { ...getOverride(name), ...patch }],
    });
  }

  return (
    <div className="space-y-4">
      <div className="space-y-1">
        <Label>Rows per page</Label>
        <Input
          type="number"
          value={itemsPerPageDraft}
          min={1}
          max={1000}
          className="w-24"
          onChange={(e) => {
            const raw = e.target.value;
            setItemsPerPageDraft(raw);
            if (raw === "") return;
            const clamped = Math.min(1000, Math.max(1, Number(raw) || DEFAULT_ITEMS_PER_PAGE));
            if (clamped !== itemsPerPage) {
              onChange({ ...options, itemsPerPage: clamped });
            }
          }}
          onBlur={() => {
            if (itemsPerPageDraft !== "") return;
            setItemsPerPageDraft(String(DEFAULT_ITEMS_PER_PAGE));
            if (itemsPerPage !== DEFAULT_ITEMS_PER_PAGE) {
              onChange({ ...options, itemsPerPage: DEFAULT_ITEMS_PER_PAGE });
            }
          }}
        />
      </div>
      <div className="space-y-2">
        <Label>Columns</Label>
        {data.columns.map((col) => {
          const ov = getOverride(col.name);
          return (
            <div key={col.name} className="flex items-center gap-3">
              <input
                type="checkbox"
                checked={ov.visible !== false}
                onChange={(e) => setOverride(col.name, { visible: e.target.checked })}
                className="size-4"
              />
              <span className="w-32 text-sm text-muted-foreground truncate">{col.name}</span>
              <Input
                className="h-7 text-sm"
                placeholder={col.friendly_name ?? col.name}
                value={ov.friendly_name ?? ""}
                onChange={(e) =>
                  setOverride(col.name, { friendly_name: e.target.value || undefined })
                }
              />
            </div>
          );
        })}
      </div>
    </div>
  );
}

export const tableDefaultOptions: Record<string, unknown> = {
  itemsPerPage: DEFAULT_ITEMS_PER_PAGE,
  columns: [],
};
