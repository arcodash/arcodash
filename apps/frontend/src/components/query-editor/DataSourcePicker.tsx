import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import type { DataSource } from "@/types/api";

interface DataSourcePickerProps {
  dataSources: DataSource[];
  value: number | null;
  onChange: (id: number) => void;
}

export function DataSourcePicker({ dataSources, value, onChange }: DataSourcePickerProps) {
  return (
    <Select value={value == null ? "" : String(value)} onValueChange={(v) => onChange(Number(v))}>
      <SelectTrigger className="w-full max-w-64 min-w-40" data-testid="query-editor-data-source-select">
        <SelectValue placeholder="Select a data source" />
      </SelectTrigger>
      <SelectContent>
        {dataSources.map((ds) => (
          <SelectItem key={ds.id} value={String(ds.id)}>
            {ds.name} ({ds.type_display_name})
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
