import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";

/** Column picker shared by the viz editors. Radix Select.Item forbids value="", so `none` adds a
 * "none" sentinel item that the caller maps back to an unset option. */
export function ColumnSelect({
  value,
  onChange,
  columns,
  none = false,
}: {
  value: string;
  onChange: (value: string) => void;
  columns: string[];
  none?: boolean;
}) {
  return (
    <Select value={value} onValueChange={onChange}>
      <SelectTrigger className="w-48">
        <SelectValue placeholder={none ? "None" : "Select column"} />
      </SelectTrigger>
      <SelectContent>
        {none && <SelectItem value="none">None</SelectItem>}
        {columns.map((c) => (
          <SelectItem key={c} value={c}>
            {c}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
