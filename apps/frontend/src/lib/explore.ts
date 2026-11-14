export interface ExploreState {
  explore: { queryId: number; queryName: string };
  dataSourceId: number;
  sql: string;
}

/** Starter SQL over a query's cached result; the name is kept on one comment line. */
export function exploreSql(queryId: number, queryName: string): string {
  const safeName = queryName.replace(/[\r\n]+/g, " ").replace(/"/g, '\\"');
  return [
    `-- Explore of "${safeName}" (#${queryId}).`,
    `-- query_${queryId} is its cached result. This runs inside arcodash, not in your database.`,
    "SELECT *",
    `FROM query_${queryId}`,
    "LIMIT 100",
  ].join("\n");
}

export function exploreState(queryId: number, queryName: string, dataSourceId: number): ExploreState {
  return { explore: { queryId, queryName }, dataSourceId, sql: exploreSql(queryId, queryName) };
}
