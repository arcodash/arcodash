import type React from "react";
import type { ResultData } from "@/types/api";
import { TableRenderer, TableEditor, tableDefaultOptions } from "./table";
import { ChartRenderer, ChartEditor, chartDefaultOptions } from "./chart";
import { CounterRenderer, CounterEditor, counterDefaultOptions } from "./counter";
import { FunnelRenderer, FunnelEditor, funnelDefaultOptions } from "./funnel";

export interface Column {
  name: string;
  friendly_name: string;
  type: string;
}

export type Row = Record<string, unknown>;

export interface QueryResultData {
  columns: Column[];
  rows: Row[];
}

export function toVizData(data: ResultData | null | undefined): QueryResultData {
  if (!data) return { columns: [], rows: [] };
  return {
    columns: data.columns.map((c) => ({ name: c.name, friendly_name: c.name, type: c.friendly_type ?? "string" })),
    rows: data.rows,
  };
}

export interface VizRendererProps {
  data: QueryResultData;
  options: Record<string, unknown>;
  /** Set only by a host with no card header of its own (public embeds) — most renderers
   * ignore it, since the surrounding tab/card already names the visualization. */
  title?: string;
}

export interface VizEditorProps {
  data: QueryResultData;
  options: Record<string, unknown>;
  onChange: (opts: Record<string, unknown>) => void;
}

export interface VizDefinition {
  type: string;
  name: string;
  Renderer: React.ComponentType<VizRendererProps>;
  Editor: React.ComponentType<VizEditorProps>;
  defaultOptions: Record<string, unknown>;
}

export const vizRegistry: VizDefinition[] = [];

export function registerViz(def: VizDefinition): void {
  vizRegistry.push(def);
}

export function getViz(type: string): VizDefinition | undefined {
  return vizRegistry.find((v) => v.type.toLowerCase() === type.toLowerCase());
}

[
  { type: "TABLE", name: "Table", Renderer: TableRenderer, Editor: TableEditor, defaultOptions: tableDefaultOptions },
  { type: "CHART", name: "Chart", Renderer: ChartRenderer, Editor: ChartEditor, defaultOptions: chartDefaultOptions },
  { type: "COUNTER", name: "Counter", Renderer: CounterRenderer, Editor: CounterEditor, defaultOptions: counterDefaultOptions },
  { type: "FUNNEL", name: "Funnel", Renderer: FunnelRenderer, Editor: FunnelEditor, defaultOptions: funnelDefaultOptions },
].forEach(registerViz);
