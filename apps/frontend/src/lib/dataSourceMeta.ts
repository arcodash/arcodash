/** Icon + category metadata for the New Data Source type picker. Presentational; the backend `display_name` lookup in
 * `apps/backend/src/api/handlers/data_sources.rs` decides which types surface. */
import {
  Activity,
  Boxes,
  CircuitBoard,
  Cloud,
  Database,
  FileText,
  Gauge,
  HardDrive,
  Layers,
  Search,
  Leaf,
  Server,
  Snowflake,
  type LucideIcon,
} from "lucide-react";

export const CATEGORY_ORDER = ["Databases", "Files", "Meta-source", "Other"] as const;

export const CATEGORY_ICONS: Record<(typeof CATEGORY_ORDER)[number], LucideIcon> = {
  Databases: Database,
  Files: FileText,
  "Meta-source": Layers,
  Other: Boxes,
};

// Types with a badge at public/data-sources/{type}.{ext}; PNG = hand-designed, SVG = brand logo
// (Simple Icons, CC0-1.0).
const LOGO_TYPES: Record<string, "png" | "svg"> = {
  pg: "png",
  mysql: "png",
  mariadb: "png",
  tidb: "png",
  clickhouse: "png",
  mongodb: "png",
  es: "png",
  sqlite: "png",
  redshift: "png",
  cockroachdb: "png",
  query_results: "png",
  csv: "png",
  json: "png",
  bigquery: "svg",
  snowflake: "svg",
  databricks: "svg",
  prometheus: "svg",
  influxdb: "svg",
  athena: "png",
};

const META: Record<string, { icon: LucideIcon; category: string; color?: string }> = {
  pg: { icon: Database, category: "Databases" },
  redshift: { icon: Database, category: "Databases" },
  cockroachdb: { icon: Database, category: "Databases" },
  mysql: { icon: Database, category: "Databases" },
  mariadb: { icon: Database, category: "Databases" },
  tidb: { icon: Database, category: "Databases" },
  clickhouse: { icon: Database, category: "Databases" },
  es: { icon: Search, category: "Databases" },
  mongodb: { icon: Leaf, category: "Databases" },
  query_results: { icon: Layers, category: "Meta-source" },
  sqlite: { icon: HardDrive, category: "Files" },
  csv: { icon: FileText, category: "Files" },
  json: { icon: FileText, category: "Files" },
  // Simple Icons has no official Microsoft mark; keep the Lucide fallback with an accent color.
  mssql: { icon: Server, category: "Databases", color: "#CC2927" },
  bigquery: { icon: Cloud, category: "Databases" },
  snowflake: { icon: Snowflake, category: "Databases" },
  athena: { icon: Cloud, category: "Databases" },
  databricks: { icon: CircuitBoard, category: "Databases" },
  prometheus: { icon: Gauge, category: "Databases" },
  influxdb: { icon: Activity, category: "Databases" },
};

const FALLBACK = { icon: Database, category: "Other" };

/** Most-used-first order for `Databases`; other categories sort alphabetically. Unlisted types sort
 * after listed ones. */
export const POPULARITY_ORDER = [
  "pg",
  "mysql",
  "bigquery",
  "snowflake",
  "redshift",
  "clickhouse",
  "mssql",
  "mongodb",
  "databricks",
  "athena",
] as const;

function popularityRank(type: string): number {
  const i = POPULARITY_ORDER.indexOf(type as (typeof POPULARITY_ORDER)[number]);
  return i === -1 ? Number.POSITIVE_INFINITY : i;
}

export function compareDataSourceTypes(
  a: { type: string; name: string },
  b: { type: string; name: string },
): number {
  return popularityRank(a.type) - popularityRank(b.type) || a.name.localeCompare(b.name);
}

export function getDataSourceMeta(
  type: string,
): { icon: LucideIcon; category: string; color?: string; logoUrl?: string } {
  const meta = META[type] ?? FALLBACK;
  const ext = LOGO_TYPES[type];
  const logoUrl = ext ? `/data-sources/${type}.${ext}` : undefined;
  return logoUrl ? { ...meta, logoUrl } : meta;
}
