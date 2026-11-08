import { useCallback, useLayoutEffect, useRef, useState } from "react";
import type { KeyboardEvent, MouseEvent, PointerEvent } from "react";
import { readStorage } from "@/lib/utils";

export const MIN_COLUMN_WIDTH = 160;
const KEY_STEP = 16;

function readStored(key: string): number | null {
  const n = Number(readStorage(key));
  return Number.isFinite(n) && n > 0 ? Math.max(MIN_COLUMN_WIDTH, n) : null;
}

function writeStored(key: string, width: number | null) {
  try {
    if (width == null) window.localStorage.removeItem(key);
    else window.localStorage.setItem(key, String(width));
  } catch {
    // storage unavailable: the width just won't persist
  }
}

/** A user-resizable column width, persisted under `storageKey`. `null` means "never resized". */
export function useResizableColumn(storageKey: string) {
  const [width, setWidth] = useState<number | null>(() => readStored(storageKey));
  const headRef = useRef<HTMLTableCellElement>(null);
  const drag = useRef<{ startX: number; startWidth: number; latest: number } | null>(null);
  const [dragging, setDragging] = useState(false);
  const [measuredWidth, setMeasuredWidth] = useState<number | undefined>();

  useLayoutEffect(() => {
    if (width == null && headRef.current) setMeasuredWidth(Math.round(headRef.current.getBoundingClientRect().width));
  }, [width]);

  const commit = useCallback(
    (next: number | null) => {
      const clamped = next == null ? null : Math.max(MIN_COLUMN_WIDTH, Math.round(next));
      setWidth(clamped);
      writeStored(storageKey, clamped);
    },
    [storageKey],
  );

  const measured = () => headRef.current?.getBoundingClientRect().width ?? MIN_COLUMN_WIDTH;

  const endDrag = (e: PointerEvent<HTMLElement>) => {
    e.stopPropagation();
    const d = drag.current;
    drag.current = null;
    setDragging(false);
    if (d) commit(d.latest);
  };

  const handleProps = {
    tabIndex: 0,
    role: "separator" as const,
    "aria-orientation": "vertical" as const,
    "aria-valuenow": width ?? measuredWidth,
    "aria-valuemin": MIN_COLUMN_WIDTH,
    "aria-label": "Resize Name column",
    onPointerDown: (e: PointerEvent<HTMLElement>) => {
      e.stopPropagation();
      e.currentTarget.setPointerCapture(e.pointerId);
      const startWidth = width ?? measured();
      drag.current = { startX: e.clientX, startWidth, latest: startWidth };
      setDragging(true);
    },
    onPointerMove: (e: PointerEvent<HTMLElement>) => {
      const d = drag.current;
      if (!d) return;
      d.latest = Math.max(MIN_COLUMN_WIDTH, d.startWidth + e.clientX - d.startX);
      setWidth(d.latest);
    },
    onPointerUp: endDrag,
    onPointerCancel: endDrag,
    onLostPointerCapture: endDrag,
    onClick: (e: MouseEvent) => e.stopPropagation(),
    onDoubleClick: (e: MouseEvent) => {
      e.stopPropagation();
      commit(null);
    },
    onKeyDown: (e: KeyboardEvent<HTMLElement>) => {
      if (e.key === "Home") commit(null);
      else if (e.key === "ArrowLeft") commit((width ?? measured()) - KEY_STEP);
      else if (e.key === "ArrowRight") commit((width ?? measured()) + KEY_STEP);
      else return;
      e.preventDefault();
      e.stopPropagation();
    },
  };

  return { width, headRef, dragging, handleProps };
}

export type ResizableColumn = ReturnType<typeof useResizableColumn>;
