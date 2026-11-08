import { cn } from "@/lib/utils";
import type { ResizableColumn } from "@/hooks/useResizableColumn";

/** Drag handle for the right edge of a `relative` table header. */
export function ColumnResizeHandle({ column }: { column: ResizableColumn }) {
  return (
    <div
      {...column.handleProps}
      className="group absolute right-0 top-0 flex h-full w-2 translate-x-1/2 cursor-col-resize touch-none select-none justify-center outline-none"
    >
      <div
        className={cn(
          "h-full w-px bg-border group-hover:bg-primary group-focus-visible:bg-primary",
          column.dragging && "bg-primary",
        )}
      />
    </div>
  );
}
